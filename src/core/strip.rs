//! The engine for stripping provably-safe revert guards.
//!
//! Port of `strip` / `is_revert_jumpi` from Python with one addition: removal is
//! gated by the enabled set (`Category`), so the `guards` feature can be turned off.
//!
//! What is ALWAYS preserved (even when the feature is enabled):
//!   * authorization — any `run` with `CALLER`/`ORIGIN` (`msg.sender == owner`);
//!   * side effects — `SSTORE`/`CALL`/`MSTORE`/`LOG*`/... and terminals;
//!   * checks that consume their own input (not a stack identity).

use std::collections::HashSet;

use super::asm::{Instr, Kind};
use super::stack::strip_residue;

/// Maximum length of the suffix analyzed before a revert JUMPI.
const WINDOW: i64 = 48;

/// The class of a gas-reducing rewrite a feature owns. `Guard` is the trusted-caller
/// revert-guard removal (the former `abi`/`math`/`assert` split was a fragile
/// opcode-sniffing label — the same calldata bounds check landed in different classes
/// across compiler codegen — so it was merged into one honest feature). `Shuffle` is
/// the always-safe stack-reschedule pass ([`crate::features::shuffle`]); `Involution`
/// is the always-safe cancelling of involutive op pairs ([`crate::features::involution`]);
/// `Recompute` is the always-safe rewrite of a `DUP1` of a cheap result-invariant nullary
/// opcode into a second copy of that opcode ([`crate::features::recompute`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    /// Any provably-safe revert guard.
    Guard,
    /// A maximal `DUP`/`SWAP`/`POP` window rewritten to a cheaper equivalent.
    Shuffle,
    /// A run of an involutive op (`NOT`) collapsed to its net effect.
    Involution,
    /// A `DUP1` of a cheap result-invariant nullary opcode recomputed as that opcode.
    Recompute,
    /// A constant `PUSH a PUSH b SHL/SHR` precomputed to a single push of the result.
    FoldShift,
    /// A `SWAP1` before a comparison folded into the mirrored comparator (`SWAP1 LT` -> `GT`).
    CmpNorm,
}

impl Category {
    /// A stable key for the CLI/config.
    #[inline]
    pub fn key(self) -> &'static str {
        match self {
            Category::Guard => "guards",
            Category::Shuffle => "shuffle",
            Category::Involution => "involution",
            Category::Recompute => "recompute",
            Category::FoldShift => "foldshift",
            Category::CmpNorm => "cmpnorm",
        }
    }
}

/// A stripped instruction range `[start, end]`, its category, and the stack-shuffle
/// that REPLACES it. `replacement` is empty for a pure identity (delete entirely) or
/// a few `POP`/`SWAP` ops for a consuming check (reproduce its live-stack residue
/// without the revert) — see [`crate::core::stack::strip_residue`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub category: Category,
    pub replacement: Vec<String>,
}

// Opcodes that protect a check from removal.
fn is_auth(m: &str) -> bool {
    matches!(m, "CALLER" | "ORIGIN")
}

fn is_side(m: &str) -> bool {
    matches!(
        m,
        "SSTORE" | "TSTORE" | "MSTORE" | "MSTORE8" | "LOG0" | "LOG1" | "LOG2" | "LOG3" | "LOG4"
            | "CALL" | "CALLCODE" | "DELEGATECALL" | "STATICCALL" | "CREATE" | "CREATE2"
            | "SELFDESTRUCT" | "RETURN" | "REVERT" | "STOP" | "INVALID"
            | "CALLDATACOPY" | "CODECOPY" | "RETURNDATACOPY" | "EXTCODECOPY" | "MCOPY"
    )
}

fn is_ctrl(m: &str) -> bool {
    matches!(m, "JUMP" | "JUMPI")
}

/// Opcode that halts execution: code before it cannot fall through to what follows.
fn is_terminal(m: &str) -> bool {
    matches!(m, "RETURN" | "REVERT" | "STOP" | "INVALID" | "SELFDESTRUCT")
}

