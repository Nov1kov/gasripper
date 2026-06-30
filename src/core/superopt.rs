//! SMT block superoptimizer engine (opt-in, `smt` Cargo feature).
//!
//! For a **pure straight-line block** — only stack movement (`PUSH`/`DUP`/`SWAP`/`POP`) and the
//! result-invariant arithmetic/logic opcodes this module interprets — it synthesizes the cheapest
//! gas-equivalent instruction sequence and **proves** the rewrite with Z3 before returning it.
//!
//! This is ebso's *basic superoptimization* (`bso`) bounded for tractability: candidate programs
//! are enumerated by increasing length over a small alphabet derived from the source, and each is
//! accepted only when Z3 shows it produces the **identical final stack on every 256-bit input**.
//! Unlike the syrup encoding (opcodes as uninterpreted functions), the interpreted bit-vector
//! semantics here let Z3 discover real algebraic simplifications (`x+0 → x`, `NOT NOT x → x`,
//! collapsing redundant recomputation), not only stack scheduling.
//!
//! Soundness rests on three things: only side-effect-free, control-flow-free, fully concrete
//! opcodes are eligible (so block-local replacement is valid — ebso's replacement lemma); the
//! interpreted opcodes map exactly onto EVM mod-2^256 semantics; and a candidate is emitted only
//! on a Z3 `unsat` proof of non-equivalence (timeout/`unknown` ⇒ keep the original, fail safe).

use z3::ast::BV;
use z3::{Config, SatResult, Solver, with_z3_config};

use super::asm::{Instr, Kind};
use super::opcodes::gas;

/// EVM word width.
const WORD: u32 = 256;

/// Longest source run the optimizer will attempt (keeps `inputs_needed`/search bounded).
const MAX_BLOCK: usize = 16;

/// Longest candidate program the search synthesizes. The optimum of the simplifications this pass
/// targets (identity/constant collapse) is tiny; a small bound keeps enumeration fast and the
/// solver honest about what it can prove (longer optima are left un-optimized — ebso likewise
/// times out on most blocks).
const MAX_SYNTH_LEN: usize = 2;

/// Hard cap on candidates examined per block — a backstop against alphabet blow-up.
const MAX_CANDIDATES: usize = 20_000;

/// Per-equivalence-check solver timeout (milliseconds). A timeout reads as "not proven" ⇒ the
/// candidate is rejected (fail safe).
const CHECK_TIMEOUT_MS: u32 = 2_000;

/// The arithmetic/logic opcodes this module interprets with exact EVM mod-2^256 semantics. Each is
/// deterministic, reads only its stack operands, and has no side effect — so a run of these plus
/// stack moves is a pure block. Division/mod/signed/`EXP`/`BYTE`/`SAR` are deliberately excluded
/// (special-case EVM semantics not modeled here).
fn is_interpreted_op(m: &str) -> bool {
    matches!(
        m,
        "ADD"
            | "SUB"
            | "MUL"
            | "AND"
            | "OR"
            | "XOR"
            | "NOT"
            | "ISZERO"
            | "EQ"
            | "LT"
            | "GT"
            | "SHL"
            | "SHR"
    )
}

/// `ins` is eligible for a pure superopt block: a concrete stack move or an interpreted op. Any
/// symbolic push, label, raw byte, jump, memory/storage/log/call, or un-interpreted opcode makes
/// the instruction ineligible and ends the run.
pub fn is_eligible(ins: &Instr) -> bool {
    match ins.kind {
        Kind::Push => true,
        Kind::Op => {
            let m = ins.mnem();
            m == "PUSH0"
                || m == "POP"
                || is_dupn(m).is_some()
                || is_swapn(m).is_some()
                || is_interpreted_op(m)
        }
        _ => false,
    }
}

/// `DUPn` index (1..=16), or `None`.
fn is_dupn(m: &str) -> Option<usize> {
    let n: usize = m.strip_prefix("DUP")?.parse().ok()?;
    (1..=16).contains(&n).then_some(n)
}

/// `SWAPn` index (1..=16), or `None`.
fn is_swapn(m: &str) -> Option<usize> {
    let n: usize = m.strip_prefix("SWAP")?.parse().ok()?;
    (1..=16).contains(&n).then_some(n)
}

/// Static gas of one eligible instruction.
fn op_gas(ins: &Instr) -> Option<u32> {
    gas(ins.mnem())
}

/// Total static gas of a program, or `None` if any instruction has no modeled cost.
fn block_gas(prog: &[Instr]) -> Option<u32> {
    prog.iter().map(op_gas).sum()
}

/// The number of pre-existing stack words the program reads below its own pushes — i.e. how many
/// input slots a faithful symbolic execution must seed. `None` if an instruction is not eligible.
fn inputs_needed(prog: &[Instr]) -> Option<usize> {
    let mut height: i64 = 0;
    let mut deepest: i64 = 0;
    for ins in prog {
        let (pops, pushes) = io(ins)?;
        let need = height - pops as i64;
        deepest = deepest.min(need);
        height = need + pushes as i64;
    }
    Some((-deepest).max(0) as usize)
}

