//! Feature `recompute` — recompute a cheap result-invariant nullary opcode instead of
//! duplicating it.
//!
//! # What it optimizes
//!
//! A `DUP1` that duplicates the result of an immediately-preceding cheap nullary opcode
//! (e.g. `ADDRESS DUP1`, `CALLVALUE DUP1`, `PUSH0 DUP1`). `DUP1` costs 3 gas
//! (`G_verylow`); each opcode in [`RECOMPUTABLE`] costs only 2 (`G_base`) and reads
//! nothing, so re-executing it (`OP OP`) leaves the identical stack one gas cheaper than
//! `OP DUP1`. The `DUP1` is rewritten to a second copy of the opcode. Like
//! [`crate::features::shuffle`] and [`crate::features::involution`] this is **always
//! safe** — it needs no trusted caller.
//!
//! # Why it fires at all
//!
//! Both compilers leave the pattern in their optimized output. solc emits the
//! non-payable guard `CALLVALUE DUP1 ISZERO …` once per call and `PUSH0 DUP1 REVERT`
//! in every revert block; Vyper's venom emits `ADDRESS DUP1 ADDRESS` when a contract
//! uses an environment value (e.g. `self`) more than once. Neither folds `OP DUP1` into
//! `OP OP`, so this is a real, ubiquitous one-gas-per-occurrence win. Measured (see
//! `e2e.rs`), creation bytecode byte-for-byte the same size in both: solc 0.8.24 — the
//! call-path `CALLVALUE DUP1` non-payable guard drops a `f(uint256)` call 22103 → 22102;
//! Vyper 0.4.3 venom — a per-iteration `CHAINID DUP1` in a loop body drops it 22099 →
//! 22094 (−5 over 5 iterations).
//!
//! # How it is sound
//!
//! Every opcode in [`RECOMPUTABLE`] is **nullary** (pops nothing, pushes one word) and
//! **result-invariant within a transaction**: `PUSH0` is the constant `0`, and the
//! environment opcodes (`ADDRESS`/`CALLER`/`CALLVALUE`/`TIMESTAMP`/…) return the same
//! word throughout one execution. So `OP` immediately followed by `DUP1` pushes that
//! same word twice, exactly as `OP OP` does — the deeper stack is untouched. A maximal
//! run of `DUP1` directly after the opcode duplicates the same invariant word each time,
//! so every `DUP1` in the run can recompute the opcode.
//!
//! Excluded on purpose: `GAS`/`PC` (change per executed op/position), `MSIZE`/
//! `RETURNDATASIZE` (change with memory / after a `CALL`), `BALANCE`/`SELFBALANCE`
//! (state-dependent and not `G_base`), and `PUSH1..PUSH32` (recomputing them as a push
//! would need the immediate and would grow the bytecode, since they are multi-byte).
//!
//! # Length-preserving — runs on symbolic AND concrete input
//!
//! Unlike `shuffle`/`involution`, this rewrites one single-byte opcode (`DUP1`, `0x80`)
//! into another single-byte opcode, so it **never shifts a `JUMPDEST` offset**. The
//! orchestrator ([`crate::features::optimize`]) therefore runs it on every program, not
//! only the symbolic (sidecar) path — it is the one pass that also lowers gas on raw
//! concrete `.hex`/`.bin` bytecode, where no compiler relinks for us.

use super::FeatureMeta;
use crate::core::asm::Kind;
use crate::core::{Category, Instr, Span, apply_spans};

#[cfg(test)]
mod e2e;

pub const META: FeatureMeta = FeatureMeta {
    key: "recompute",
    name: "Recompute",
    description: "recompute a cheap nullary opcode instead of DUP-ing it (OP DUP1 -> OP OP; always safe)",
    category: Category::Recompute,
    default_enabled: true,
};

/// The `DUP` this pass rewrites: only `DUP1` reaches the opcode's just-pushed value.
const DUP1: &str = "DUP1";

/// Cheap (`G_base` = 2 gas) nullary opcodes whose result is invariant within a single
/// transaction execution — recomputing one is cheaper than a `DUP1` (`G_verylow` = 3)
/// and leaves the identical stack. `PUSH0` is the constant `0`; the rest read fixed
/// transaction/block environment. See the module doc for what is deliberately excluded.
const RECOMPUTABLE: &[&str] = &[
    "PUSH0", "ADDRESS", "ORIGIN", "CALLER", "CALLVALUE", "CALLDATASIZE", "CODESIZE",
    "GASPRICE", "COINBASE", "TIMESTAMP", "NUMBER", "PREVRANDAO", "GASLIMIT", "CHAINID",
    "BASEFEE", "BLOBBASEFEE",
];

