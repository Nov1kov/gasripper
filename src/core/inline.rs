//! Discovery of small internal functions that can be inlined into their call sites.
//!
//! This module is the pure, language-agnostic ANALYSIS half of the `inline` feature: it
//! finds Vyper-`venom` internal functions whose body is provably safe to splice into every
//! call site, and reports a [`Plan`] per function. It performs no rewriting — building the
//! (optimized, renamed) replacement bodies and emitting [`crate::core::Span`]s lives in
//! [`crate::features::inline`], because that step composes the other passes over each body.
//!
//! # The venom calling convention this recognizes
//!
//! `venom` emits an internal function as a contiguous runtime region introduced by a label
//! whose symbol begins `_sym_internal` (e.g. `_sym_internal 0 _f(uint256)_runtime`). A call
//! site is the three-instruction sequence
//!
//! ```text
//!   <pushsym ret>           ; the return address (a continuation label after the call)
//!   <pushsym entry>         ; the function's entry symbol
//!   JUMP                    ; transfer to the function
//!   <ret> JUMPDEST          ; control returns here, the body JUMPs back to <ret>
//! ```
//!
//! The body threads `ret` through the stack and returns by `JUMP`ing to it. Inlining keeps
//! the `pushsym ret` and the body's return `JUMP` verbatim, dropping only the `pushsym entry`
//! and the call `JUMP`: the body then runs in place and still returns to the same
//! continuation. That makes the rewrite a pure relocation of the body — no stack renumbering,
//! so it is straightforward to prove equivalent (see [`crate::features::inline`]).
//!
//! # What is rejected (left for later iterations)
//!
//! A function is inlined only when every safety premise holds; otherwise it is skipped:
//!   * the body is self-contained — every internal jump target is a label defined inside the
//!     body, and no code outside the body jumps into one of those labels;
//!   * the body neither calls another internal function nor recurses (`pushsym` of any
//!     `_sym_internal` symbol, including its own entry);
//!   * the body uses no `_mem_`/`_OFST` operands (their symbols are not recoverable from the
//!     sidecar dump, so they cannot be safely renamed);
//!   * every reference to the entry symbol is a well-formed call site as above;
//!   * the body size is within the configured threshold.

use std::collections::{HashMap, HashSet};

use super::asm::{Instr, Kind};
use super::opcodes::arity;

/// The label-symbol prefix venom gives every internal-function symbol (entry and the per-
/// function `_cleanup`/return label both share it).
const ENTRY_PREFIX: &str = "_sym_internal";

/// The suffix venom gives an internal-function ENTRY label specifically (`_sym_internal …
/// _runtime`), distinguishing it from the function's own internal labels such as `_cleanup`.
const ENTRY_SUFFIX: &str = "_runtime";

/// `s` is an internal-function entry symbol — a call target, as opposed to a function's own
/// internal label (e.g. its `_cleanup` return point).
#[inline]
fn is_entry_sym(s: &str) -> bool {
    s.starts_with(ENTRY_PREFIX) && s.ends_with(ENTRY_SUFFIX)
}

/// An inlinable internal function: its entry label, the inclusive body range that follows it,
/// and every call site (the index of the `pushsym entry` instruction).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Index of the entry `Label`.
    pub entry: usize,
    /// First body instruction (just after the entry label).
    pub body_start: usize,
    /// Last body instruction (inclusive).
    pub body_end: usize,
    /// Indices of the `pushsym entry` instruction at each call site.
    pub call_sites: Vec<usize>,
}

impl Plan {
    /// Number of instructions in the function body.
    #[allow(dead_code)] // used in tests; a useful accessor on the public Plan
    #[inline]
    pub fn body_len(&self) -> usize {
        self.body_end - self.body_start + 1
    }
}

/// Every internal function whose body is at most `max_body` instructions and is provably safe
/// to inline into all of its call sites. Plans never overlap each other.
pub fn find_inlinable(instrs: &[Instr], max_body: usize) -> Vec<Plan> {
    let refs = pushsym_refs(instrs);
    let mut plans = Vec::new();
    for (e, ins) in instrs.iter().enumerate() {
        if is_entry_label(ins) {
            if let Some(plan) = analyze(instrs, e, &refs, max_body) {
                plans.push(plan);
            }
        }
    }
    plans
}

