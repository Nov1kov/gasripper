//! Optimization features. Each feature is one gas-reduction pass.
//!
//! Two passes ship today: [`guards`] (trusted-caller revert-guard removal, via the
//! [`crate::core::strip_guards`] engine) and [`shuffle`] (always-safe stack-shuffle
//! rescheduling). Each owns one [`Category`]. By default ALL features are enabled;
//! they can be disabled via the CLI (`--disable`) or a config file.
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

#[cfg(test)]
pub mod e2e_harness;
pub mod guards;
pub mod shuffle;

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

/// The full registry of available features.
pub fn registry() -> Vec<FeatureMeta> {
    vec![guards::META, shuffle::META]
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

/// Run the enabled passes over `instrs`, returning the rewritten program and every
/// applied edit span (on original indices).
///
/// Guard removal runs first. Stack-shuffle rescheduling runs only on symbolic
/// programs — it changes instruction lengths and relies on the sidecar/compiler to
/// relink jumps, which the concrete-bytecode path cannot do. A shuffle span that
/// overlaps a guard span is dropped, so the merged edit set stays non-overlapping.
pub fn optimize(instrs: &[Instr], enabled: &HashSet<Category>) -> (Vec<Instr>, Vec<Span>) {
    let (_, mut spans) = strip_guards(instrs, enabled);
    if enabled.contains(&Category::Shuffle) && is_symbolic(instrs) {
        for span in shuffle::scan(instrs) {
            if !spans.iter().any(|g| overlaps(&span, g)) {
                spans.push(span);
            }
        }
        spans.sort_by_key(|s| s.start);
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
        assert_eq!(spans.len(), 1, "a reschedulable window in a symbolic program was not rewritten");
        assert_eq!(spans[0].category, Category::Shuffle, "the span must carry the Shuffle category");
    }

    #[test]
    fn shuffle_never_fires_on_concrete_bytecode() {
        // Same reschedulable window, but a fully concrete program: rewriting it would
        // shift JUMPDEST offsets with no linker to fix them, so the pass must skip it.
        let p = parse_str("PUSH1 1 SWAP1 SWAP1 PUSH1 2");
        assert!(!is_symbolic(&p), "the test program must be concrete for this invariant");
        let (_out, spans) = optimize(&p, &only(Category::Shuffle));
        assert!(spans.is_empty(), "shuffle wrongly rewrote a concrete program (would break jumps)");
    }
}
