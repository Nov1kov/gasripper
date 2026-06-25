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
        assert_eq!(strip_residue(&run), Some(vec!["SWAP1".to_string(), "POP".to_string()]));
    }

    #[test]
    fn residue_surviving_computed_value_is_rejected() {
        // Here x+1 is left on the stack for live code (not just the branch cond) -> None.
        let run = parse_str(&format!("PUSH1 1 ADD DUP1 ISZERO {REV} JUMPI"));
        assert_eq!(strip_residue(&run), None);
    }
}