/// (pops, pushes) for an eligible instruction, accounting for `DUP`/`SWAP`'s real stack reach.
fn io(ins: &Instr) -> Option<(usize, usize)> {
    if ins.kind == Kind::Push {
        return Some((0, 1));
    }
    let m = ins.mnem();
    if let Some(n) = is_dupn(m) {
        return Some((n, n + 1)); // reads n deep, leaves them, adds a copy
    }
    if let Some(n) = is_swapn(m) {
        return Some((n + 1, n + 1)); // touches top and the (n+1)-th, net height unchanged
    }
    match m {
        "PUSH0" => Some((0, 1)),
        "POP" => Some((1, 0)),
        "NOT" | "ISZERO" => Some((1, 1)),
        _ if is_interpreted_op(m) => Some((2, 1)),
        _ => None,
    }
}

/// Symbolically execute `prog` from an initial stack of `inputs` (index 0 = bottom). Returns the
/// final stack as bit-vector terms, or `None` on a stack underflow (an exceptional EVM halt — not
/// a valid pure block in this context). Must run inside a [`with_z3_config`] scope.
fn symexec(prog: &[Instr], inputs: &[BV]) -> Option<Vec<BV>> {
    let zero = BV::from_u64(0, WORD);
    let one = BV::from_u64(1, WORD);
    let mut st: Vec<BV> = inputs.to_vec();
    for ins in prog {
        let m = ins.mnem();
        if ins.kind == Kind::Push {
            st.push(push_value_bv(ins)?);
            continue;
        }
        if m == "PUSH0" {
            st.push(zero.clone());
            continue;
        }
        if m == "POP" {
            st.pop()?;
            continue;
        }
        if let Some(n) = is_dupn(m) {
            if st.len() < n {
                return None;
            }
            st.push(st[st.len() - n].clone());
            continue;
        }
        if let Some(n) = is_swapn(m) {
            if st.len() < n + 1 {
                return None;
            }
            let top = st.len() - 1;
            st.swap(top, top - n);
            continue;
        }
        // Unary / binary interpreted ops. EVM pops the top operand first.
        if matches!(m, "NOT" | "ISZERO") {
            let a = st.pop()?;
            st.push(match m {
                "NOT" => a.bvnot(),
                _ => a.eq(&zero).ite(&one, &zero),
            });
            continue;
        }
        let a = st.pop()?; // top
        let b = st.pop()?; // second
        let r = match m {
            "ADD" => a.bvadd(&b),
            "SUB" => a.bvsub(&b),
            "MUL" => a.bvmul(&b),
            "AND" => a.bvand(&b),
            "OR" => a.bvor(&b),
            "XOR" => a.bvxor(&b),
            "EQ" => a.eq(&b).ite(&one, &zero),
            "LT" => a.bvult(&b).ite(&one, &zero),
            "GT" => a.bvugt(&b).ite(&one, &zero),
            "SHL" => b.bvshl(&a), // a = shift (top), b = value
            "SHR" => b.bvlshr(&a),
            _ => return None,
        };
        st.push(r);
    }
    Some(st)
}

/// The 256-bit value of a `PUSHn <imm>` instruction as a bit-vector. Must run inside a
/// [`with_z3_config`] scope.
fn push_value_bv(ins: &Instr) -> Option<BV> {
    let bytes = value_bytes(ins.tokens.get(1)?)?;
    let mut bits = [false; WORD as usize];
    // `bytes` is big-endian; bit j (LSB = 0) lives in the j/8-th byte from the end.
    for (k, byte) in bytes.iter().rev().enumerate() {
        for bit in 0..8 {
            let idx = k * 8 + bit;
            if idx < bits.len() {
                bits[idx] = (byte >> bit) & 1 == 1;
            }
        }
    }
    BV::from_bits(&bits)
}

/// Parse a PUSH immediate token (`0x..` hex or decimal) into minimal big-endian bytes
/// (leading zeros trimmed; zero ⇒ empty).
fn value_bytes(token: &str) -> Option<Vec<u8>> {
    let raw = if let Some(h) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        let h = if h.len() % 2 == 1 {
            format!("0{h}")
        } else {
            h.to_string()
        };
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .ok()?
    } else {
        let v: u128 = token.parse().ok()?;
        v.to_be_bytes().to_vec()
    };
    let first = raw.iter().position(|&b| b != 0).unwrap_or(raw.len());
    Some(raw[first..].to_vec())
}

/// Replacement token for one candidate instruction (the inverse the sidecar/`apply_spans` decode):
/// a `PUSHn <imm>` becomes a folded-literal `#<hex>` (or `PUSH0` for zero), every other eligible
/// op is its bare mnemonic.
fn token_for(ins: &Instr) -> String {
    if ins.kind == Kind::Push {
        match ins.tokens.get(1).and_then(|t| value_bytes(t)) {
            Some(b) if b.is_empty() => "PUSH0".to_string(),
            Some(b) => format!(
                "#{}",
                b.iter().map(|x| format!("{x:02x}")).collect::<String>()
            ),
            None => ins.mnem().to_string(),
        }
    } else {
        ins.mnem().to_string()
    }
}

