//! Feature `cmpnorm` — fold a `SWAP1` before a comparison into the mirrored comparator.
//!
//! # What it optimizes
//!
//! A `SWAP1` immediately followed by a strict-order comparison (`LT`/`GT`/`SLT`/`SGT`).
//! `SWAP1` reorders the two operands the comparison is about to consume, so swapping
//! them and then comparing equals comparing in the reversed direction without the swap:
//! the two-instruction window collapses to a single mirrored comparator, dropping the
//! `SWAP1` (3 gas, one byte). Like [`crate::features::shuffle`] this computes no new
//! value and touches no memory/storage, so it is **always safe** — it needs no trusted
//! caller, unlike [`crate::features::guards`].
//!
//! # Why it fires at all
//!
//! When a comparison's two operands are independent freshly-computed subexpressions
//! (`(a * b) < (c * e)`), Vyper's `venom` backend lands them on the stack in the order
//! it evaluated them and emits a `SWAP1` to put them in comparison order rather than
//! re-scheduling the producers — leaving `... SWAP1 LT ...` (measured on Vyper 0.4.3
//! venom + `OptimizationLevel.GAS`). solc instead selects operand order via `DUP` depth
//! and never emits the idiom, so this is a Vyper-effective pass.
//!
//! `EQ`/`SLT`-of-equality are deliberately NOT handled: equality is symmetric, so the
//! compiler already drops any `SWAP1` before `EQ` and the idiom never reaches us (an
//! inert rule would be pure noise). Only the four strict orders flip.
//!
//! # How it is sound
//!
//! `LT` pops the top word `a` then the next `b` and pushes `a < b`; `GT` pushes `a > b`.
//! A preceding `SWAP1` exchanges those top two words, so `SWAP1 LT` computes `b < a`,
//! which is exactly `GT` on the original order — and symmetrically `SWAP1 GT == LT`,
//! `SWAP1 SLT == SGT`, `SWAP1 SGT == SLT` (the signed forms compare the same two words).
//! The comparison must directly follow the `SWAP1` (the two words it swaps are the two
//! the comparison consumes); a label or any other op between them breaks the match, so
//! the rewrite is basic-block-local and never crosses a jump target.
//!
//! # Symbolic only
//!
//! Folding two instructions into one shifts every later `JUMPDEST` offset, and gasripper
//! has no linker, so — like `shuffle`/`involution`/`foldshift` — this pass runs **only**
//! on symbolic programs (the Vyper/Solidity sidecar path, where the compiler relinks).
//! The orchestrator ([`crate::features::optimize`]) gates it on
//! [`crate::core::asm::is_symbolic`].

use super::FeatureMeta;
use crate::core::asm::Kind;
use crate::core::{Category, Instr, Span, apply_spans};

#[cfg(test)]
mod e2e;

pub const META: FeatureMeta = FeatureMeta {
    key: "cmpnorm",
    name: "Compare normalize",
    description: "fold a SWAP1 before a comparison into the mirrored comparator (SWAP1 LT -> GT; always safe; symbolic input only)",
    category: Category::CmpNorm,
    default_enabled: true,
};

/// The stack-reorder op this pass folds away: `SWAP1` exchanges the top two words, the
/// exact pair the following comparison consumes.
const SWAP1: &str = "SWAP1";

/// `ins` is a `SWAP1` opcode.
#[inline]
fn is_swap1(ins: &Instr) -> bool {
    ins.kind == Kind::Op && ins.mnem() == SWAP1
}

/// The comparator with its operands mirrored, or `None` if `ins` is not a strict-order
/// comparison this pass folds. Equality is symmetric and never reaches us, so it is
/// excluded (see the module doc).
fn mirrored(ins: &Instr) -> Option<&'static str> {
    if ins.kind != Kind::Op {
        return None;
    }
    match ins.mnem() {
        "LT" => Some("GT"),
        "GT" => Some("LT"),
        "SLT" => Some("SGT"),
        "SGT" => Some("SLT"),
        _ => None,
    }
}