/// `ins` is an internal-function entry label (`_sym_internal… _runtime JUMPDEST`).
#[inline]
fn is_entry_label(ins: &Instr) -> bool {
    ins.kind == Kind::Label && ins.tokens.len() > 1 && is_entry_sym(&ins.tokens[0])
}

/// Map every `_sym_*` push to the indices that reference it.
fn pushsym_refs(instrs: &[Instr]) -> HashMap<String, Vec<usize>> {
    let mut map: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, ins) in instrs.iter().enumerate() {
        if ins.kind == Kind::PushSym {
            map.entry(ins.mnem().to_string()).or_default().push(i);
        }
    }
    map
}

/// Validate the function entered at `e` and, if every safety premise holds, return its [`Plan`].
fn analyze(
    instrs: &[Instr],
    e: usize,
    refs: &HashMap<String, Vec<usize>>,
    max_body: usize,
) -> Option<Plan> {
    let sym = instrs[e].tokens[0].as_str();
    let f = body_end(instrs, e, refs)?;
    let body_start = e + 1;
    let body_end = f - 1;
    if body_end < body_start {
        return None;
    }
    if body_end - body_start + 1 > max_body {
        return None;
    }

    let internal = internal_labels(instrs, body_start, body_end);
    if !body_is_self_contained(instrs, body_start, body_end, &internal) {
        return None;
    }
    if external_jumps_into_body(refs, body_start, body_end, &internal) {
        return None;
    }

    let call_sites = call_sites(instrs, refs, sym, body_start, body_end)?;
    Some(Plan {
        entry: e,
        body_start,
        body_end,
        call_sites,
    })
}

/// The exclusive end of the body that follows entry `e`: the first later label that belongs to
/// another region (another internal function, or a block referenced from at or before `e` — a
/// shared block such as the revert handler), or the trailing data table. `None` if the body is
/// empty.
fn body_end(instrs: &[Instr], e: usize, refs: &HashMap<String, Vec<usize>>) -> Option<usize> {
    let mut f = e + 1;
    while f < instrs.len() {
        let ins = &instrs[f];
        if ins.kind == Kind::Label && ins.tokens.len() > 1 {
            let sym = ins.tokens[0].as_str();
            let foreign =
                is_entry_sym(sym) || refs.get(sym).is_some_and(|idx| idx.iter().any(|&r| r <= e));
            if foreign {
                break;
            }
        }
        if ins.kind == Kind::Raw && ins.mnem().starts_with('[') {
            break;
        }
        f += 1;
    }
    (f > e + 1).then_some(f)
}

/// The set of label symbols defined inside `[start, end]`.
fn internal_labels(instrs: &[Instr], start: usize, end: usize) -> HashSet<String> {
    let mut set = HashSet::new();
    for ins in &instrs[start..=end] {
        if ins.kind == Kind::Label && ins.tokens.len() > 1 {
            set.insert(ins.tokens[0].clone());
        }
    }
    set
}

/// The body jumps only to its own labels, calls no other internal function, does not recurse,
/// and carries no `_mem_`/`_OFST` operand. A pushed entry symbol (`…_runtime`) is a call to a
/// function (its own — recursion — or another), which the MVP refuses; the function's own
/// internal labels (e.g. `_cleanup`) are defined in the body and so pass the closure check.
fn body_is_self_contained(
    instrs: &[Instr],
    start: usize,
    end: usize,
    internal: &HashSet<String>,
) -> bool {
    for ins in &instrs[start..=end] {
        match ins.kind {
            Kind::PushSym => {
                let s = ins.mnem();
                if is_entry_sym(s) {
                    return false; // recursion or a call into another internal function
                }
                if !internal.contains(s) {
                    return false; // a jump out of the body
                }
            }
            Kind::PushMem | Kind::Ofst => return false,
            _ => {}
        }
    }
    true
}

/// Some instruction outside `[start, end]` jumps to one of the body's internal labels.
fn external_jumps_into_body(
    refs: &HashMap<String, Vec<usize>>,
    start: usize,
    end: usize,
    internal: &HashSet<String>,
) -> bool {
    internal.iter().any(|lbl| {
        refs.get(lbl)
            .is_some_and(|idx| idx.iter().any(|&r| r < start || r > end))
    })
}

