//! Feature `shuffle` — stack-shuffle minimization.
//!
//! # What it optimizes
//!
//! A maximal run of pure stack-scheduling ops (`POP` / `DUPn` / `SWAPn`) that a
//! compiler's stack scheduler left non-minimal: it is replaced by the cheapest
//! equivalent run producing the identical stack. No value is computed and no
//! memory/storage is touched, so the rewrite is **always safe** — it needs no
//! trusted caller, unlike [`crate::features::guards`].
//!
//! # Why it fires at all
//!
//! Vyper's `venom` backend already minimizes most stack juggling, but where several
//! independent subexpressions merge through a commutative/associative reduction
//! (`a | b | c`, `a + b + c`) it leaves windows like `SWAP1 DUP2 SWAP1 DUP1 SWAP3`
//! that are a five-op way to write `DUP2 DUP2` (15 gas → 6). Measured on real
//! Vyper 0.4.3 output, these recur often enough to be worth a pass.
//!
//! # How it is sound
//!
//! Stack ops move/copy/drop slots by position without inspecting values, so two
//! windows are equivalent iff they map an all-distinct stack identically. The engine
//! ([`crate::core::stack::minimize_shuffle`]) computes the window's net effect at its
//! minimal safe depth, searches for a strictly cheaper realizing sequence, and emits
//! it only when one exists — so a rewrite can never raise gas and never disturbs a
//! live value below the window.
//!
//! # Symbolic only
//!
//! Length-changing rewrites shift `JUMPDEST` offsets, and gasripper has no linker, so
//! this pass runs **only** on symbolic programs (the Vyper/Solidity sidecar path,
//! where the compiler relinks). The orchestrator ([`crate::features::optimize`]) gates
//! it on [`crate::core::asm::is_symbolic`].

use std::time::Instant;

use super::FeatureMeta;
use crate::core::asm::Kind;
use crate::core::stack::{is_shuffle, minimize_shuffle_counted, reschedule_estimate};
use crate::core::{Category, Instr, Span, apply_spans};

/// Below this window count the pass is silent (small inputs finish instantly); at or
/// above it, it logs an up-front work estimate and periodic progress.
const PROGRESS_MIN_WINDOWS: usize = 64;

/// Emit a progress line every this many processed windows.
const PROGRESS_STEP: usize = 50;

/// Rough search-steps-per-second for the up-front time estimate (debug build, order
/// of magnitude only — the live progress ETA refines it).
const STEPS_PER_SEC: f64 = 1_000_000.0;

#[cfg(test)]
mod e2e;

pub const META: FeatureMeta = FeatureMeta {
    key: "shuffle",
    name: "Stack shuffle",
    description: "reschedule DUP/SWAP/POP windows to a cheaper equivalent (always safe)",
    category: Category::Shuffle,
    default_enabled: true,
};

/// `instrs[i]` is a pure stack-scheduling op (`POP`/`DUPn`/`SWAPn`).
#[inline]
fn is_window_op(ins: &Instr) -> bool {
    ins.kind == Kind::Op && is_shuffle(ins.mnem())
}