/// `instr[i]` is a push of a revert label, followed by `JUMPI` (a conditional revert).
fn is_revert_jumpi(instrs: &[Instr], i: usize) -> bool {
    let a = &instrs[i];
    let b = instrs.get(i + 1);
    a.kind == Kind::PushSym
        && a.mnem().to_lowercase().contains("revert")
        && matches!(b, Some(x) if x.kind == Kind::Op && x.mnem() == "JUMPI")
}

/// The straight-line block before `start` (back to the nearest label) is free of auth
/// and side-effect opcodes.
///
/// A residue strip DROPS stack values, so it must not drop a value derived from
/// authorization (`CALLER`/`ORIGIN` — would silently remove `msg.sender == owner`)
/// or from a side effect (e.g. a `CALL`'s success flag — would ignore a failed call).
/// We conservatively refuse such a strip when its block contains either. Pure-identity
/// strips drop nothing and are always safe, so they bypass this check.
///
/// The scan stops at a `Kind::Label` or a terminal opcode ([`is_terminal`]): both end
/// the straight-line region reaching `start`. Stopping at a terminal matters for whole
/// programs — a compiler's deploy preamble (ending in `RETURN`) precedes the runtime
/// body with no intervening `JUMPDEST`, and its side effects (`CODECOPY`/`RETURN`) run
/// at deploy time, never feeding the runtime call stack.
fn block_clean_for_residue(instrs: &[Instr], start: usize) -> bool {
    let mut i = start;
    while i > 0 {
        i -= 1;
        if instrs[i].kind == Kind::Label {
            break;
        }
        if instrs[i].kind == Kind::Op {
            let m = instrs[i].mnem();
            if is_terminal(m) {
                break;
            }
            if is_auth(m) || is_side(m) {
                return false;
            }
        }
    }
    true
}

/// Removes provably-safe revert guards of the categories present in `enabled`.
///
/// Returns `(rewritten_instructions, list_of_stripped_spans)`. For each
/// `<...> _sym_*revert* JUMPI` it grows the LONGEST barrier-free suffix that can be
/// cut by reproducing its live-stack residue (see [`crate::core::stack::strip_residue`]):
/// a pure identity is deleted, a consuming check is replaced by a small `POP`/`SWAP`
/// shuffle. Removal happens only if the span's category is enabled. Auth (`CALLER`/
/// `ORIGIN`), side effects, and non-terminal `JUMP(I)` are always preserved.
pub fn strip_guards(instrs: &[Instr], enabled: &HashSet<Category>) -> (Vec<Instr>, Vec<Span>) {
    let mut spans: Vec<Span> = Vec::new();
    let n = instrs.len();

    let mut j = 1usize;
    while j < n {
        if instrs[j].kind == Kind::Op
            && instrs[j].mnem() == "JUMPI"
            && is_revert_jumpi(instrs, j - 1)
        {
            // Grow the suffix; keep the LONGEST one that is safely removable.
            let lo = (j as i64 - WINDOW).max(-1); // lower bound, exclusive
            let mut best: Option<(usize, Vec<String>)> = None;
            let mut s = j as i64 - 1;
            while s > lo {
                let su = s as usize;
                let run = &instrs[su..=j];

                // Stop conditions: label / auth / side effect / non-terminal JUMP(I).
                let mut bad = false;
                for (k, ins) in run.iter().enumerate() {
                    if ins.kind == Kind::Label {
                        bad = true;
                        break;
                    }
                    if ins.kind == Kind::Op {
                        let mm = ins.mnem();
                        if is_side(mm) || is_auth(mm) || (is_ctrl(mm) && k != run.len() - 1) {
                            bad = true;
                            break;
                        }
                    }
                }
                if bad {
                    break;
                }
                if let Some(rep) = strip_residue(run) {
                    // Identity (empty shuffle) is always safe; a residue that drops
                    // values must not drop an auth-derived value.
                    if rep.is_empty() || block_clean_for_residue(instrs, su) {
                        best = Some((su, rep)); // smallest su survives -> the longest run
                    }
                }
                s -= 1;
            }

            if let Some((f, rep)) = best {
                if enabled.contains(&Category::Guard) {
                    spans.push(Span { start: f, end: j, category: Category::Guard, replacement: rep });
                }
            }
        }
        j += 1;
    }

    // Post-strip DCE: a revert block whose last reference was just removed is now
    // unreachable dead weight (the compiler relinks remaining jumps), so delete it.
    if !spans.is_empty() {
        let referenced = referenced_syms(&apply_spans(instrs, &spans));
        let mut dead = dead_revert_spans(instrs, &referenced);
        if !dead.is_empty() {
            spans.append(&mut dead);
            spans.sort_by_key(|s| s.start);
        }
    }

    (apply_spans(instrs, &spans), spans)
}