/// Every reference to the entry symbol, validated as a `pushsym ret; pushsym entry; JUMP` call
/// site located outside the body. `None` if there are no references or any is malformed.
fn call_sites(
    instrs: &[Instr],
    refs: &HashMap<String, Vec<usize>>,
    entry_sym: &str,
    body_start: usize,
    body_end: usize,
) -> Option<Vec<usize>> {
    let entry_refs = refs.get(entry_sym)?;
    let mut sites = Vec::with_capacity(entry_refs.len());
    for &r in entry_refs {
        if r == 0 || (r >= body_start && r <= body_end) {
            return None;
        }
        let ret = &instrs[r - 1];
        let jump = instrs.get(r + 1);
        let well_formed = ret.kind == Kind::PushSym
            && matches!(jump, Some(j) if j.kind == Kind::Op && j.mnem() == "JUMP");
        if !well_formed {
            return None;
        }
        sites.push(r);
    }
    (!sites.is_empty()).then_some(sites)
}

/// If `body` is a **straight-line tail-return** function — a single basic block whose only jump is
/// a trailing dynamic `JUMP` returning to the threaded return address — return the body with the
/// return address eliminated: the trailing `JUMP` becomes a fall-through and every `DUP`/`SWAP`
/// that reached past the return-address slot is renumbered one shallower. The caller then also
/// drops its `pushsym ret`, so the relocated body needs no return indirection at all (the return
/// `JUMP`, the address push, and the stack shuffle that raised it for the return are all gone).
///
/// `None` when the body is not this shape — it has a label, a `JUMPI`, or a non-final `JUMP`
/// (control flow whose merge inherently needs a jump), or it would consume/duplicate the return
/// address as a value. Such functions try [`dethread_diamond`] next, then the verbatim relocation.
///
/// The return address sits on top of the stack at function entry (the caller pushes it last before
/// the entry symbol). The pass simulates the single block tracking only that address's stack depth.
pub fn dethread_tail_return(body: &[Instr]) -> Option<Vec<Instr>> {
    let last = body.len().checked_sub(1)?;
    if !(body[last].kind == Kind::Op && body[last].mnem() == "JUMP") {
        return None;
    }
    let (out, end) = dethread_segment(&body[..last], 0, true)?;
    // The trailing JUMP must consume the return address from the top.
    (end == 0).then_some(out)
}

/// The synthetic join label a de-threaded diamond falls through to; renamed per copy by
/// [`crate::features::inline`] so duplicate copies never collide.
const EXIT_LABEL: &str = "_sym_inlexit";