/// `ins` is a cheap result-invariant nullary opcode this pass can recompute.
#[inline]
fn is_recomputable(ins: &Instr) -> bool {
    ins.kind == Kind::Op && RECOMPUTABLE.contains(&ins.mnem())
}

/// `ins` is a `DUP1`.
#[inline]
fn is_dup1(ins: &Instr) -> bool {
    ins.kind == Kind::Op && ins.mnem() == DUP1
}

/// A [`Span`] for every `DUP1` that duplicates a recomputable opcode's just-pushed value,
/// replacing the `DUP1` with a second copy of that opcode. A maximal `DUP1` run directly
/// after the opcode duplicates the same invariant word each time, so every `DUP1` in it
/// is rewritten.
pub fn scan(instrs: &[Instr]) -> Vec<Span> {
    let mut out = Vec::new();
    let n = instrs.len();
    let mut i = 0;
    while i < n {
        if !is_recomputable(&instrs[i]) {
            i += 1;
            continue;
        }
        let op = instrs[i].mnem().to_string();
        let mut j = i + 1;
        while j < n && is_dup1(&instrs[j]) {
            out.push(Span { start: j, end: j, category: Category::Recompute, replacement: vec![op.clone()] });
            j += 1;
        }
        i = j.max(i + 1);
    }
    out
}

/// Apply every recompute rewrite (for tests/targeted runs); the CLI rewrites via the
/// enabled config through [`crate::features::optimize`].
#[allow(dead_code)] // feature's module API; the CLI rewrites via the orchestrator
pub fn eliminate(instrs: &[Instr]) -> (Vec<Instr>, Vec<Span>) {
    let spans = scan(instrs);
    (apply_spans(instrs, &spans), spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{mnemonics, parse_str};

    #[test]
    fn address_dup1_recomputed() {
        // ADDRESS DUP1 pushes the contract address twice; ADDRESS ADDRESS is the same
        // stack one gas cheaper.
        let p = parse_str("ADDRESS DUP1 ADD");
        let (out, spans) = eliminate(&p);
        assert_eq!(spans.len(), 1, "ADDRESS DUP1 was not recomputed");
        assert_eq!(spans[0].category, Category::Recompute, "the span must carry the Recompute category");
        assert_eq!(mnemonics(&out), vec!["ADDRESS", "ADDRESS", "ADD"], "DUP1 was not rewritten to a second ADDRESS");
    }

    #[test]
    fn push0_dup1_recomputed() {
        // PUSH0 DUP1 REVERT (the universal empty-revert idiom) -> PUSH0 PUSH0 REVERT.
        let p = parse_str("PUSH0 DUP1 REVERT");
        let (out, spans) = eliminate(&p);
        assert_eq!(spans.len(), 1, "PUSH0 DUP1 was not recomputed");
        assert_eq!(mnemonics(&out), vec!["PUSH0", "PUSH0", "REVERT"], "DUP1 of PUSH0 was not rewritten to PUSH0");
    }

    #[test]
    fn dup1_run_all_recomputed() {
        // A maximal DUP1 run after the opcode all duplicate the same invariant word.
        let p = parse_str("CALLER DUP1 DUP1");
        let (out, spans) = eliminate(&p);
        assert_eq!(spans.len(), 2, "a DUP1 run after a recomputable op was not fully rewritten");
        assert_eq!(mnemonics(&out), vec!["CALLER", "CALLER", "CALLER"], "the DUP1 run was not all recomputed");
    }

    #[test]
    fn dup2_not_touched() {
        // DUP2 duplicates a deeper slot, not the opcode's value — must not be rewritten.
        let p = parse_str("ADDRESS DUP2");
        let (_out, spans) = eliminate(&p);
        assert!(spans.is_empty(), "DUP2 (a deeper slot) was wrongly recomputed as the opcode");
    }

    #[test]
    fn dup1_of_non_invariant_op_kept() {
        // GAS changes every opcode, so GAS DUP1 != GAS GAS — must never be recomputed.
        let p = parse_str("GAS DUP1");
        let (_out, spans) = eliminate(&p);
        assert!(spans.is_empty(), "a non-invariant op (GAS) was wrongly recomputed");
    }

    #[test]
    fn lone_dup1_kept() {
        // A DUP1 not preceded by a recomputable op duplicates an arbitrary value — keep it.
        let p = parse_str("CALLDATALOAD DUP1");
        let (_out, spans) = eliminate(&p);
        assert!(spans.is_empty(), "a DUP1 of a non-recomputable value was wrongly rewritten");
    }
}
