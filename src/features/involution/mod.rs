//! Feature `involution` — cancel runs of an involutive opcode.
//!
//! # What it optimizes
//!
//! A maximal run of consecutive `NOT` opcodes. `NOT` is an involution
//! (`NOT(NOT(x)) == x` for every 256-bit `x`), so an even-length run is a no-op and
//! an odd-length run equals a single `NOT`. The run is replaced by that net effect —
//! nothing, or one `NOT`. Like [`crate::features::shuffle`] this computes no value and
//! touches no memory/storage, so it is **always safe**: it needs no trusted caller.
//!
//! # Why it fires at all
//!
//! Vyper's `venom` backend does NOT fold a double bitwise complement: source `~(~x)`
//! lowers to a literal `... NOT NOT ...` in the runtime assembly (measured on Vyper
//! 0.4.3 venom + `OptimizationLevel.GAS`). solc's optimizer folds the same `~~x` to
//! nothing, so this is a Vyper-effective pass. `ISZERO ISZERO` is deliberately NOT
//! handled here — see the README: it is an involution only on already-boolean inputs,
//! and the surviving compiler occurrences are not locally removable.
//!
//! # How it is sound
//!
//! `NOT` pops one word and pushes its bitwise complement; two in a row restore the
//! original word and leave every deeper slot untouched. A maximal run contains only
//! `NOT` opcodes (any other op or a label ends it), so the cancellation never crosses
//! a basic-block boundary or a value the run does not itself produce.
//!
//! # Symbolic only
//!
//! Deleting opcodes shifts `JUMPDEST` offsets, and gasripper has no linker, so this
//! pass runs **only** on symbolic programs (the Vyper/Solidity sidecar path, where the
//! compiler relinks). The orchestrator ([`crate::features::optimize`]) gates it on
//! [`crate::core::asm::is_symbolic`].

use super::FeatureMeta;
use crate::core::asm::Kind;
use crate::core::{Category, Instr, Span, apply_spans};

#[cfg(test)]
mod e2e;

pub const META: FeatureMeta = FeatureMeta {
    key: "involution",
    name: "Involution",
    description: "cancel runs of an involutive op (NOT NOT -> nothing; always safe)",
    category: Category::Involution,
    default_enabled: true,
};

/// The involutive mnemonic this pass cancels.
const NOT: &str = "NOT";

/// `ins` is a `NOT` opcode.
#[inline]
fn is_not(ins: &Instr) -> bool {
    ins.kind == Kind::Op && ins.mnem() == NOT
}

/// A [`Span`] for every maximal run of >= 2 `NOT`s, replacing it with its net effect
/// (empty for an even run, a single `NOT` for an odd one). A run holds only `NOT`
/// opcodes, so each rewrite is basic-block-local and value-preserving.
pub fn scan(instrs: &[Instr]) -> Vec<Span> {
    let mut out = Vec::new();
    let n = instrs.len();
    let mut i = 0;
    while i < n {
        if !is_not(&instrs[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && is_not(&instrs[i]) {
            i += 1;
        }
        if i - start >= 2 {
            let replacement = if (i - start) % 2 == 0 { Vec::new() } else { vec![NOT.to_string()] };
            out.push(Span { start, end: i - 1, category: Category::Involution, replacement });
        }
    }
    out
}

/// Cancel every involutive run (for tests/targeted runs); the CLI cancels via the
/// enabled config through [`crate::features::optimize`].
#[allow(dead_code)] // feature's module API; the CLI cancels via the orchestrator
pub fn eliminate(instrs: &[Instr]) -> (Vec<Instr>, Vec<Span>) {
    let spans = scan(instrs);
    (apply_spans(instrs, &spans), spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{mnemonics, parse_str};

    #[test]
    fn not_pair_deleted() {
        // NOT NOT is an identity on the top word — the whole run is deleted.
        let p = parse_str("NOT NOT");
        let (out, spans) = eliminate(&p);
        assert_eq!(spans.len(), 1, "an adjacent NOT pair was not cancelled");
        assert!(mnemonics(&out).is_empty(), "the cancelling NOT pair was not deleted: {:?}", mnemonics(&out));
    }

    #[test]
    fn triple_not_collapses_to_one() {
        // An odd run keeps exactly one NOT.
        let p = parse_str("NOT NOT NOT");
        let (out, spans) = eliminate(&p);
        assert_eq!(spans.len(), 1, "a triple-NOT run was not collapsed");
        assert_eq!(mnemonics(&out), vec!["NOT"], "an odd NOT run must collapse to a single NOT");
    }

    #[test]
    fn quad_not_deleted() {
        // Four complements cancel completely.
        let p = parse_str("NOT NOT NOT NOT");
        let (out, _spans) = eliminate(&p);
        assert!(mnemonics(&out).is_empty(), "an even NOT run was not fully cancelled: {:?}", mnemonics(&out));
    }

    #[test]
    fn lone_not_untouched() {
        // A single NOT is real work and must stay.
        let p = parse_str("NOT");
        let (_out, spans) = eliminate(&p);
        assert!(spans.is_empty(), "a lone NOT was wrongly cancelled");
    }

    #[test]
    fn not_run_broken_by_other_op() {
        // An op between the NOTs splits the run; neither lone NOT is cancellable.
        let p = parse_str("NOT ADD NOT");
        let (out, spans) = eliminate(&p);
        assert!(spans.is_empty(), "a NOT run was wrongly grown across a non-NOT op");
        assert_eq!(mnemonics(&out), vec!["NOT", "ADD", "NOT"], "live code around the NOTs was altered");
    }

    #[test]
    fn surrounding_code_preserved() {
        // Only the NOT pair is removed; the values it complemented flow through unchanged.
        let p = parse_str("CALLDATALOAD NOT NOT PUSH1 0x40 MSTORE");
        let (out, spans) = eliminate(&p);
        assert_eq!(spans.len(), 1, "the embedded NOT pair was not cancelled");
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD", "PUSH1", "MSTORE"],
            "cancelling NOT NOT disturbed the surrounding code"
        );
    }
}