/// If `body` is a **single-merge diamond** — a header that branches to one of two straight-line
/// arms, both rejoining at a single merge block that returns via the threaded address — return the
/// body with the return address eliminated and the two arms joined at a fresh fall-through label,
/// so the dynamic return `JUMP` (and the call site's `pushsym ret`) disappear.
///
/// This is the branching counterpart of [`dethread_tail_return`]. venom lays an `if`/`else` out as
///
/// ```text
///   <header ...> pushsym ARM_B; JUMPI   ; conditional to the branch arm
///   <fall-through arm ...>               ; runs when not taken, falls into the merge
///   MERGE: <tail ...> JUMP               ; the merge: brings the address up, returns
///   ARM_B: <branch arm ...> pushsym MERGE; JUMP
/// ```
///
/// The merge sits physically between the two arms, so deleting it and retargeting both arms to a
/// new join placed at the body end yields the optimal structure with no reordering: the
/// fall-through arm jumps to the join, the branch arm falls into it, and the merge's de-threaded
/// tail (the address-raising `SWAP` dropped, any real ops kept) runs once at the join. The return
/// value, if any, sits below the address and falls through to the continuation unchanged.
///
/// `None` (→ verbatim relocation) unless the body matches this canonical shape exactly — one
/// dynamic return, one conditional, two straight-line arms agreeing on the merge-entry depth, and
/// no op consuming or duplicating the return address.
pub fn dethread_diamond(body: &[Instr]) -> Option<Vec<Instr>> {
    let (rj, ji) = diamond_control(body)?;
    let last = body.len() - 1;
    let rl = nearest_label_before(body, rj)?;
    if !(ji < rl && rl < rj && rj < last) {
        return None;
    }
    if !(body[ji - 1].kind == Kind::PushSym) {
        return None;
    }
    let lb = body[ji - 1].mnem().to_string();
    let r_label = body[rl].tokens[0].clone();
    // Branch arm: `ARM_B:` right after the merge, ending in `pushsym MERGE; JUMP`.
    let el = rj + 1;
    let arm_b_labelled =
        body[el].kind == Kind::Label && body[el].tokens.len() > 1 && body[el].tokens[0] == lb;
    let branch_returns = body[last].kind == Kind::Op
        && body[last].mnem() == "JUMP"
        && body[last - 1].kind == Kind::PushSym
        && body[last - 1].mnem() == r_label;
    if !(arm_b_labelled && branch_returns) {
        return None;
    }
    // The canonical diamond defines exactly the merge label and the branch-arm label.
    let extra_label = body
        .iter()
        .enumerate()
        .any(|(i, ins)| ins.kind == Kind::Label && ins.tokens.len() > 1 && i != rl && i != el);
    if extra_label {
        return None;
    }

    let header = &body[0..ji - 1];
    let arm_fall = &body[ji + 1..rl];
    let r_tail = &body[rl + 1..rj];
    let arm_branch = &body[el + 1..last - 1];

    let (header_out, d0) = dethread_segment(header, 0, false)?;
    // `pushsym ARM_B` (+1) then `JUMPI` (-2) leave both arms at the same depth.
    let d = (d0 + 1).checked_sub(2)?;
    let (fall_out, df) = dethread_segment(arm_fall, d, false)?;
    let (branch_out, db) = dethread_segment(arm_branch, d, false)?;
    if df != db {
        return None;
    }
    let (tail_out, dt) = dethread_segment(r_tail, df, true)?;
    if dt != 0 {
        return None;
    }

    let mut out = Vec::with_capacity(body.len());
    out.extend(header_out);
    out.push(Instr::new(Kind::PushSym, vec![lb.clone()]));
    out.push(op("JUMPI".to_string()));
    out.extend(fall_out);
    out.push(Instr::new(Kind::PushSym, vec![EXIT_LABEL.to_string()]));
    out.push(op("JUMP".to_string()));
    out.push(Instr::new(Kind::Label, vec![lb, "JUMPDEST".to_string()]));
    out.extend(branch_out);
    out.push(Instr::new(
        Kind::Label,
        vec![EXIT_LABEL.to_string(), "JUMPDEST".to_string()],
    ));
    out.extend(tail_out);
    Some(out)
}

/// The `(return_jump, conditional)` indices of a single-merge diamond: exactly one dynamic return
/// `JUMP` (one not preceded by a pushed target) and exactly one `JUMPI` (a static conditional).
/// `None` if either is missing or duplicated, or the conditional is not a static branch.
fn diamond_control(body: &[Instr]) -> Option<(usize, usize)> {
    let mut ret_jump = None;
    let mut cond = None;
    for (i, ins) in body.iter().enumerate() {
        if ins.kind != Kind::Op {
            continue;
        }
        match ins.mnem() {
            "JUMP" if !is_static_target(body, i) => {
                if ret_jump.is_some() {
                    return None;
                }
                ret_jump = Some(i);
            }
            "JUMPI" => {
                if cond.is_some() || !is_static_target(body, i) {
                    return None;
                }
                cond = Some(i);
            }
            _ => {}
        }
    }
    Some((ret_jump?, cond?))
}

/// A `JUMP`/`JUMPI` at `i` jumps to a statically pushed target (the preceding instruction pushes a
/// label), as opposed to the dynamic return that consumes the threaded address.
#[inline]
fn is_static_target(body: &[Instr], i: usize) -> bool {
    i > 0 && body[i - 1].kind == Kind::PushSym
}

/// The index of the nearest named-label definition before `i`.
#[inline]
fn nearest_label_before(body: &[Instr], i: usize) -> Option<usize> {
    (0..i)
        .rev()
        .find(|&k| body[k].kind == Kind::Label && body[k].tokens.len() > 1)
}