/// The candidate alphabet for a source run: stack primitives, the source's own interpreted ops,
/// and each distinct non-zero push constant the source uses (zero is covered by `PUSH0`).
fn alphabet(run: &[Instr]) -> Vec<Instr> {
    let mut out = vec![
        Instr::new(Kind::Op, vec!["PUSH0".into()]),
        Instr::new(Kind::Op, vec!["POP".into()]),
        Instr::new(Kind::Op, vec!["DUP1".into()]),
        Instr::new(Kind::Op, vec!["DUP2".into()]),
        Instr::new(Kind::Op, vec!["SWAP1".into()]),
    ];
    let mut seen_ops: Vec<String> = Vec::new();
    let mut seen_push: Vec<Vec<u8>> = Vec::new();
    for ins in run {
        if ins.kind == Kind::Push {
            if let Some(b) = ins.tokens.get(1).and_then(|t| value_bytes(t)) {
                if !b.is_empty() && !seen_push.contains(&b) {
                    seen_push.push(b.clone());
                    out.push(ins.clone());
                }
            }
        } else if ins.kind == Kind::Op {
            let m = ins.mnem();
            if is_interpreted_op(m) && !seen_ops.iter().any(|s| s == m) {
                seen_ops.push(m.to_string());
                out.push(ins.clone());
            }
        }
    }
    out
}

/// Enumerate candidate programs (the empty program, then length 1..=`MAX_SYNTH_LEN`) over the
/// source alphabet, capped at [`MAX_CANDIDATES`].
fn candidates(run: &[Instr]) -> Vec<Vec<Instr>> {
    let alpha = alphabet(run);
    // Up to the source length (not one less): a same-length candidate can still be strictly cheaper
    // (e.g. `PUSH0 DUP1` -> `PUSH0 PUSH0`, swapping a 3-gas `DUP1` for a 2-gas `PUSH0`).
    let max_len = run.len().min(MAX_SYNTH_LEN);
    let mut out: Vec<Vec<Instr>> = vec![Vec::new()];
    let mut frontier: Vec<Vec<Instr>> = vec![Vec::new()];
    for _ in 0..max_len {
        let mut next = Vec::new();
        for prefix in &frontier {
            for sym in &alpha {
                if out.len() + next.len() >= MAX_CANDIDATES {
                    out.extend(next);
                    return out;
                }
                let mut prog = prefix.clone();
                prog.push(sym.clone());
                next.push(prog);
            }
        }
        out.extend(next.iter().cloned());
        frontier = next;
    }
    out
}

/// Whether two final stacks are provably equal on every input. Per-slot independent checks: the
/// stacks differ iff *some* slot can differ, so equivalence holds iff every slot's inequality is
/// `unsat`. Must run inside a [`with_z3_config`] scope.
fn stacks_equiv(a: &[BV], b: &[BV]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let s = Solver::new();
    for (x, y) in a.iter().zip(b.iter()) {
        s.push();
        s.assert(x.eq(y).not());
        let r = s.check();
        s.pop(1);
        if r != SatResult::Unsat {
            return false; // a differing input exists (or the solver could not prove equality)
        }
    }
    true
}

/// The cheapest SMT-proven-equivalent program for a pure `run`, or `None` if the run is not a valid
/// superopt block or nothing strictly cheaper was proven. The returned program is guaranteed to
/// cost less gas than `run` and to leave the identical final stack on every input.
pub fn optimize_block(run: &[Instr]) -> Option<Vec<Instr>> {
    if run.len() < 2 || run.len() > MAX_BLOCK || !run.iter().all(is_eligible) {
        return None;
    }
    let n = inputs_needed(run)?;
    let src_gas = block_gas(run)?;
    let cands = candidates(run);

    let mut cfg = Config::new();
    cfg.set_timeout_msec(CHECK_TIMEOUT_MS as u64);

    let chosen = with_z3_config(&cfg, || {
        let inputs: Vec<BV> = (0..n)
            .map(|i| BV::new_const(format!("in{i}"), WORD))
            .collect();
        let src_out = symexec(run, &inputs)?;
        let mut best: Option<(u32, usize)> = None;
        for (idx, cand) in cands.iter().enumerate() {
            let cg = match block_gas(cand) {
                Some(g) if g < src_gas => g,
                _ => continue,
            };
            if best.map(|(bg, _)| cg >= bg).unwrap_or(false) {
                continue; // cannot beat the incumbent — skip the solver call
            }
            if let Some(out) = symexec(cand, &inputs) {
                if stacks_equiv(&src_out, &out) {
                    best = Some((cg, idx));
                }
            }
        }
        best.map(|(_, idx)| idx)
    });

    chosen.map(|idx| cands[idx].clone())
}

/// Replacement tokens for an `optimize_block` result, ready for a [`super::Span`].
pub fn tokens(prog: &[Instr]) -> Vec<String> {
    prog.iter().map(token_for).collect()
}
