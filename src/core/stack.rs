//! Stack simulation over slot-ids — the heart of the safe-removal criterion.
//!
//! Port of `simulate_identity` from Python: run a sequence of instructions over
//! abstract stack slots. If the input stack is UNCHANGED after the run, the
//! sequence is an identity and can be cut out entirely without touching "live"
//! values. DUP/SWAP are modeled exactly (read/reorder); other opcodes via
//! (pops, pushes).

use super::asm::{Instr, Kind};
use super::opcodes::arity;

/// True if simulating `run` leaves the input stack UNCHANGED (an identity).
///
/// Kept as the canonical, simplest statement of the criterion (and exercised by
/// tests); the strip engine now uses the more general [`strip_residue`].
#[allow(dead_code)]
pub fn simulate_identity(run: &[Instr]) -> bool {
    // Base: 64 "live" slots with unique negative ids.
    let base: Vec<i64> = (-64..0).collect();
    let mut stack = base.clone();
    let mut nxt: i64 = 1;

    for ins in run {
        // Labels and raw data break the analysis — conservatively "not an identity".
        if matches!(ins.kind, Kind::Label | Kind::Raw) {
            return false;
        }
        let m = ins.mnem();

        if ins.kind == Kind::Op {
            if let Some(d) = m.strip_prefix("DUP").and_then(|s| s.parse::<usize>().ok()) {
                if d == 0 || stack.len() < d {
                    return false;
                }
                let v = stack[stack.len() - d];
                stack.push(v);
                continue;
            }
            if let Some(d) = m.strip_prefix("SWAP").and_then(|s| s.parse::<usize>().ok()) {
                if d == 0 || stack.len() < d + 1 {
                    return false;
                }
                let n = stack.len();
                stack.swap(n - 1, n - 1 - d);
                continue;
            }
        }

        let (pops, pushes) = match instr_arity(ins) {
            Some(a) => a,
            None => return false,
        };
        if stack.len() < pops {
            return false;
        }
        for _ in 0..pops {
            stack.pop();
        }
        for _ in 0..pushes {
            stack.push(nxt);
            nxt += 1;
        }
    }

    stack == base
}

/// The safe REPLACEMENT for cutting a guard run `<...> JUMPI`.
///
/// Generalizes [`simulate_identity`]: a guard can be removed if its fall-through
/// stack effect (the "residue") consists only of INPUT slots — i.e. the run creates
/// no value that survives into live code (the only created value, the branch
/// condition, is consumed by `JUMPI`). Then removing the revert is behavior-neutral
/// for the live path as long as we reproduce that residue with a minimal `POP`/`SWAP`
/// shuffle.
///
/// Returns:
///   * `Some([])`        — a pure stack identity (delete the run entirely);
///   * `Some([ops..])`   — a consuming check; replace the run with these stack ops;
///   * `None`            — the run produces a surviving computed value (live
///                         computation — keep it) or the residue needs duplication.
pub fn strip_residue(run: &[Instr]) -> Option<Vec<String>> {
    let base: Vec<i64> = (-64..0).collect();
    let mut stack = base.clone();
    let mut nxt: i64 = 1;

    for ins in run {
        if matches!(ins.kind, Kind::Label | Kind::Raw) {
            return None;
        }
        let m = ins.mnem();
        if ins.kind == Kind::Op {
            if let Some(d) = m.strip_prefix("DUP").and_then(|s| s.parse::<usize>().ok()) {
                if d == 0 || stack.len() < d {
                    return None;
                }
                let v = stack[stack.len() - d];
                stack.push(v);
                continue;
            }
            if let Some(d) = m.strip_prefix("SWAP").and_then(|s| s.parse::<usize>().ok()) {
                if d == 0 || stack.len() < d + 1 {
                    return None;
                }
                let n = stack.len();
                stack.swap(n - 1, n - 1 - d);
                continue;
            }
        }
        let (pops, pushes) = instr_arity(ins)?;
        if stack.len() < pops {
            return None;
        }
        for _ in 0..pops {
            stack.pop();
        }
        for _ in 0..pushes {
            stack.push(nxt);
            nxt += 1;
        }
    }

    // Untouched deep prefix stays; the disturbed top window is `source`, its residue `target`.
    let mut p = 0;
    while p < base.len() && p < stack.len() && base[p] == stack[p] {
        p += 1;
    }
    let source: Vec<i64> = base[p..].to_vec();
    let target: Vec<i64> = stack[p..].to_vec();

    // Every surviving slot must be an input slot we touched — no created value escapes.
    if target.iter().any(|t| !source.contains(t)) {
        return None;
    }
    synth_pop_swap(&source, &target)
}

