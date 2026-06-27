//! Feature `inline` — relocate a small internal function into its call sites.
//!
//! # What it optimizes
//!
//! Vyper's `venom` backend keeps every `@internal` function as a separate runtime block reached
//! by a call convention: `pushsym ret; pushsym entry; JUMP`, with the body returning by
//! `JUMP`ing back to `ret`. Each call therefore pays a fixed indirection — two pushes, the call
//! `JUMP`, and the entry `JUMPDEST` — on top of the body's own work. This pass splices the body
//! into each call site, dropping the `pushsym entry` and the call `JUMP` (and, once every call
//! site is inlined, deleting the now-unreachable function definition). The `pushsym ret` and the
//! body's return `JUMP` are kept verbatim, so the relocated body still returns to the same
//! continuation — no stack renumbering, which makes the rewrite a provable relocation.
//!
//! # First pass, and how it composes
//!
//! Inline runs FIRST in [`crate::features::optimize`] so its edits take precedence. Because the
//! other passes are independent scans of the ORIGINAL program and never see inline's output,
//! inline optimizes each relocated body itself: it runs the other enabled passes over the body
//! before copying it (via [`crate::features::optimize`] with `inline` removed). This both lets
//! the in-block folding the user expects actually happen and guarantees the inlined copy is at
//! least as optimized as the shared original would have been (so enabling inline never raises
//! gas). Each copy's internal labels are renamed uniquely so duplicating the body across several
//! call sites cannot collide.
//!
//! # Configurable threshold (the first numeric feature)
//!
//! Only functions whose body is at most a configurable number of instructions are inlined
//! (default [`DEFAULT_MAX_BODY`]); larger ones are left alone, since duplicating a big body
//! across many call sites grows the bytecode more than the saved indirection is worth. The
//! threshold is set via `--inline-max-body N` or `inline_max_body = N` in a config file.
//!
//! # Length-changing — symbolic (relinkable) input only
//!
//! Relocating and renaming a body changes instruction lengths and emits fresh symbolic labels,
//! so — like the other length-changing passes — it runs only on symbolic programs, where the
//! sidecar's compiler assembler relinks. The relocated body is carried to the sidecar one
//! instruction per edit token (see [`crate::core::asm::replacement_token`]); the sidecar stays
//! dumb and just splices.

use std::collections::{HashMap, HashSet};

use super::FeatureMeta;
use crate::core::asm::{Kind, replacement_token};
use crate::core::inline::{dethread_diamond, dethread_tail_return, find_inlinable};
use crate::core::{Category, Instr, Span};

#[cfg(test)]
mod e2e;

/// Default maximum body size (instructions) a function may have to be inlined.
pub const DEFAULT_MAX_BODY: usize = 20;

pub const META: FeatureMeta = FeatureMeta {
    key: "inline",
    name: "Inline",
    description: "relocate a small internal function into its call sites, removing the call/return indirection (configurable size via --inline-max-body; symbolic input only)",
    category: Category::Inline,
    default_enabled: true,
};

/// A [`Span`] per call site (replacing `pushsym entry; JUMP` with the relocated body) plus one
/// deleting each inlined function's definition. `enabled` selects which other passes optimize
/// the relocated body; `max_body` is the size threshold. Returns no spans on a non-symbolic
/// program (it has no relinkable labels).
pub fn scan(instrs: &[Instr], enabled: &HashSet<Category>, max_body: usize) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut copy_id = 0usize;
    for plan in find_inlinable(instrs, max_body) {
        let body = optimize_body(instrs, plan.body_start, plan.body_end, enabled);
        if let Some(dethreaded) = dethread_tail_return(&body) {
            // Straight-line tail-return: the return address is eliminated, so the call site's
            // `pushsym ret` (at site - 1) is dropped too and the body falls through to the
            // continuation. The body has no internal labels, so every copy is identical.
            let tokens: Vec<String> = dethreaded.iter().map(replacement_token).collect();
            for &site in &plan.call_sites {
                spans.push(Span {
                    start: site - 1,
                    end: site + 1,
                    category: Category::Inline,
                    replacement: tokens.clone(),
                });
            }
        } else if let Some(diamond) = dethread_diamond(&body) {
            // Single-merge diamond: the return address is eliminated and the two arms rejoin at a
            // fresh fall-through label, so (like tail-return) the call site's `pushsym ret` is
            // dropped. The body has internal labels (the branch arm + the join), renamed per copy.
            let internal = internal_labels(&diamond);
            for &site in &plan.call_sites {
                spans.push(Span {
                    start: site - 1,
                    end: site + 1,
                    category: Category::Inline,
                    replacement: render_body(&diamond, &internal, copy_id),
                });
                copy_id += 1;
            }
        } else {
            // Branching / non-tail body that is not a single-merge diamond: relocate it verbatim,
            // keeping the `pushsym ret` and the body's return JUMP. Internal labels renamed per copy.
            let internal = internal_labels(&body);
            for &site in &plan.call_sites {
                spans.push(Span {
                    start: site,
                    end: site + 1,
                    category: Category::Inline,
                    replacement: render_body(&body, &internal, copy_id),
                });
                copy_id += 1;
            }
        }
        spans.push(Span {
            start: plan.entry,
            end: plan.body_end,
            category: Category::Inline,
            replacement: Vec::new(),
        });
    }
    spans
}

