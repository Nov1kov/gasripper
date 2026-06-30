//! Feature `superopt` — SMT block superoptimization (opt-in, `smt` Cargo feature).
//!
//! # What it optimizes
//!
//! A maximal **pure straight-line block** — only stack movement (`PUSH`/`DUP`/`SWAP`/`POP`) and the
//! result-invariant arithmetic/logic opcodes the engine interprets ([`crate::core::superopt`]). For
//! each such run the engine synthesizes the cheapest gas-equivalent instruction sequence and
//! **proves** the rewrite with Z3 (the candidate must leave the identical final stack on every
//! 256-bit input); the run is replaced only on that proof. So unlike the pattern-specific passes
//! (`recompute`, `cmpnorm`, …) it is not keyed on a fixed idiom — it discovers simplifications such
//! as `x + 0 → x`, `NOT NOT x → x`, or collapsing repeated recomputation, by search-and-prove.
//!
//! # How it is sound
//!
//! Only side-effect-free, control-flow-free, fully concrete opcodes are eligible, so a block-local
//! replacement is valid in any surrounding program (ebso's replacement lemma); the interpreted
//! opcodes map exactly onto EVM mod-2^256 semantics; and a rewrite is emitted only on a Z3 `unsat`
//! proof of non-equivalence. A solver timeout or anything unproven leaves the block untouched —
//! wrong bytecode in a gas tool is dangerous, so the failure mode is "do not optimize", never
//! "optimize unsafely".
//!
//! # Length-changing — symbolic input only (in the pipeline)
//!
//! A cheaper equivalent generally has a different instruction count, which shifts every later
//! `JUMPDEST` offset. Like `shuffle`/`cmpnorm`, the orchestrator therefore runs this pass only on
//! **symbolic** programs, where the compiler's assembler relinks the result; the concrete
//! `.hex`/`.bin` path has no linker. (The e2e proves the gas win directly on a hand-assembled
//! jumpless block, where the shift is harmless.)

use std::time::Instant;

use super::{progress, FeatureMeta};
use crate::core::opcodes::gas;
use crate::core::superopt;
use crate::core::{Category, Instr, Span, apply_spans};

#[cfg(test)]
mod e2e;

pub const META: FeatureMeta = FeatureMeta {
    key: "superopt",
    name: "Superopt",
    description: "replace a pure straight-line block with a cheaper SMT-proven-equivalent sequence",
    default_enabled: true,
    category: Category::Superopt,
};

/// A [`Span`] for every maximal pure run the engine can prove a strictly-cheaper equivalent for.
///
/// Emits `tracing` progress (visible at the default `info` level, since each block can take up to the
/// per-block solver timeout): one line per block as it is analyzed, one per proven rewrite with its
/// block-gas delta, and a final summary. Set `RUST_LOG=warn` to silence it.
pub fn scan(instrs: &[Instr]) -> Vec<Span> {
    let runs = pure_runs(instrs);
    if runs.is_empty() {
        return Vec::new();
    }
    let report = runs.len() >= progress::MIN_ITEMS;
    if report {
        tracing::info!("superopt: analyzing {} pure block(s) with Z3", runs.len());
    }
    let clock = Instant::now();
    let mut last = clock;
    let mut out = Vec::new();
    let mut saved = 0u32;
    for (idx, &(start, end)) in runs.iter().enumerate() {
        let run = &instrs[start..=end];
        if let Some(better) = superopt::optimize_block(run) {
            saved += block_gas(run).saturating_sub(block_gas(&better));
            out.push(Span {
                start,
                end,
                category: Category::Superopt,
                replacement: superopt::tokens(&better),
            });
        }
        if report && progress::due(&mut last) {
            let frac = (idx + 1) as f64 / runs.len() as f64;
            tracing::info!(
                "superopt: {}/{} blocks ({:.0}%), {} rewritten, ~{:.1}s left",
                idx + 1,
                runs.len(),
                frac * 100.0,
                out.len(),
                progress::eta(clock.elapsed(), frac),
            );
        }
    }
    if report {
        tracing::info!(
            "superopt: {} block(s) in {:.1}s — {} rewritten (block gas -{saved})",
            runs.len(),
            clock.elapsed().as_secs_f64(),
            out.len(),
        );
    }
    out
}

/// Inclusive `[start, end]` ranges of every maximal pure run of length >= 2.
fn pure_runs(instrs: &[Instr]) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let n = instrs.len();
    let mut i = 0;
    while i < n {
        if !superopt::is_eligible(&instrs[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && superopt::is_eligible(&instrs[i]) {
            i += 1;
        }
        if i - start >= 2 {
            runs.push((start, i - 1));
        }
    }
    runs
}

/// Total static gas of a block (sum of the eligible opcodes' costs).
fn block_gas(prog: &[Instr]) -> u32 {
    prog.iter().filter_map(|ins| gas(ins.mnem())).sum()
}

/// Apply every superopt rewrite (for tests/targeted runs); the CLI rewrites via the enabled config
/// through [`crate::features::optimize`].
#[allow(dead_code)] // feature's module API; the CLI rewrites via the orchestrator
pub fn optimize(instrs: &[Instr]) -> (Vec<Instr>, Vec<Span>) {
    let spans = scan(instrs);
    (apply_spans(instrs, &spans), spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{mnemonics, parse_str};

    #[test]
    fn add_zero_is_proven_identity() {
        // x + 0 == x: the engine proves `PUSH1 0 ADD` equivalent to the empty program and deletes
        // it (the cheapest equivalent), leaving the surrounding ops untouched.
        let p = parse_str("CALLDATALOAD PUSH1 0 ADD PUSH1 0 MSTORE");
        let (out, spans) = optimize(&p);
        assert_eq!(
            spans.len(),
            1,
            "the redundant +0 block was not recognized as identity"
        );
        assert_eq!(
            spans[0].category,
            Category::Superopt,
            "the span must carry the Superopt category"
        );
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD", "PUSH0", "MSTORE"],
            "the +0 must collapse, leaving the MSTORE offset push as PUSH0"
        );
    }

    #[test]
    fn double_not_cancels() {
        // NOT NOT x == x: proven equivalent to the empty program.
        let p = parse_str("CALLDATALOAD NOT NOT PUSH1 0 MSTORE");
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "NOT NOT was not proven to cancel");
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD", "PUSH0", "MSTORE"],
            "the cancelling NOT pair was not removed"
        );
    }

    #[test]
    fn mul_one_is_identity() {
        // x * 1 == x: `PUSH1 1 MUL` (gas 8) proven equivalent to nothing.
        let p = parse_str("CALLDATALOAD PUSH1 1 MUL PUSH1 0 MSTORE");
        let (_out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "x*1 was not proven to be identity");
    }

    #[test]
    fn non_redundant_block_kept() {
        // x + y is already optimal — there is no strictly-cheaper equivalent, so nothing is emitted.
        let p = parse_str("CALLDATALOAD DUP1 ADD PUSH1 0 MSTORE");
        let (_out, spans) = optimize(&p);
        assert!(
            spans.is_empty(),
            "a block with no cheaper equivalent was wrongly rewritten"
        );
    }

    #[test]
    fn side_effecting_run_is_ineligible() {
        // SSTORE is not eligible, so no pure run of length >= 2 forms around it.
        let p = parse_str("PUSH1 0 SSTORE");
        let (_out, spans) = optimize(&p);
        assert!(
            spans.is_empty(),
            "a run containing a side effect must not be superoptimized"
        );
    }
}