/// Find the shortest `POP`/`SWAP` sequence turning stack window `source` into `target`
/// (a reordered subsequence of `source`). No `DUP` — duplication is refused (None).
fn synth_pop_swap(source: &[i64], target: &[i64]) -> Option<Vec<String>> {
    use std::collections::{HashSet, VecDeque};
    if source == target {
        return Some(Vec::new());
    }
    let mut seen: HashSet<Vec<i64>> = HashSet::new();
    let mut q: VecDeque<(Vec<i64>, Vec<String>)> = VecDeque::new();
    seen.insert(source.to_vec());
    q.push_back((source.to_vec(), Vec::new()));
    while let Some((st, ops)) = q.pop_front() {
        if ops.len() >= 12 {
            continue;
        }
        // POP the top.
        if !st.is_empty() {
            let mut s2 = st.clone();
            s2.pop();
            let mut o2 = ops.clone();
            o2.push("POP".to_string());
            if s2 == target {
                return Some(o2);
            }
            if seen.insert(s2.clone()) {
                q.push_back((s2, o2));
            }
        }
        // SWAPk: swap the top with the k-th element below it.
        let n = st.len();
        for k in 1..n {
            let mut s2 = st.clone();
            s2.swap(n - 1, n - 1 - k);
            if seen.contains(&s2) {
                continue;
            }
            let mut o2 = ops.clone();
            o2.push(format!("SWAP{k}"));
            if s2 == target {
                return Some(o2);
            }
            seen.insert(s2.clone());
            q.push_back((s2, o2));
        }
    }
    None
}

/// EVM gas of the pure stack-scheduling opcodes (stable costs): `POP` is `base`,
/// `DUPn`/`SWAPn` are `verylow`.
const GAS_POP: u64 = 2;
const GAS_VERYLOW: u64 = 3;

/// The deepest stack slot a single DUP/SWAP can reach (DUP16/SWAP16 read 17 slots).
const MAX_SHUFFLE_DEPTH: usize = 17;

/// Node budget for the reschedule search per window — a backstop so a single window
/// can never run away. The bounded depth below keeps the real state space well under
/// this, so it rarely bites; it only caps a degenerate case.
const SEARCH_NODES: u32 = 60_000;

/// Maximum window input depth the rescheduler will search. The state space grows
/// roughly factorially in depth, so beyond this a window is computationally infeasible
/// to optimize — and real codegen wins are shallow (every venom shuffle observed in a
/// multi-contract sweep is depth <= 6). Deeper windows are skipped, not searched. See
/// [`reschedule_estimate`].
const MAX_RESCHEDULE_DEPTH: usize = 6;

/// A pure stack-scheduling opcode — the only ops a shuffle window may contain.
#[derive(Clone, Copy)]
enum Shuffle {
    Pop,
    Dup(usize),
    Swap(usize),
}

impl Shuffle {
    #[inline]
    fn gas(self) -> u64 {
        match self {
            Shuffle::Pop => GAS_POP,
            _ => GAS_VERYLOW,
        }
    }

    fn mnem(self) -> String {
        match self {
            Shuffle::Pop => "POP".to_string(),
            Shuffle::Dup(d) => format!("DUP{d}"),
            Shuffle::Swap(d) => format!("SWAP{d}"),
        }
    }

    /// Apply to `stack`, or `None` on underflow.
    fn apply(self, stack: &[i64]) -> Option<Vec<i64>> {
        let mut s = stack.to_vec();
        match self {
            Shuffle::Pop => {
                s.pop()?;
            }
            Shuffle::Dup(d) => {
                if s.len() < d {
                    return None;
                }
                let v = s[s.len() - d];
                s.push(v);
            }
            Shuffle::Swap(d) => {
                if s.len() < d + 1 {
                    return None;
                }
                let n = s.len();
                s.swap(n - 1, n - 1 - d);
            }
        }
        Some(s)
    }
}