/// A [`Span`] for every `SWAP1` directly followed by a strict-order comparison, replacing
/// the two-instruction window with the single mirrored comparator. The comparison must be
/// the very next instruction, so each rewrite is basic-block-local.
pub fn scan(instrs: &[Instr]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < instrs.len() {
        if is_swap1(&instrs[i]) {
            if let Some(op) = mirrored(&instrs[i + 1]) {
                out.push(Span {
                    start: i,
                    end: i + 1,
                    category: Category::CmpNorm,
                    replacement: vec![op.to_string()],
                });
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Fold every `SWAP1`-before-comparison (for tests/targeted runs); the CLI folds via the
/// enabled config through [`crate::features::optimize`].
#[allow(dead_code)] // feature's module API; the CLI folds via the orchestrator
pub fn normalize(instrs: &[Instr]) -> (Vec<Instr>, Vec<Span>) {
    let spans = scan(instrs);
    (apply_spans(instrs, &spans), spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{mnemonics, parse_str};

    #[test]
    fn swap1_lt_becomes_gt() {
        // SWAP1 LT compares the operands in reverse — exactly GT without the swap.
        let p = parse_str("DUP2 DUP4 SWAP1 LT");
        let (out, spans) = normalize(&p);
        assert_eq!(spans.len(), 1, "a SWAP1 before LT was not folded into GT");
        assert_eq!(
            spans[0].category,
            Category::CmpNorm,
            "the span must carry the CmpNorm category"
        );
        assert_eq!(
            mnemonics(&out),
            vec!["DUP2", "DUP4", "GT"],
            "SWAP1 LT was not rewritten to a single GT"
        );
    }

    #[test]
    fn swap1_gt_becomes_lt() {
        // The mirror of the LT case.
        let p = parse_str("SWAP1 GT");
        let (out, spans) = normalize(&p);
        assert_eq!(spans.len(), 1, "a SWAP1 before GT was not folded into LT");
        assert_eq!(
            mnemonics(&out),
            vec!["LT"],
            "SWAP1 GT was not rewritten to a single LT"
        );
    }

    #[test]
    fn swap1_signed_orders_flip() {
        // The signed strict orders mirror the same way.
        let (out_slt, _) = normalize(&parse_str("SWAP1 SLT"));
        assert_eq!(
            mnemonics(&out_slt),
            vec!["SGT"],
            "SWAP1 SLT must fold to SGT"
        );
        let (out_sgt, _) = normalize(&parse_str("SWAP1 SGT"));
        assert_eq!(
            mnemonics(&out_sgt),
            vec!["SLT"],
            "SWAP1 SGT must fold to SLT"
        );
    }

    #[test]
    fn swap2_before_cmp_untouched() {
        // SWAP2 exchanges s0 and s2, NOT the two comparison operands, so folding it would
        // change the result — it must be left alone (this idiom co-occurs in real output).
        let p = parse_str("SWAP2 LT");
        let (out, spans) = normalize(&p);
        assert!(
            spans.is_empty(),
            "SWAP2 before a comparison was wrongly folded"
        );
        assert_eq!(
            mnemonics(&out),
            vec!["SWAP2", "LT"],
            "a non-foldable window was altered"
        );
    }

    #[test]
    fn swap1_eq_untouched() {
        // EQ is symmetric; the compiler never emits SWAP1 EQ, so the pass deliberately
        // does not handle it (an inert rule would be noise).
        let p = parse_str("SWAP1 EQ");
        let (_out, spans) = normalize(&p);
        assert!(
            spans.is_empty(),
            "SWAP1 EQ was folded though equality needs no normalization"
        );
    }

    #[test]
    fn swap1_non_comparison_untouched() {
        // SWAP1 before a non-comparison op is real stack work — must not be touched.
        let p = parse_str("SWAP1 ADD");
        let (_out, spans) = normalize(&p);
        assert!(
            spans.is_empty(),
            "a SWAP1 before a non-comparison op was wrongly folded"
        );
    }

    #[test]
    fn label_between_swap_and_cmp_blocks_fold() {
        // A JUMPDEST can be jumped to between the swap and the compare, so the SWAP1 is
        // not guaranteed to run before the LT — the window must not be folded.
        let p = parse_str("SWAP1 _sym_x JUMPDEST LT");
        let spans = scan(&p);
        assert!(
            spans.is_empty(),
            "a SWAP1/LT split by a label was wrongly folded"
        );
    }

    #[test]
    fn lone_comparison_untouched() {
        // A comparison with no preceding SWAP1 is already in normal form.
        let p = parse_str("DUP2 DUP4 LT");
        let (_out, spans) = normalize(&p);
        assert!(
            spans.is_empty(),
            "a comparison without a preceding SWAP1 was wrongly rewritten"
        );
    }
}