/// Symbols still pushed as a jump target (`PushSym`) or referenced via an offset
/// (`_OFST sym n`) in `instrs` — i.e. labels that remain reachable by a jump.
fn referenced_syms(instrs: &[Instr]) -> HashSet<String> {
    let mut out = HashSet::new();
    for ins in instrs {
        match ins.kind {
            Kind::PushSym => {
                out.insert(ins.mnem().to_string());
            }
            Kind::Ofst => {
                if let Some(sym) = ins.tokens.get(1) {
                    out.insert(sym.clone());
                }
            }
            _ => {}
        }
    }
    out
}

/// `instrs[i-1]` cannot fall through into `instrs[i]` (it halts or jumps away), so a
/// label at `i` is reachable only by a jump to it.
fn unreachable_by_fallthrough(instrs: &[Instr], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    let pred = &instrs[i - 1];
    pred.kind == Kind::Op && (is_terminal(pred.mnem()) || pred.mnem() == "JUMP")
}

/// Delete spans for `_sym_*revert*` blocks that no remaining jump targets and cannot be
/// reached by fall-through — orphaned by the guard strip above. The block runs from its
/// label to just before the next label (or end of program). Solidity's revert blocks are
/// labelled `_sym_tag_*` (no "revert" substring), so this is a no-op there; its inverse-
/// idiom inline reverts are already dropped during the strip by the sidecar.
fn dead_revert_spans(instrs: &[Instr], referenced: &HashSet<String>) -> Vec<Span> {
    let mut out = Vec::new();
    for i in 0..instrs.len() {
        if instrs[i].kind != Kind::Label || !instrs[i].mnem().to_lowercase().contains("revert") {
            continue;
        }
        if referenced.contains(instrs[i].mnem()) || !unreachable_by_fallthrough(instrs, i) {
            continue;
        }
        let end = instrs[i + 1..]
            .iter()
            .position(|x| x.kind == Kind::Label)
            .map_or(instrs.len() - 1, |off| i + off);
        out.push(Span { start: i, end, category: Category::Guard, replacement: Vec::new() });
    }
    out
}

/// Rewrite `instrs` by replacing each span `[start, end]` with its `replacement` ops.
/// Spans must be non-overlapping; they are applied in `start` order. A replacement
/// token may be a bare opcode (`POP`/`SWAP1`/…) or a folded push literal encoded as
/// `#<hex>` (see [`super::asm::replacement_instr`]).
pub(crate) fn apply_spans(instrs: &[Instr], spans: &[Span]) -> Vec<Instr> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut it = spans.iter().peekable();
    while i < instrs.len() {
        if let Some(sp) = it.peek() {
            if sp.start == i {
                for op in &sp.replacement {
                    out.push(super::asm::replacement_instr(op));
                }
                i = sp.end + 1;
                it.next();
                continue;
            }
        }
        out.push(instrs[i].clone());
        i += 1;
    }
    out
}