/// Classify a mnemonic as a pure stack op, or `None` for anything else.
fn as_shuffle(m: &str) -> Option<Shuffle> {
    if m == "POP" {
        return Some(Shuffle::Pop);
    }
    if let Some(d) = m.strip_prefix("DUP").and_then(|s| s.parse::<usize>().ok())
        && (1..=16).contains(&d)
    {
        return Some(Shuffle::Dup(d));
    }
    if let Some(d) = m.strip_prefix("SWAP").and_then(|s| s.parse::<usize>().ok())
        && (1..=16).contains(&d)
    {
        return Some(Shuffle::Swap(d));
    }
    None
}

/// True if `m` is `POP`/`DUPn`/`SWAPn`.
#[inline]
pub fn is_shuffle(m: &str) -> bool {
    as_shuffle(m).is_some()
}

#[inline]
fn shuffle_gas(m: &str) -> u64 {
    if m == "POP" { GAS_POP } else { GAS_VERYLOW }
}

/// Apply one pure stack op to `stack`, or `None` on underflow / a non-stack op.
fn step(op: &str, stack: &[i64]) -> Option<Vec<i64>> {
    let mut s = stack.to_vec();
    match as_shuffle(op)? {
        Shuffle::Pop => {
            s.pop()?;
        }
        Shuffle::Dup(d) => {
            if s.len() < d {
                return None;
            }
            let v = s[s.len() - d];
            s.push(v);
        }
        Shuffle::Swap(d) => {
            if s.len() < d + 1 {
                return None;
            }
            let n = s.len();
            s.swap(n - 1, n - 1 - d);
        }
    }
    Some(s)
}

/// Run a window of pure stack ops over `base`, or `None` if it underflows there
/// (or contains a non-stack op).
fn run_shuffle(run: &[Instr], base: &[i64]) -> Option<Vec<i64>> {
    let mut s = base.to_vec();
    for ins in run {
        s = step(ins.mnem(), &s)?;
    }
    Some(s)
}

/// The smallest initial stack height at which `run` does not underflow — the exact
/// depth its replacement must also be safe at (and no deeper, so a replacement that
/// reaches below it is rejected as underflow).
fn min_input_depth(run: &[Instr]) -> Option<usize> {
    (0..=MAX_SHUFFLE_DEPTH).find(|&h| {
        let base: Vec<i64> = (0..h as i64).collect();
        run_shuffle(run, &base).is_some()
    })
}

/// The pure stack ops worth trying from a stack of `height` (bounded by `cap` so the
/// search cannot grow the stack without limit).
fn candidate_ops(height: usize, cap: usize) -> Vec<Shuffle> {
    let mut out = vec![Shuffle::Pop];
    if height < cap {
        for d in 1..=height.min(16) {
            out.push(Shuffle::Dup(d));
        }
    }
    for d in 1..height.min(17) {
        out.push(Shuffle::Swap(d));
    }
    out
}

/// Dijkstra over DUP/SWAP/POP for the cheapest sequence turning `base` into `target`.
/// Returns `(replacement, steps_explored)`: `replacement` is `Some` only if strictly
/// cheaper than `budget` (a rewrite never raises gas), `None` if nothing cheaper is
/// found within the node cap. The heap holds only `(gas, state)` and the path is
/// reconstructed from a `from` map, so memory is O(distinct states), not O(paths).
fn cheapest_equivalent(base: &[i64], target: &[i64], budget: u64) -> (Option<Vec<String>>, u32) {
    use std::cmp::Reverse;
    use std::collections::{BinaryHeap, HashMap};

    let cap = base.len().max(target.len()) + 2;
    let mut best: HashMap<Vec<i64>, u64> = HashMap::new();
    let mut from: HashMap<Vec<i64>, (Vec<i64>, Shuffle)> = HashMap::new();
    best.insert(base.to_vec(), 0);
    let mut heap: BinaryHeap<Reverse<(u64, Vec<i64>)>> = BinaryHeap::new();
    heap.push(Reverse((0, base.to_vec())));

    let mut steps = 0u32;
    while let Some(Reverse((g, st))) = heap.pop() {
        if g >= budget {
            break;
        }
        if st == target {
            return (Some(rebuild_path(&from, base, &st)), steps);
        }
        if best.get(&st).is_some_and(|&bg| bg < g) {
            continue;
        }
        steps += 1;
        if steps > SEARCH_NODES {
            break;
        }
        for op in candidate_ops(st.len(), cap) {
            if let Some(next) = op.apply(&st) {
                let ng = g + op.gas();
                if ng >= budget {
                    continue;
                }
                if best.get(&next).is_none_or(|&bg| ng < bg) {
                    best.insert(next.clone(), ng);
                    from.insert(next.clone(), (st.clone(), op));
                    heap.push(Reverse((ng, next)));
                }
            }
        }
    }
    (None, steps)
}