/// The function body, after running the other enabled passes over it in isolation. `inline` is
/// removed from the set so a body (which carries no nested internal function anyway) is never
/// re-entered.
fn optimize_body(
    instrs: &[Instr],
    start: usize,
    end: usize,
    enabled: &HashSet<Category>,
) -> Vec<Instr> {
    let slice = instrs[start..=end].to_vec();
    let mut sub = enabled.clone();
    sub.remove(&Category::Inline);
    let (optimized, _) = super::optimize(&slice, &sub);
    optimized
}

/// The set of label symbols defined inside an (already optimized) body.
fn internal_labels(body: &[Instr]) -> HashSet<String> {
    body.iter()
        .filter(|ins| ins.kind == Kind::Label && ins.tokens.len() > 1)
        .map(|ins| ins.tokens[0].clone())
        .collect()
}

/// Encode `body` as replacement tokens for one call site, renaming its internal labels with
/// `copy_id` so duplicate copies across call sites never collide.
fn render_body(body: &[Instr], internal: &HashSet<String>, copy_id: usize) -> Vec<String> {
    let map = rename_map(internal, copy_id);
    body.iter()
        .map(|ins| replacement_token(&rename(ins, &map)))
        .collect()
}

/// Map each internal label to a fresh, copy-unique, side-effect-free name (`_sym_inl<copy>_<k>`).
/// A synthetic name is required because a venom internal label can itself carry spaces and commas
/// (e.g. `_sym_internal 0 _f(uint256,uint256)_cleanup`), which the edit serialization cannot hold.
fn rename_map(internal: &HashSet<String>, copy_id: usize) -> HashMap<String, String> {
    let mut names: Vec<&String> = internal.iter().collect();
    names.sort();
    names
        .into_iter()
        .enumerate()
        .map(|(k, sym)| (sym.clone(), format!("_sym_inl{copy_id}_{k}")))
        .collect()
}

/// Clone `ins`, rewriting an internal label definition or reference through `map`.
fn rename(ins: &Instr, map: &HashMap<String, String>) -> Instr {
    match ins.kind {
        Kind::PushSym => match map.get(ins.mnem()) {
            Some(name) => Instr::new(Kind::PushSym, vec![name.clone()]),
            None => ins.clone(),
        },
        Kind::Label if ins.tokens.len() > 1 => match map.get(&ins.tokens[0]) {
            Some(name) => Instr::new(Kind::Label, vec![name.clone(), "JUMPDEST".into()]),
            None => ins.clone(),
        },
        _ => ins.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apply_spans;
    use crate::core::asm::{parse_str, render};

    fn all() -> HashSet<Category> {
        [Category::Inline].into_iter().collect()
    }

    // A two-call-site internal function with an internal branch label, so inlining must both
    // remove the call indirection and rename the body's label per copy. parse_str cannot encode
    // venom's space-bearing entry symbol, so the single-token `_sym_internal_f` stands in.
    const TWO_CALLS: &str = "\
        _sym_ret0 _sym_internal_f_runtime JUMP _sym_ret0 JUMPDEST \
        _sym_ret1 _sym_internal_f_runtime JUMP _sym_ret1 JUMPDEST STOP \
        _sym_internal_f_runtime JUMPDEST DUP1 _sym_skip JUMPI POP _sym_skip JUMPDEST JUMP";

    #[test]
    fn inlines_into_every_call_site_and_deletes_the_definition() {
        // Two call sites -> two body copies + one definition deletion = 3 spans.
        let p = parse_str(TWO_CALLS);
        let spans = scan(&p, &all(), 20);
        assert_eq!(
            spans.len(),
            3,
            "expected a span per call site plus the definition deletion"
        );
        assert!(
            spans.iter().all(|s| s.category == Category::Inline),
            "every emitted span must carry the Inline category"
        );
        let out = render(&apply_spans(&p, &spans));
        assert!(
            !out.contains("_sym_internal_f"),
            "the function definition and its call targets must be gone after inlining"
        );
    }

    #[test]
    fn body_labels_are_renamed_uniquely_per_copy() {
        // The body's `_sym_skip` label must become a distinct name in each copy so the two
        // inlined bodies do not define the same label twice.
        let p = parse_str(TWO_CALLS);
        let spans = scan(&p, &all(), 20);
        let out = render(&apply_spans(&p, &spans));
        assert!(
            out.contains("_sym_inl0_0"),
            "the first copy's internal label was not renamed"
        );
        assert!(
            out.contains("_sym_inl1_0"),
            "the second copy's internal label was not renamed"
        );
        assert!(
            !out.contains("_sym_skip"),
            "the original internal label name must not survive"
        );
    }

    #[test]
    fn over_threshold_function_is_left_untouched() {
        // With a threshold below the body size nothing is inlined.
        let p = parse_str(TWO_CALLS);
        assert!(
            scan(&p, &all(), 2).is_empty(),
            "a body over the threshold was wrongly inlined"
        );
    }
}
