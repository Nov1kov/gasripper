//! Optimization features. Each feature is one gas-reduction pass.
//!
//! Six passes ship today: [`guards`] (trusted-caller revert-guard removal, via the
//! [`crate::core::strip_guards`] engine) and the always-safe rewrites [`shuffle`]
//! (stack-shuffle rescheduling), [`involution`] (`NOT NOT` cancelling), [`recompute`]
//! (`OP DUP1` → `OP OP`), [`fold_shift`] (constant `SHL`/`SHR` precompute), and
//! [`cmpnorm`] (`SWAP1 LT` → `GT` comparison normalization). Each owns one [`Category`].
//! By default ALL features are enabled; they can be disabled via the CLI (`--disable`)
//! or a config file.
//!
//! [`optimize`] is the single entry point the CLI and e2e harness drive: it runs the
//! enabled passes over a program and returns the rewritten instructions plus every
//! applied edit [`Span`] (on original indices, for the sidecar to re-assemble).
//!
//! Each feature lives in its own module, exports a [`FeatureMeta`] and a thin
//! rewrite function, and keeps its own tests pinning down exactly what it changes.

use std::collections::HashSet;

use crate::core::asm::is_symbolic;
use crate::core::{Category, Instr, Span, apply_spans, strip_guards};

pub mod cmpnorm;
#[cfg(test)]
pub mod e2e_harness;
pub mod fold_shift;
pub mod guards;
pub mod inline;
pub mod involution;
mod progress;
#[cfg(test)]
mod progressive_e2e;
pub mod recompute;
pub mod shuffle;
#[cfg(feature = "smt")]
pub mod superopt;

/// Feature metadata for the registry, CLI, and config.
#[derive(Clone, Copy, Debug)]
pub struct FeatureMeta {
    /// Stable key (for `--disable`/config).
    pub key: &'static str,
    /// Human-readable name.
    pub name: &'static str,
    /// Short description of what the pass does.
    pub description: &'static str,
    /// The rewrite category this feature owns.
    pub category: Category,
    /// Whether enabled by default (currently — all enabled).
    pub default_enabled: bool,
}

/// The full registry of available features. The SMT block superoptimizer is present only in an
/// `smt`-feature build (it pulls in the Z3 dependency).
pub fn registry() -> Vec<FeatureMeta> {
    #[cfg_attr(not(feature = "smt"), allow(unused_mut))]
    let mut metas = vec![
        inline::META,
        guards::META,
        shuffle::META,
        involution::META,
        recompute::META,
        fold_shift::META,
        cmpnorm::META,
    ];
    #[cfg(feature = "smt")]
    metas.push(superopt::META);
    metas
}

/// Find a feature's metadata by key.
pub fn find(key: &str) -> Option<FeatureMeta> {
    registry().into_iter().find(|f| f.key == key)
}

/// Inclusive-range intersection of two spans.
#[inline]
fn overlaps(a: &Span, b: &Span) -> bool {
    a.start <= b.end && b.start <= a.end
}

/// Add each candidate span to `spans` unless it overlaps one already accepted, then
/// re-sort by start. Keeps the merged edit set non-overlapping (a later pass yields to
/// an earlier one on a conflict), so [`apply_spans`] can splice deterministically.
fn merge_nonoverlapping(spans: &mut Vec<Span>, candidates: Vec<Span>) {
    for span in candidates {
        if !spans.iter().any(|s| overlaps(&span, s)) {
            spans.push(span);
        }
    }
    spans.sort_by_key(|s| s.start);
}

/// Run the enabled passes over `instrs` with the default inline threshold
/// ([`inline::DEFAULT_MAX_BODY`]). See [`optimize_with`].
pub fn optimize(instrs: &[Instr], enabled: &HashSet<Category>) -> (Vec<Instr>, Vec<Span>) {
    optimize_with(instrs, enabled, inline::DEFAULT_MAX_BODY)
}