/// Walk the `from` map back from `target` to `base`, collecting the ops in order.
fn rebuild_path(
    from: &std::collections::HashMap<Vec<i64>, (Vec<i64>, Shuffle)>,
    base: &[i64],
    target: &[i64],
) -> Vec<String> {
    let mut ops = Vec::new();
    let mut cur = target.to_vec();
    while cur != base {
        let (prev, op) = from
            .get(&cur)
            .expect("a reached target has a recorded predecessor");
        ops.push(op.mnem());
        cur = prev.clone();
    }
    ops.reverse();
    ops
}

/// The cheapest equivalent for a maximal window of pure stack ops (`POP`/`DUPn`/
/// `SWAPn`), or `None` if the window is already minimal (or not purely stack ops).
///
/// Stack ops only move/copy/drop slots by position, never inspecting values, so two
/// windows are equivalent iff they map an all-distinct stack identically. We compute
/// the window's net effect on such a stack at its minimal safe depth and search for a
/// strictly cheaper realizing sequence. Equality there proves equality on every taller
/// stack (the deeper slots are untouched), and the search only emits ops that stay
/// within that depth — so live values below the window are never disturbed. The result
/// is gas-monotone by construction: returned only when strictly cheaper than the input.
#[inline]
#[allow(dead_code)] // canonical engine entry; the pass uses the counted variant
pub fn minimize_shuffle(run: &[Instr]) -> Option<Vec<String>> {
    minimize_shuffle_counted(run).0
}

/// As [`minimize_shuffle`], also returning how many search states were explored (for
/// progress reporting over a whole program's windows).
///
/// A window deeper than [`MAX_RESCHEDULE_DEPTH`] is skipped without searching — its
/// state space is factorial in depth, so optimizing it is infeasible (and shallow
/// windows are where real codegen wins sit). [`reschedule_estimate`] reports the size.
pub fn minimize_shuffle_counted(run: &[Instr]) -> (Option<Vec<String>>, u32) {
    if run.is_empty() {
        return (None, 0);
    }
    let depth = match min_input_depth(run) {
        Some(d) => d,
        None => return (None, 0),
    };
    if depth > MAX_RESCHEDULE_DEPTH {
        return (None, 0);
    }
    let base: Vec<i64> = (0..depth as i64).collect();
    let target = match run_shuffle(run, &base) {
        Some(t) => t,
        None => return (None, 0),
    };
    let budget: u64 = run.iter().map(|i| shuffle_gas(i.mnem())).sum();
    cheapest_equivalent(&base, &target, budget)
}

/// Estimate the reschedule search for one pure-stack window: its input `depth`,
/// whether it is `feasible` (shallow enough to search), and `est_steps` — the search
/// states a feasible window may explore (bounded by [`SEARCH_NODES`]), or the
/// astronomical brute-force size that makes a too-deep window infeasible (so callers
/// can report why it is skipped instead of hanging).
pub fn reschedule_estimate(run: &[Instr]) -> (usize, bool, f64) {
    let depth = match min_input_depth(run) {
        Some(d) => d,
        None => return (0, false, 0.0),
    };
    let budget = run.iter().map(|i| shuffle_gas(i.mnem())).sum::<u64>() as f64;
    let branching = (2 * depth).max(1) as f64;
    let raw = branching.powf((budget / 2.0).max(1.0));
    if depth <= MAX_RESCHEDULE_DEPTH {
        (depth, true, raw.min(SEARCH_NODES as f64))
    } else {
        (depth, false, raw)
    }
}