/// De-thread one straight-line `ops` segment given the return address's depth at its start
/// (`0` = on top). Returns the rewritten ops — every `DUP`/`SWAP` reaching past the removed
/// address slot renumbered one shallower — and the depth at the segment end. `None` if an op would
/// consume or duplicate the address, or a control-flow/`_mem_`/`_OFST` instruction appears (the
/// caller handles control flow). A `SWAP` that raises the address back to the top is allowed only
/// as the final op, and only when `final_raise_ok` (the merge tail just before a return).
fn dethread_segment(
    ops: &[Instr],
    start_depth: usize,
    final_raise_ok: bool,
) -> Option<(Vec<Instr>, usize)> {
    let mut out = Vec::with_capacity(ops.len());
    let mut ret_depth = start_depth;
    for (i, ins) in ops.iter().enumerate() {
        let is_last = i + 1 == ops.len();
        let m = ins.mnem();
        if matches!(ins.kind, Kind::PushMem | Kind::Ofst | Kind::Label) {
            return None;
        }
        if ins.kind == Kind::Op && (m == "JUMP" || m == "JUMPI") {
            return None;
        }
        if let Some(n) = suffix_n(m, "SWAP") {
            if ret_depth == 0 {
                // The address is on top; this buries it. In the address-free stack the value at
                // depth n rises one position.
                if n >= 2 {
                    out.push(op(format!("SWAP{}", n - 1)));
                }
                ret_depth = n;
            } else if ret_depth == n {
                if !(final_raise_ok && is_last) {
                    return None;
                }
                ret_depth = 0;
            } else {
                out.push(op(format!("SWAP{}", if ret_depth < n { n - 1 } else { n })));
            }
        } else if let Some(n) = suffix_n(m, "DUP") {
            if ret_depth + 1 == n {
                return None; // a DUP of the return address (it reads stack depth n-1)
            }
            out.push(op(format!(
                "DUP{}",
                if ret_depth < n - 1 { n - 1 } else { n }
            )));
            ret_depth += 1;
        } else {
            let (pops, pushes) = stack_effect(ins)?;
            if ret_depth < pops {
                return None; // the op would consume the return address
            }
            out.push(ins.clone());
            ret_depth = ret_depth - pops + pushes;
        }
    }
    Some((out, ret_depth))
}

/// The numeric suffix of a `DUP`/`SWAP` mnemonic (`SWAP2` -> `2`), or `None` for any other op.
#[inline]
fn suffix_n(mnem: &str, prefix: &str) -> Option<usize> {
    mnem.strip_prefix(prefix)?.parse().ok()
}

/// The `(pops, pushes)` stack effect of a non-`DUP`/`SWAP` body instruction.
#[inline]
fn stack_effect(ins: &Instr) -> Option<(usize, usize)> {
    match ins.kind {
        Kind::Push => Some((0, 1)),
        Kind::Raw => Some((0, 0)), // an immediate byte of a preceding push; no stack effect
        Kind::Op => arity(ins.mnem()),
        _ => None,
    }
}

