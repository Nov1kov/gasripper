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

use super::{FeatureMeta, progress};
use crate::core::opcodes::gas;
use crate::core::superopt;
pub use crate::core::superopt::Limits;
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

/// A [`Span`] for every maximal pure run the engine can prove a strictly-cheaper equivalent for,
/// searching under the given [`Limits`].
///
/// Emits `tracing` progress (visible at the default `info` level, since each block can take up to the
/// per-block solver timeout): one line per block as it is analyzed, one per proven rewrite with its
/// block-gas delta, and a final summary. Set `RUST_LOG=warn` to silence it.
pub fn scan(instrs: &[Instr], limits: &Limits) -> Vec<Span> {
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
        if let Some(better) = superopt::optimize_block(run, limits) {
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

/// Apply every superopt rewrite under the default [`Limits`] (for tests/targeted runs); the CLI
/// rewrites via the enabled config through [`crate::features::optimize`].
#[allow(dead_code)] // feature's module API; the CLI rewrites via the orchestrator
pub fn optimize(instrs: &[Instr]) -> (Vec<Instr>, Vec<Span>) {
    let spans = scan(instrs, &Limits::default());
    (apply_spans(instrs, &spans), spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{Kind, mnemonics, parse_str};

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
        // x + x followed by a non-zero offset push is already optimal — there is no
        // strictly-cheaper equivalent, so nothing is emitted.
        let p = parse_str("CALLDATALOAD DUP1 ADD PUSH1 32 MSTORE");
        let (_out, spans) = optimize(&p);
        assert!(
            spans.is_empty(),
            "a block with no cheaper equivalent was wrongly rewritten"
        );
    }

    #[test]
    fn length_three_candidate_synthesized() {
        // Three zero pushes need a three-instruction replacement (each PUSH0 nets +1 stack): the
        // search must reach candidate length 3 to swap `PUSH1 0` x3 (9 gas) for `PUSH0` x3 (6 gas).
        let p = parse_str("PUSH1 0 PUSH1 0 PUSH1 0");
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the three-push run was not rewritten");
        assert_eq!(
            mnemonics(&out),
            vec!["PUSH0", "PUSH0", "PUSH0"],
            "the cheapest three-word equivalent must be three PUSH0s"
        );
    }

    #[test]
    fn run_longer_than_sixteen_optimized() {
        // A 19-instruction pure run (nine `PUSH1 0 ADD` pairs plus the MSTORE offset push) must not
        // be skipped by the block-length bound: it collapses to a single `PUSH0`.
        let p = parse_str(
            "CALLDATALOAD \
             PUSH1 0 ADD PUSH1 0 ADD PUSH1 0 ADD PUSH1 0 ADD PUSH1 0 ADD \
             PUSH1 0 ADD PUSH1 0 ADD PUSH1 0 ADD PUSH1 0 ADD \
             PUSH1 0 MSTORE",
        );
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the long redundant run was skipped");
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD", "PUSH0", "MSTORE"],
            "the long +0 chain must collapse to one PUSH0"
        );
    }

    #[test]
    fn div_by_zero_folds_to_zero() {
        // x / 0 == 0 on the EVM (the SMT-LIB default for bvudiv by zero is all-ones): the engine
        // must prove `PUSH1 0 SWAP1 DIV` always leaves 0 and replace it with `POP PUSH0`.
        let p = parse_str("CALLDATALOAD PUSH1 0 SWAP1 DIV");
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the division by zero was not folded");
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD", "POP", "PUSH0"],
            "x/0 must fold to dropping x and pushing 0"
        );
    }

    #[test]
    fn mod_by_zero_folds_to_zero() {
        // x mod 0 == 0 on the EVM, while SMT-LIB bvurem by zero returns the dividend — this fold
        // is provable only with the explicit zero-divisor guard.
        let p = parse_str("CALLDATALOAD PUSH1 0 SWAP1 MOD");
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the modulo by zero was not folded");
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD", "POP", "PUSH0"],
            "x mod 0 must fold to dropping x and pushing 0"
        );
    }

    #[test]
    fn sdiv_by_one_is_identity() {
        // x sdiv 1 == x for every x including the most negative word: the signed division block
        // is proven equivalent to the empty program.
        let p = parse_str("CALLDATALOAD PUSH1 1 SWAP1 SDIV");
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "x sdiv 1 was not proven to be identity");
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD"],
            "the sdiv-by-one block must vanish"
        );
    }

    #[test]
    fn smod_by_one_folds_to_zero() {
        // x smod 1 == 0 for every x (the remainder magnitude is |x| mod 1).
        let p = parse_str("CALLDATALOAD PUSH1 1 SWAP1 SMOD");
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "x smod 1 was not folded");
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD", "POP", "PUSH0"],
            "x smod 1 must fold to dropping x and pushing 0"
        );
    }

    #[test]
    fn signed_self_compare_folds_to_zero() {
        // x < x and x > x are false under the signed comparators too.
        for op in ["SLT", "SGT"] {
            let p = parse_str(&format!("CALLDATALOAD DUP1 {op}"));
            let (out, spans) = optimize(&p);
            assert_eq!(spans.len(), 1, "the self-{op} was not folded");
            assert_eq!(
                mnemonics(&out),
                vec!["CALLDATALOAD", "POP", "PUSH0"],
                "self-{op} must fold to dropping x and pushing 0"
            );
        }
    }

    #[test]
    fn byte_beyond_range_folds_to_zero() {
        // BYTE with index 32 reads past the most significant byte and is 0 by definition.
        let p = parse_str("CALLDATALOAD PUSH1 32 BYTE");
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the out-of-range BYTE was not folded");
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD", "POP", "PUSH0"],
            "BYTE past index 31 must fold to dropping x and pushing 0"
        );
    }

    #[test]
    fn sar_zero_shift_is_identity() {
        // x sar 0 == x: the arithmetic shift block is proven equivalent to the empty program.
        let p = parse_str("CALLDATALOAD PUSH1 0 SAR");
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the zero-shift SAR was not removed");
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD"],
            "the zero-shift SAR block must vanish"
        );
    }

    #[test]
    fn signextend_full_width_is_identity() {
        // SIGNEXTEND from byte 31 extends from the word's own sign bit, i.e. changes nothing.
        let p = parse_str("CALLDATALOAD PUSH1 31 SIGNEXTEND");
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the full-width SIGNEXTEND was not removed");
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD"],
            "the full-width SIGNEXTEND block must vanish"
        );
    }

    #[test]
    fn signextend_is_idempotent() {
        // Extending twice from the same byte equals extending once — provable only if the
        // bit-level mask/sign encoding is faithful.
        let p = parse_str("CALLDATALOAD PUSH1 0 SIGNEXTEND PUSH1 0 SIGNEXTEND");
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the doubled SIGNEXTEND was not collapsed");
        assert_eq!(
            mnemonics(&out),
            vec!["CALLDATALOAD", "PUSH0", "SIGNEXTEND"],
            "the doubled SIGNEXTEND must collapse to a single extension"
        );
    }

    #[test]
    fn addmod_reduces_full_width_intermediate() {
        // addmod(MAX, MAX, 3): the true 512-bit sum 2^257-2 is divisible by 3, so the result is 0.
        // A mod-2^256-truncating encoding would compute (2^256-2) mod 3 == 2 and refute the fold.
        let max = format!("0x{}", "ff".repeat(32));
        let p = parse_str(&format!("PUSH1 3 PUSH32 {max} PUSH32 {max} ADDMOD"));
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the constant ADDMOD was not folded");
        assert_eq!(
            mnemonics(&out),
            vec!["PUSH0"],
            "addmod(MAX, MAX, 3) must fold to zero via the 512-bit intermediate"
        );
    }

    #[test]
    fn mulmod_reduces_full_width_intermediate() {
        // mulmod(MAX, MAX, 3): MAX is divisible by 3, so the true product mod 3 is 0. A truncating
        // encoding would compute (MAX*MAX mod 2^256) mod 3 == 1 and refute the fold.
        let max = format!("0x{}", "ff".repeat(32));
        let p = parse_str(&format!("PUSH1 3 PUSH32 {max} PUSH32 {max} MULMOD"));
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the constant MULMOD was not folded");
        assert_eq!(
            mnemonics(&out),
            vec!["PUSH0"],
            "mulmod(MAX, MAX, 3) must fold to zero via the 512-bit intermediate"
        );
    }

    #[test]
    fn sar_idempotent_solc_shape_collapsed() {
        // The exact shape solc 0.8.24 --optimize emits for `(a >> 255) >> 255` (stack on entry:
        // [ret, a]): the doubled arithmetic shift is idempotent. The search discovers the
        // even-shorter `DUP1 SAR` — sar(x, x) equals sar(sar(x, 255), 255) on every input (a
        // negative x makes the shift saturate to -1, a non-negative x always shifts to 0).
        let p = parse_str("JUMPDEST PUSH1 255 SWAP1 DUP2 SAR SWAP1 SAR SWAP1 JUMP");
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the doubled SAR was not collapsed");
        assert_eq!(
            mnemonics(&out),
            vec!["JUMPDEST", "DUP1", "SAR", "SWAP1", "JUMP"],
            "the doubled SAR must collapse to the sar(x, x) form plus the return swap"
        );
    }

    #[test]
    fn mulmod_by_one_solc_shape_collapsed() {
        // The exact body solc 0.8.24 --optimize emits for `mulmod(a + b, a - b, 1)` in unchecked
        // code (stack on entry: [ret, a, b]): anything mod 1 is 0, so the 14-op block reduces to
        // dropping both arguments and returning 0 — a four-instruction replacement around the
        // threaded return address.
        let p = parse_str(
            "JUMPDEST PUSH0 PUSH1 1 DUP3 DUP5 SUB DUP4 DUP6 ADD MULMOD \
             SWAP4 SWAP3 POP POP POP JUMP",
        );
        let (out, spans) = optimize(&p);
        assert_eq!(spans.len(), 1, "the mulmod-by-one body was not collapsed");
        assert_eq!(
            mnemonics(&out),
            vec!["JUMPDEST", "POP", "POP", "PUSH0", "SWAP1", "JUMP"],
            "mulmod by one must reduce to dropping the arguments and pushing 0"
        );
    }

    #[test]
    fn solc_bare_push_block_optimized() {
        // The solc asm-json frontend emits a literal push as mnemonic `PUSH` with no width suffix
        // (the assembler picks the width): such a block must still be priced and optimized, not
        // silently skipped. Same doubled-SAR shape as above, with the push in solc's dump form.
        let mut p = parse_str("JUMPDEST");
        p.push(Instr::new(Kind::Push, vec!["PUSH".into(), "0xff".into()]));
        p.extend(parse_str("SWAP1 DUP2 SAR SWAP1 SAR SWAP1 JUMP"));
        let (out, spans) = optimize(&p);
        assert_eq!(
            spans.len(),
            1,
            "a block containing a solc bare-PUSH literal was skipped"
        );
        assert_eq!(
            mnemonics(&out),
            vec!["JUMPDEST", "DUP1", "SAR", "SWAP1", "JUMP"],
            "the solc-form block must collapse like the parse_str form"
        );
    }

    #[test]
    fn symbolic_push_breaks_run_but_neighbors_optimized() {
        // A solc `PUSH [tag]` reaches the engine as a push with NO immediate: it must end the pure
        // run (its value is link-time) instead of poisoning it, so the eligible tail right after
        // it is still optimized.
        let mut p = parse_str("JUMPDEST");
        p.push(Instr::new(Kind::Push, vec!["PUSH".into()]));
        p.extend(parse_str("NOT NOT PUSH1 0 MSTORE"));
        let (out, spans) = optimize(&p);
        assert_eq!(
            spans.len(),
            1,
            "the run next to a symbolic push was not optimized"
        );
        assert_eq!(
            mnemonics(&out),
            vec!["JUMPDEST", "PUSH", "PUSH0", "MSTORE"],
            "the NOT NOT after the symbolic push must still cancel"
        );
    }

    #[test]
    fn limits_bound_the_search() {
        // With the synthesis length capped at 2, the three-PUSH0 rewrite (which needs a
        // three-instruction candidate) is out of reach and the run must stay untouched.
        let p = parse_str("PUSH1 0 PUSH1 0 PUSH1 0");
        let tight = Limits {
            max_synth: 2,
            ..Limits::default()
        };
        assert!(
            scan(&p, &tight).is_empty(),
            "a synth-length-2 search must not reach the three-instruction rewrite"
        );
        assert_eq!(
            scan(&p, &Limits::default()).len(),
            1,
            "the default limits must still find the rewrite"
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