/// Instruction arity: all push kinds yield (0, 1), op — from the opcode table.
fn instr_arity(ins: &Instr) -> Option<(usize, usize)> {
    match ins.kind {
        Kind::Push | Kind::PushSym | Kind::PushMem | Kind::Ofst => Some((0, 1)),
        Kind::Op => arity(ins.mnem()),
        Kind::Label | Kind::Raw => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::parse_str;

    const REV: &str = "_sym___revert";

    #[test]
    fn pure_check_is_identity() {
        // DUP1 reads the value, checks it, reverts — stack after == before.
        let run = parse_str(&format!("DUP1 PUSH1 32 LT {REV} JUMPI"));
        assert!(
            simulate_identity(&run),
            "a pure check via DUP must be a stack identity"
        );
    }

    #[test]
    fn consuming_check_not_identity() {
        // GT consumes a live value (not via DUP) — not an identity.
        let run = parse_str(&format!("PUSH1 5 GT {REV} JUMPI"));
        assert!(
            !simulate_identity(&run),
            "a check that consumes its input must not count as an identity"
        );
    }

    #[test]
    fn unknown_opcode_breaks_identity() {
        let run = parse_str("FOOBAR");
        assert!(!simulate_identity(&run));
    }

    #[test]
    fn residue_identity_is_empty_replacement() {
        // A pure identity check deletes entirely (empty shuffle).
        let run = parse_str(&format!("DUP1 PUSH1 32 LT {REV} JUMPI"));
        assert_eq!(strip_residue(&run), Some(vec![]));
    }

    #[test]
    fn residue_consuming_check_drops_its_input() {
        // `assert x > 5` consumes x; the live path expected nothing there -> POP.
        let run = parse_str(&format!("PUSH1 5 GT {REV} JUMPI"));
        assert_eq!(strip_residue(&run), Some(vec!["POP".to_string()]));
    }

    #[test]
    fn residue_overflow_shape_is_swap_pop() {
        // Vyper's `a + b` overflow check: input [b, a+b] -> residue [a+b].
        let run = parse_str(&format!("SWAP1 DUP2 LT {REV} JUMPI"));
        assert_eq!(
            strip_residue(&run),
            Some(vec!["SWAP1".to_string(), "POP".to_string()])
        );
    }

    #[test]
    fn residue_surviving_computed_value_is_rejected() {
        // Here x+1 is left on the stack for live code (not just the branch cond) -> None.
        let run = parse_str(&format!("PUSH1 1 ADD DUP1 ISZERO {REV} JUMPI"));
        assert_eq!(strip_residue(&run), None);
    }

    /// The replacement leaves the identical stack as `run` at `run`'s minimal depth.
    fn shuffle_equivalent(run: &[Instr], rep: &[String]) -> bool {
        let depth = min_input_depth(run).expect("a pure stack window must have a minimal depth");
        let base: Vec<i64> = (0..depth as i64).collect();
        let rep_instrs: Vec<Instr> = rep
            .iter()
            .map(|m| Instr::new(Kind::Op, vec![m.clone()]))
            .collect();
        run_shuffle(run, &base) == run_shuffle(&rep_instrs, &base)
    }

    #[test]
    fn shuffle_self_cancel_deletes_window() {
        // SWAP1 SWAP1 is a stack identity — the cheapest equivalent is nothing.
        let run = parse_str("SWAP1 SWAP1");
        assert_eq!(
            minimize_shuffle(&run),
            Some(vec![]),
            "a self-cancelling swap pair was not deleted"
        );
    }

    #[test]
    fn shuffle_dup_then_pop_deletes_window() {
        // DUP2 POP duplicates a value just to discard it — a no-op, delete it.
        let run = parse_str("DUP2 POP");
        assert_eq!(
            minimize_shuffle(&run),
            Some(vec![]),
            "a dup-then-pop no-op was not deleted"
        );
    }

    #[test]
    fn shuffle_venom_window_is_rescheduled_cheaper() {
        // A real Vyper venom leftover: SWAP1 DUP2 SWAP1 DUP1 SWAP3 (15 gas) is the
        // five-op way to write DUP2 DUP2 (6 gas). Must reschedule to a strictly
        // cheaper, provably-equivalent sequence.
        let run = parse_str("SWAP1 DUP2 SWAP1 DUP1 SWAP3");
        let rep =
            minimize_shuffle(&run).expect("a non-minimal venom shuffle was left unrescheduled");
        assert!(
            shuffle_equivalent(&run, &rep),
            "the reschedule changed the window's stack effect"
        );
        let rep_gas: u64 = rep.iter().map(|m| shuffle_gas(m)).sum();
        assert!(
            rep_gas < 15,
            "the reschedule did not lower gas below the original 15: {rep_gas}"
        );
    }

    #[test]
    fn shuffle_real_venom_windows_all_reschedule_cheaper() {
        // The distinct non-minimal windows venom 0.4.3 (GAS) actually emits where
        // independent subexpressions merge through a commutative/associative reduction
        // (measured across a multi-contract sweep). Each must reschedule to a strictly
        // cheaper, provably-equivalent sequence — a regression guard that the general
        // engine keeps covering every observed shape, not just the canonical one.
        let windows = [
            "SWAP1 SWAP3 SWAP1 SWAP3",
            "SWAP1 SWAP2 SWAP1 SWAP2",
            "SWAP1 SWAP5 SWAP4 SWAP2 SWAP1 SWAP2",
            "DUP2 SWAP3 SWAP4 SWAP1 SWAP4",
            "DUP1 SWAP2 SWAP3 SWAP1 SWAP3",
            "SWAP1 DUP2 SWAP1 DUP1 SWAP3",
        ];
        for w in windows {
            let run = parse_str(w);
            let rep = minimize_shuffle(&run)
                .unwrap_or_else(|| panic!("a real venom window was left unrescheduled: {w}"));
            assert!(
                shuffle_equivalent(&run, &rep),
                "the reschedule changed the window's stack effect: {w}"
            );
            let before: u64 = run.iter().map(|i| shuffle_gas(i.mnem())).sum();
            let after: u64 = rep.iter().map(|m| shuffle_gas(m)).sum();
            assert!(
                after < before,
                "the reschedule did not lower gas for {w}: {after} >= {before}"
            );
        }
    }

    #[test]
    fn shuffle_too_deep_window_is_skipped_not_searched() {
        // A deep permutation window like a large real contract emits has a factorial
        // search space; it must be skipped instantly (this exact shape hung the
        // rescheduler before the depth bound), not explored.
        let run = parse_str(
            "SWAP3 SWAP13 SWAP12 SWAP11 SWAP10 SWAP9 SWAP8 SWAP7 SWAP6 SWAP5 SWAP4 SWAP3",
        );
        let (rep, steps) = minimize_shuffle_counted(&run);
        assert_eq!(
            rep, None,
            "a too-deep window must be skipped, not rewritten"
        );
        assert_eq!(steps, 0, "a too-deep window must not be searched at all");
        let (_depth, feasible, _est) = reschedule_estimate(&run);
        assert!(
            !feasible,
            "a deep permutation window must be reported infeasible"
        );
    }

    #[test]
    fn shuffle_already_minimal_is_left_alone() {
        // DUP2 DUP2 is already the cheapest sequence for its stack effect.
        let run = parse_str("DUP2 DUP2");
        assert_eq!(
            minimize_shuffle(&run),
            None,
            "an already-minimal window was needlessly rewritten"
        );
    }

    #[test]
    fn shuffle_single_op_is_left_alone() {
        // A lone SWAP1 / POP cannot be made cheaper.
        assert_eq!(
            minimize_shuffle(&parse_str("SWAP1")),
            None,
            "a lone SWAP1 was wrongly rewritten"
        );
        assert_eq!(
            minimize_shuffle(&parse_str("POP")),
            None,
            "a lone POP was wrongly rewritten"
        );
    }

    #[test]
    fn shuffle_rejects_non_stack_op() {
        // A window that is not purely stack ops must never be rescheduled.
        assert_eq!(
            minimize_shuffle(&parse_str("ADD")),
            None,
            "a non-stack op was treated as a shuffle"
        );
        assert_eq!(
            minimize_shuffle(&parse_str("DUP1 ADD POP")),
            None,
            "a window containing ADD was treated as a pure shuffle"
        );
    }
}