/// Run the enabled passes over `instrs`, returning the rewritten program and every applied edit
/// span (on original indices). `inline_max` bounds the body size the inline pass will relocate.
///
/// Inline runs FIRST so its definition-deletion and call-site spans take precedence: a later
/// pass cannot rewrite code that inline relocates or deletes. It (like the other length-changing
/// passes — stack-shuffle rescheduling, involution cancelling, shift-constant folding, and
/// comparison normalization) runs only on symbolic programs: it changes instruction lengths and
/// emits symbolic labels the sidecar/compiler relinks, which the concrete-bytecode path cannot
/// do. Guard removal runs next, then the remaining always-safe passes. Recompute is
/// length-preserving (one single-byte opcode for another), so it runs on every program —
/// including concrete bytecode. A later pass's span that overlaps one already accepted is
/// dropped, so the merged edit set stays non-overlapping.
pub fn optimize_with(
    instrs: &[Instr],
    enabled: &HashSet<Category>,
    inline_max: usize,
) -> (Vec<Instr>, Vec<Span>) {
    let mut spans: Vec<Span> = Vec::new();
    if is_symbolic(instrs) && enabled.contains(&Category::Inline) {
        merge_nonoverlapping(&mut spans, inline::scan(instrs, enabled, inline_max));
    }
    let (_, guard_spans) = strip_guards(instrs, enabled);
    merge_nonoverlapping(&mut spans, guard_spans);
    if is_symbolic(instrs) {
        if enabled.contains(&Category::Shuffle) {
            merge_nonoverlapping(&mut spans, shuffle::scan(instrs));
        }
        if enabled.contains(&Category::Involution) {
            merge_nonoverlapping(&mut spans, involution::scan(instrs));
        }
        if enabled.contains(&Category::FoldShift) {
            merge_nonoverlapping(&mut spans, fold_shift::scan(instrs));
        }
        if enabled.contains(&Category::CmpNorm) {
            merge_nonoverlapping(&mut spans, cmpnorm::scan(instrs));
        }
        #[cfg(feature = "smt")]
        if enabled.contains(&Category::Superopt) {
            merge_nonoverlapping(&mut spans, superopt::scan(instrs));
        }
    }
    if enabled.contains(&Category::Recompute) {
        merge_nonoverlapping(&mut spans, recompute::scan(instrs));
    }
    (apply_spans(instrs, &spans), spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::parse_str;

    fn only(c: Category) -> HashSet<Category> {
        [c].into_iter().collect()
    }

    #[test]
    fn shuffle_fires_on_symbolic_program() {
        // A symbolic label makes the program relinkable, so the reschedule runs.
        let p = parse_str("_sym_x JUMPDEST SWAP1 SWAP1 STOP");
        let (_out, spans) = optimize(&p, &only(Category::Shuffle));
        assert_eq!(
            spans.len(),
            1,
            "a reschedulable window in a symbolic program was not rewritten"
        );
        assert_eq!(
            spans[0].category,
            Category::Shuffle,
            "the span must carry the Shuffle category"
        );
    }

    #[test]
    fn shuffle_never_fires_on_concrete_bytecode() {
        // Same reschedulable window, but a fully concrete program: rewriting it would
        // shift JUMPDEST offsets with no linker to fix them, so the pass must skip it.
        let p = parse_str("PUSH1 1 SWAP1 SWAP1 PUSH1 2");
        assert!(
            !is_symbolic(&p),
            "the test program must be concrete for this invariant"
        );
        let (_out, spans) = optimize(&p, &only(Category::Shuffle));
        assert!(
            spans.is_empty(),
            "shuffle wrongly rewrote a concrete program (would break jumps)"
        );
    }

    #[test]
    fn involution_fires_on_symbolic_program() {
        // A symbolic label makes the program relinkable, so the NOT pair is cancelled.
        let p = parse_str("_sym_x JUMPDEST NOT NOT STOP");
        let (_out, spans) = optimize(&p, &only(Category::Involution));
        assert_eq!(
            spans.len(),
            1,
            "a cancelling NOT pair in a symbolic program was not removed"
        );
        assert_eq!(
            spans[0].category,
            Category::Involution,
            "the span must carry the Involution category"
        );
    }

    #[test]
    fn involution_never_fires_on_concrete_bytecode() {
        // A fully concrete program: cancelling the NOT pair would shift JUMPDEST offsets
        // with no linker to fix them, so the pass must skip it.
        let p = parse_str("PUSH1 1 NOT NOT PUSH1 2");
        assert!(
            !is_symbolic(&p),
            "the test program must be concrete for this invariant"
        );
        let (_out, spans) = optimize(&p, &only(Category::Involution));
        assert!(
            spans.is_empty(),
            "involution wrongly rewrote a concrete program (would break jumps)"
        );
    }

    #[test]
    fn cmpnorm_fires_on_symbolic_program() {
        // A symbolic label makes the program relinkable, so the SWAP1/LT folds to GT.
        let p = parse_str("_sym_x JUMPDEST SWAP1 LT STOP");
        let (_out, spans) = optimize(&p, &only(Category::CmpNorm));
        assert_eq!(
            spans.len(),
            1,
            "a foldable SWAP1/LT in a symbolic program was not rewritten"
        );
        assert_eq!(
            spans[0].category,
            Category::CmpNorm,
            "the span must carry the CmpNorm category"
        );
    }

    #[test]
    fn cmpnorm_never_fires_on_concrete_bytecode() {
        // Same foldable window, but a fully concrete program: folding it would shift
        // JUMPDEST offsets with no linker to fix them, so the pass must skip it.
        let p = parse_str("PUSH1 1 PUSH1 2 SWAP1 LT PUSH1 3");
        assert!(
            !is_symbolic(&p),
            "the test program must be concrete for this invariant"
        );
        let (_out, spans) = optimize(&p, &only(Category::CmpNorm));
        assert!(
            spans.is_empty(),
            "cmpnorm wrongly rewrote a concrete program (would break jumps)"
        );
    }
}