/// A bare-opcode instruction.
#[inline]
fn op(mnem: String) -> Instr {
    Instr::new(Kind::Op, vec![mnem])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::parse_str;

    // A single-call-site internal function: a `pushsym ret; pushsym entry; JUMP` call, the
    // continuation label, then the function body returning by `JUMP`. parse_str cannot encode
    // venom's space-bearing entry symbol, so the tests use the single-token `_sym_internal_f_runtime`
    // (same `_sym_internal` prefix the detector keys on).
    const ONE_CALL: &str = "\
        PUSH1 4 CALLDATALOAD \
        _sym_ret0 _sym_internal_f_runtime JUMP \
        _sym_ret0 JUMPDEST STOP \
        _sym_internal_f_runtime JUMPDEST PUSH1 1 ADD JUMP";

    #[test]
    fn finds_a_simple_function_and_its_call_site() {
        // The body `PUSH1 1 ADD JUMP` (4 instrs) is self-contained and has one well-formed
        // call site, so it must be reported as inlinable.
        let p = parse_str(ONE_CALL);
        let plans = find_inlinable(&p, 20);
        assert_eq!(
            plans.len(),
            1,
            "the one inlinable function was not discovered"
        );
        assert_eq!(
            plans[0].call_sites.len(),
            1,
            "the single call site was not found"
        );
        assert_eq!(
            plans[0].body_len(),
            3,
            "the body extent was computed wrongly"
        );
    }

    #[test]
    fn threshold_excludes_a_too_large_body() {
        // With a threshold below the body size (3) the function must not be inlined.
        let p = parse_str(ONE_CALL);
        assert!(
            find_inlinable(&p, 2).is_empty(),
            "a body over the size threshold was wrongly reported as inlinable"
        );
    }

    #[test]
    fn recursive_function_is_rejected() {
        // The body pushes its own entry symbol (a self-call), which inlining must refuse.
        let src = "\
            _sym_ret0 _sym_internal_f_runtime JUMP _sym_ret0 JUMPDEST STOP \
            _sym_internal_f_runtime JUMPDEST _sym_ret1 _sym_internal_f_runtime JUMP _sym_ret1 JUMPDEST JUMP";
        let p = parse_str(src);
        assert!(
            find_inlinable(&p, 20).is_empty(),
            "a recursive function was wrongly inlined"
        );
    }

    #[test]
    fn malformed_call_site_is_rejected() {
        // The entry is pushed without a preceding return-address push (not the call idiom), so
        // the function cannot be inlined.
        let src = "_sym_internal_f_runtime JUMP _sym_internal_f_runtime JUMPDEST PUSH1 1 JUMP";
        let p = parse_str(src);
        assert!(
            find_inlinable(&p, 20).is_empty(),
            "a function whose entry is used outside the call idiom was wrongly inlined"
        );
    }

    #[test]
    fn external_jump_into_body_is_rejected() {
        // Code outside the body jumps to an internal label of the function, so relocating the
        // body would break that jump — it must be refused.
        let src = "\
            _sym_inner JUMP \
            _sym_ret0 _sym_internal_f_runtime JUMP _sym_ret0 JUMPDEST STOP \
            _sym_internal_f_runtime JUMPDEST _sym_inner JUMPDEST PUSH1 1 JUMP";
        let p = parse_str(src);
        assert!(
            find_inlinable(&p, 20).is_empty(),
            "a function whose internal label is targeted from outside was wrongly inlined"
        );
    }

    #[test]
    fn dethread_eliminates_the_return_on_a_straight_line_body() {
        // venom tail-return body for `(x|y)&255`: SWAP2 buries the return address, SWAP1 raises it
        // for the return JUMP. De-threading drops both the raise+JUMP and renumbers SWAP2 -> SWAP1.
        let body = parse_str("SWAP2 OR PUSH1 0xff AND SWAP1 JUMP");
        let out =
            dethread_tail_return(&body).expect("a straight-line tail-return body must de-thread");
        assert_eq!(
            crate::core::asm::mnemonics(&out),
            vec!["SWAP1", "OR", "PUSH1", "AND"],
            "de-threading did not remove the return indirection / renumber the buried swap"
        );
    }

    #[test]
    fn dethread_rejects_a_branching_body() {
        // A body with a JUMPI is not a single basic block; its merge needs a jump, so it must NOT
        // de-thread (the verbatim relocation is used instead).
        let body = parse_str("DUP1 _sym_skip JUMPI POP _sym_skip JUMPDEST SWAP1 JUMP");
        assert!(
            dethread_tail_return(&body).is_none(),
            "a branching body was wrongly de-threaded"
        );
    }

    #[test]
    fn dethread_rejects_a_non_tail_return() {
        // The last instruction is not the return JUMP, so the shape is not tail-return.
        let body = parse_str("SWAP1 JUMP PUSH1 1");
        assert!(
            dethread_tail_return(&body).is_none(),
            "a body whose last op is not the return JUMP was wrongly de-threaded"
        );
    }

    #[test]
    fn dethread_diamond_eliminates_the_return_on_a_void_diamond() {
        // venom `if`/`else` with no trailing code (the `_advance_to` shape): the fall-through arm
        // (then) and the branch arm (else) rejoin at an empty merge that returns. De-threading
        // deletes the merge, joins both arms at a fresh `_sym_inlexit`, and the dynamic return JUMP
        // disappears (the fall-through arm jumps to the join, the branch arm falls into it).
        let body = parse_str(
            "SWAP2 DUP2 DUP2 LT _sym_else JUMPI \
             POP PUSH1 0xb SHL PUSH3 0x400000 OR PUSH1 1 TSTORE \
             _sym_merge JUMPDEST JUMP \
             _sym_else JUMPDEST SWAP1 PUSH1 0xb SHL OR PUSH1 1 TSTORE _sym_merge JUMP",
        );
        let out = dethread_diamond(&body).expect("a single-merge void diamond must de-thread");
        assert_eq!(
            crate::core::asm::mnemonics(&out),
            vec![
                "SWAP1",
                "DUP2",
                "DUP2",
                "LT",
                "_sym_else",
                "JUMPI",
                "POP",
                "PUSH1",
                "SHL",
                "PUSH3",
                "OR",
                "PUSH1",
                "TSTORE",
                "_sym_inlexit",
                "JUMP",
                "_sym_else",
                "SWAP1",
                "PUSH1",
                "SHL",
                "OR",
                "PUSH1",
                "TSTORE",
                "_sym_inlexit",
            ],
            "the diamond de-thread did not remove the merge/return or renumber the buried swap"
        );
    }

    #[test]
    fn dethread_diamond_handles_a_value_returning_merge() {
        // venom `if x<y: return x; return y` (the `_pick` shape): the merge raises the return value
        // above the address with a `SWAP1` before returning. De-threading drops that raise so the
        // return value falls through to the continuation on top of the stack.
        let body = parse_str(
            "SWAP2 DUP2 DUP2 LT _sym_then JUMPI \
             POP _sym_merge JUMPDEST SWAP1 JUMP \
             _sym_then JUMPDEST SWAP1 POP _sym_merge JUMP",
        );
        let out =
            dethread_diamond(&body).expect("a value-returning single-merge diamond must de-thread");
        assert_eq!(
            crate::core::asm::mnemonics(&out),
            vec![
                "SWAP1",
                "DUP2",
                "DUP2",
                "LT",
                "_sym_then",
                "JUMPI",
                "POP",
                "_sym_inlexit",
                "JUMP",
                "_sym_then",
                "SWAP1",
                "POP",
                "_sym_inlexit",
            ],
            "the value-returning diamond de-thread did not drop the merge's address-raising swap"
        );
    }

    #[test]
    fn dethread_diamond_rejects_two_return_points() {
        // Two dynamic return JUMPs are two exits, not a single-merge diamond — it must NOT
        // de-thread (verbatim relocation handles it).
        let body = parse_str("PUSH1 1 JUMP PUSH1 2 JUMP");
        assert!(
            dethread_diamond(&body).is_none(),
            "a body with two return points was wrongly de-threaded as a diamond"
        );
    }

    #[test]
    fn dethread_diamond_rejects_a_nested_branch() {
        // A second conditional means the control flow is not the canonical one-branch diamond.
        let body = parse_str(
            "DUP1 _sym_a JUMPI DUP1 _sym_b JUMPI POP _sym_m JUMPDEST JUMP \
             _sym_a JUMPDEST _sym_m JUMP _sym_b JUMPDEST _sym_m JUMP",
        );
        assert!(
            dethread_diamond(&body).is_none(),
            "a body with a nested branch was wrongly de-threaded as a diamond"
        );
    }

    #[test]
    fn multiple_call_sites_are_all_collected() {
        // Two well-formed call sites for the same function must both be reported.
        let src = "\
            _sym_ret0 _sym_internal_f_runtime JUMP _sym_ret0 JUMPDEST \
            _sym_ret1 _sym_internal_f_runtime JUMP _sym_ret1 JUMPDEST STOP \
            _sym_internal_f_runtime JUMPDEST PUSH1 1 ADD JUMP";
        let p = parse_str(src);
        let plans = find_inlinable(&p, 20);
        assert_eq!(plans.len(), 1, "the inlinable function was not discovered");
        assert_eq!(
            plans[0].call_sites.len(),
            2,
            "not every call site was collected"
        );
    }
}