/// The `[start, end]` ranges of every maximal pure-stack window of length >= 2.
/// Windows never cross a non-stack op or a label (those break the run), so each is
/// basic-block-local and safe to rewrite in isolation.
fn collect_windows(instrs: &[Instr]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let n = instrs.len();
    let mut i = 0;
    while i < n {
        if !is_window_op(&instrs[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && is_window_op(&instrs[i]) {
            i += 1;
        }
        if i - start >= 2 {
            out.push((start, i - 1));
        }
    }
    out
}

/// Up-front work estimate for a large program: how many windows are searchable, the
/// rough total search steps / time, and how many are too deep to optimize (their
/// brute-force size is reported, not attempted).
fn log_estimate(instrs: &[Instr], windows: &[(usize, usize)]) {
    let mut searchable = 0usize;
    let mut deep = 0usize;
    let mut steps = 0f64;
    let mut worst = 0f64;
    for &(s, e) in windows {
        let (_depth, feasible, est) = reschedule_estimate(&instrs[s..=e]);
        if feasible {
            searchable += 1;
            steps += est;
        } else {
            deep += 1;
            worst = worst.max(est);
        }
    }
    if deep > 0 {
        tracing::info!(
            "shuffle: {} windows — {searchable} searchable (~{:.0}k steps, ~{:.1}s), \
             {deep} too deep to optimize (brute force ~{:.0e} ops each, skipped)",
            windows.len(), steps / 1000.0, steps / STEPS_PER_SEC, worst,
        );
    } else {
        tracing::info!(
            "shuffle: {} windows, ~{:.0}k search steps (~{:.1}s)",
            windows.len(), steps / 1000.0, steps / STEPS_PER_SEC,
        );
    }
}

/// Find every maximal pure-stack window and, for each one the engine can make cheaper,
/// a [`Span`] replacing it with the minimal equivalent sequence. On a large program it
/// logs an up-front estimate and periodic progress (a window deeper than the search
/// bound is skipped, not optimized — see [`crate::core::stack::minimize_shuffle_counted`]).
pub fn scan(instrs: &[Instr]) -> Vec<Span> {
    let windows = collect_windows(instrs);
    if windows.is_empty() {
        return Vec::new();
    }
    let report = windows.len() >= PROGRESS_MIN_WINDOWS;
    if report {
        log_estimate(instrs, &windows);
    }

    let clock = Instant::now();
    let mut spans = Vec::new();
    let mut steps_done = 0u64;
    let mut skipped = 0usize;
    for (idx, &(start, end)) in windows.iter().enumerate() {
        let (replacement, steps) = minimize_shuffle_counted(&instrs[start..=end]);
        steps_done += steps as u64;
        if let Some(replacement) = replacement {
            spans.push(Span { start, end, category: Category::Shuffle, replacement });
        } else if steps == 0 {
            // (None, 0) means the window was too deep and skipped without searching.
            skipped += 1;
        }
        if report && (idx + 1) % PROGRESS_STEP == 0 {
            let frac = (idx + 1) as f64 / windows.len() as f64;
            let eta = clock.elapsed().mul_f64((1.0 - frac) / frac);
            tracing::info!(
                "shuffle: {}/{} windows ({:.0}%), {} rescheduled, {} steps, ~{:.1}s left",
                idx + 1, windows.len(), frac * 100.0, spans.len(), steps_done, eta.as_secs_f64(),
            );
        }
    }
    if report {
        tracing::info!(
            "shuffle: {} windows in {:.2}s — {} rescheduled, {} too deep (skipped), {} search steps",
            windows.len(), clock.elapsed().as_secs_f64(), spans.len(), skipped, steps_done,
        );
    }
    spans
}

/// Rewrite every reschedulable window (for tests/targeted runs); the CLI rewrites
/// via the enabled config through [`crate::features::optimize`].
#[allow(dead_code)] // feature's module API; the CLI reschedules via the orchestrator
pub fn reschedule(instrs: &[Instr]) -> (Vec<Instr>, Vec<Span>) {
    let spans = scan(instrs);
    (apply_spans(instrs, &spans), spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{mnemonics, parse_str};

    #[test]
    fn venom_window_is_rescheduled() {
        // The recurring real-Vyper leftover collapses to its two-op equivalent.
        let p = parse_str("SWAP1 DUP2 SWAP1 DUP1 SWAP3");
        let (out, spans) = reschedule(&p);
        assert_eq!(spans.len(), 1, "the non-minimal shuffle window was not rescheduled");
        let gas_before = 15u64;
        let gas_after: u64 = out
            .iter()
            .map(|i| if i.mnem() == "POP" { 2 } else { 3 })
            .sum();
        assert!(gas_after < gas_before, "the reschedule did not lower gas: {gas_after} >= {gas_before}");
    }

    #[test]
    fn self_cancel_window_deleted() {
        // SWAP1 SWAP1 is an identity — the whole window is deleted.
        let p = parse_str("SWAP1 SWAP1");
        let (out, spans) = reschedule(&p);
        assert_eq!(spans.len(), 1, "a self-cancelling swap pair was not rescheduled");
        assert!(mnemonics(&out).is_empty(), "the identity window was not deleted: {:?}", mnemonics(&out));
    }

    #[test]
    fn window_broken_by_non_stack_op() {
        // A non-stack op splits the run into two short windows; neither (DUP1 / POP
        // alone) is reschedulable, so nothing fires across the ADD.
        let p = parse_str("DUP1 ADD POP");
        let (_out, spans) = reschedule(&p);
        assert!(spans.is_empty(), "a window was wrongly grown across a non-stack op");
    }

    #[test]
    fn window_does_not_cross_label() {
        // A JUMPDEST label ends a basic block; a window may not span it.
        let p = parse_str("DUP2 _sym_x JUMPDEST POP");
        let spans = scan(&p);
        assert!(spans.is_empty(), "a shuffle window wrongly crossed a label boundary");
    }

    #[test]
    fn already_minimal_window_untouched() {
        // DUP2 DUP2 is already the cheapest sequence for its effect.
        let p = parse_str("DUP2 DUP2");
        let (_out, spans) = reschedule(&p);
        assert!(spans.is_empty(), "an already-minimal window was needlessly rewritten");
    }
}