/// Convenience shortcut: the strip feature enabled.
#[allow(dead_code)] // used in tests; a useful public helper
pub fn all_categories() -> HashSet<Category> {
    [Category::Guard].into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{mnemonics, parse_str};

    const REV: &str = "_sym___revert";

    fn strip_all(src: &str) -> (Vec<String>, Vec<Span>) {
        let p = parse_str(src);
        let (out, spans) = strip_guards(&p, &all_categories());
        (mnemonics(&out), spans)
    }

    #[test]
    fn auth_check_preserved() {
        // CALLER (msg.sender == owner) — NEVER strip.
        let (flat, spans) = strip_all(&format!("CALLER PUSH20 0xABCD XOR {REV} JUMPI"));
        assert!(spans.is_empty(), "auth check (CALLER) was wrongly stripped");
        assert!(flat.contains(&"CALLER".to_string()));
    }

    #[test]
    fn origin_check_preserved() {
        let (_flat, spans) = strip_all(&format!("ORIGIN PUSH20 0x1234 EQ ISZERO {REV} JUMPI"));
        assert!(spans.is_empty(), "auth check (ORIGIN) was wrongly stripped");
    }

    #[test]
    fn side_effect_preserved() {
        let (flat, spans) = strip_all(&format!("STATICCALL ISZERO {REV} JUMPI"));
        assert!(spans.is_empty(), "check with STATICCALL was wrongly stripped");
        assert!(flat.contains(&"STATICCALL".to_string()));
    }

    #[test]
    fn normal_jumpi_untouched() {
        // A normal conditional jump (not to revert) is left alone.
        let (flat, spans) = strip_all("DUP1 _sym_loop JUMPI");
        assert!(spans.is_empty(), "a normal JUMPI (not revert) was wrongly touched");
        assert_eq!(flat, vec!["DUP1", "_sym_loop", "JUMPI"]);
    }

    #[test]
    fn residue_strip_not_blocked_by_preceding_terminal() {
        // A consuming overflow guard (residue SWAP1/POP) in the runtime block, with a
        // deploy preamble ending in RETURN before it. A terminal halts execution, so
        // code before it cannot leave a live value on this block's stack and must not
        // block the residue strip. Reproduces a real Vyper `a + b` program whose
        // overflow assertion was wrongly preserved on the full (deploy + runtime) asm.
        let src = format!(
            "PUSH1 0 PUSH1 0 PUSH1 0 CODECOPY PUSH1 0 PUSH1 0 RETURN \
             PUSH1 4 CALLDATALOAD DUP1 PUSH1 36 CALLDATALOAD ADD SWAP1 DUP2 LT {REV} JUMPI"
        );
        let p = parse_str(&src);
        let (_out, spans) = strip_guards(&p, &all_categories());
        assert_eq!(spans.len(), 1, "overflow guard after a deploy-header RETURN was not stripped");
        assert_eq!(spans[0].category, Category::Guard, "a stripped guard must carry the single Guard category");
    }

    #[test]
    fn orphaned_revert_block_is_eliminated() {
        // After a guard is stripped, the shared `_sym_*revert*` block it jumped to
        // loses its last reference and becomes unreachable (its predecessor RETURN is
        // terminal). Post-strip DCE must delete that dead block as well.
        let src = format!(
            "DUP1 CALLDATALOAD PUSH1 32 LT {REV} JUMPI PUSH1 0 PUSH1 0 RETURN \
             {REV} JUMPDEST PUSH0 DUP1 REVERT"
        );
        let (flat, spans) = strip_all(&src);
        assert_eq!(spans.len(), 2, "the orphaned revert block was not removed alongside its guard");
        assert!(!flat.contains(&"REVERT".to_string()), "the dead revert body survived DCE");
        assert!(
            !flat.iter().any(|m| m.contains("revert")),
            "the orphaned revert label survived DCE"
        );
    }

    #[test]
    fn referenced_revert_block_is_kept() {
        // A second, NON-stripped guard (auth) still jumps to the same revert label, so
        // the block is still reachable and must NOT be deleted.
        let src = format!(
            "DUP1 CALLDATALOAD PUSH1 32 LT {REV} JUMPI CALLER PUSH1 1 XOR {REV} JUMPI \
             PUSH1 0 PUSH1 0 RETURN {REV} JUMPDEST PUSH0 DUP1 REVERT"
        );
        let (flat, _spans) = strip_all(&src);
        assert!(
            flat.iter().any(|m| m.contains("revert")),
            "a revert block still targeted by a live auth guard was wrongly deleted"
        );
        assert!(flat.contains(&"REVERT".to_string()), "the live revert body was wrongly deleted");
    }

    #[test]
    fn empty_enabled_set_disables_removal() {
        // With the strip feature disabled (empty enabled set) nothing is removed.
        let p = parse_str(&format!("DUP1 PUSH1 1 ADD PUSH1 100 LT {REV} JUMPI"));
        let none: HashSet<Category> = HashSet::new();
        let (out, spans) = strip_guards(&p, &none);
        assert!(spans.is_empty(), "with the feature disabled the check must not be stripped");
        assert_eq!(out.len(), p.len());
    }
}
