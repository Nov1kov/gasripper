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

use z3::ast::{Ast, BV};
use z3::{Config, SatResult, Solver, with_z3_config};

use super::asm::{Instr, Kind};
use super::opcodes::gas;

/// EVM word width.
const WORD: u32 = 256;

/// User-tunable search limits — the power/time trade-off of the pass. Larger candidates and
/// budgets find more rewrites and burn more solver time; the defaults are the values the
/// shipped e2e proofs are pinned against. Set via `superopt_*` config keys or `--superopt-*`
/// CLI flags.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Longest source run the optimizer will attempt.
    pub max_block: usize,
    /// Longest candidate program the search synthesizes.
    pub max_synth: usize,
    /// Per-equivalence-check solver timeout (milliseconds).
    pub timeout_ms: u32,
    /// Hard cap on solver checks per block.
    pub max_checks: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_block: MAX_BLOCK,
            max_synth: MAX_SYNTH_LEN,
            timeout_ms: CHECK_TIMEOUT_MS,
            max_checks: MAX_CHECKS_PER_BLOCK,
        }
    }
}

/// Longest source run the optimizer will attempt (keeps `inputs_needed`/search bounded).
const MAX_BLOCK: usize = 24;

/// Longest candidate program the search synthesizes. The optimum of the simplifications this pass
/// targets (identity/constant collapse) is tiny; a small bound keeps enumeration fast and the
/// solver honest about what it can prove (longer optima are left un-optimized — ebso likewise
/// times out on most blocks). Four covers the smallest solc-shaped rewrite: the internal-call
/// return address threads through the block, so a collapsed body still needs
/// `POP POP PUSH0 SWAP1`-style drop+reorder around it.
const MAX_SYNTH_LEN: usize = 4;

/// Hard cap on candidates examined per block — a backstop against alphabet blow-up.
const MAX_CANDIDATES: usize = 20_000;

/// Per-equivalence-check solver timeout (milliseconds). A timeout reads as "not proven" ⇒ the
/// candidate is rejected (fail safe). The proofs this pass lands (identity/constant collapse)
/// take tens of milliseconds; what runs long is *refuting* a wrong candidate over nonlinear
/// 512-bit terms, so a short timeout mostly trims wasted refutations.
const CHECK_TIMEOUT_MS: u32 = 500;

/// Hard cap on solver checks per block. Ground vectors refute almost every wrong candidate for
/// free; if a block still drives this many checks, its terms are solver-hostile (nonlinear,
/// symbolic moduli) and the block is left unoptimized (fail safe) rather than stalling the scan.
const MAX_CHECKS_PER_BLOCK: usize = 128;

/// The arithmetic/logic opcodes this module interprets with exact EVM mod-2^256 semantics. Each is
/// deterministic, reads only its stack operands, and has no side effect — so a run of these plus
/// stack moves is a pure block. The EVM special cases are modeled exactly: division/mod by zero is
/// zero, `ADDMOD`/`MULMOD` reduce the full-width intermediate (512-bit, not mod 2^256), `BYTE`
/// beyond index 31 is zero, `SIGNEXTEND` beyond byte 30 is the identity. `EXP` is the one
/// arithmetic opcode deliberately excluded: it has no closed bit-vector form (an unrolled
/// square-and-multiply would drown the solver) and its gas is dynamic, so the static cost model
/// cannot rank candidates containing it.
fn is_interpreted_op(m: &str) -> bool {
    matches!(
        m,
        "ADD"
            | "SUB"
            | "MUL"
            | "DIV"
            | "SDIV"
            | "MOD"
            | "SMOD"
            | "ADDMOD"
            | "MULMOD"
            | "SIGNEXTEND"
            | "AND"
            | "OR"
            | "XOR"
            | "NOT"
            | "ISZERO"
            | "EQ"
            | "LT"
            | "GT"
            | "SLT"
            | "SGT"
            | "BYTE"
            | "SHL"
            | "SHR"
            | "SAR"
    )
}

/// `ins` is eligible for a pure superopt block: a concrete stack move or an interpreted op. Any
/// symbolic push, label, raw byte, jump, memory/storage/log/call, or un-interpreted opcode makes
/// the instruction ineligible and ends the run.
pub fn is_eligible(ins: &Instr) -> bool {
    match ins.kind {
        // Only a push with a parseable literal immediate: a symbolic push (solc `PUSH [tag]`)
        // has a link-time value, so it must end the run rather than poison it.
        Kind::Push => ins.tokens.get(1).and_then(|t| value_bytes(t)).is_some(),
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

/// The program's stack shape from arities alone — `(net height change, input slots read below the
/// start)` — with no Z3 term in sight. `None` if an instruction is not eligible.
fn shape(prog: &[Instr]) -> Option<(i64, usize)> {
    let mut height: i64 = 0;
    let mut deepest: i64 = 0;
    for ins in prog {
        let (pops, pushes) = io(ins)?;
        let need = height - pops as i64;
        deepest = deepest.min(need);
        height = need + pushes as i64;
    }
    Some((height, (-deepest).max(0) as usize))
}

/// The number of pre-existing stack words the program reads below its own pushes — i.e. how many
/// input slots a faithful symbolic execution must seed. `None` if an instruction is not eligible.
fn inputs_needed(prog: &[Instr]) -> Option<usize> {
    shape(prog).map(|(_, inputs)| inputs)
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
        "ADDMOD" | "MULMOD" => Some((3, 1)),
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
        // Unary / binary / ternary interpreted ops. EVM pops the top operand first.
        if matches!(m, "NOT" | "ISZERO") {
            let a = st.pop()?;
            st.push(match m {
                "NOT" => a.bvnot(),
                _ => a.eq(&zero).ite(&one, &zero),
            });
            continue;
        }
        // ADDMOD/MULMOD reduce the FULL-WIDTH intermediate: (a op b) is computed in 512 bits
        // before the modulus, exactly like the EVM (mod-2^256 truncation first would be wrong).
        if matches!(m, "ADDMOD" | "MULMOD") {
            let a = st.pop()?.zero_ext(WORD);
            let b = st.pop()?.zero_ext(WORD);
            let n = st.pop()?;
            let wide = if m == "ADDMOD" {
                a.bvadd(&b)
            } else {
                a.bvmul(&b)
            };
            let r = wide.bvurem(&n.zero_ext(WORD)).extract(WORD - 1, 0);
            st.push(n.eq(&zero).ite(&zero, &r));
            continue;
        }
        let a = st.pop()?; // top
        let b = st.pop()?; // second
        let r = match m {
            "ADD" => a.bvadd(&b),
            "SUB" => a.bvsub(&b),
            "MUL" => a.bvmul(&b),
            // The EVM defines every division/remainder by zero as zero (the SMT-LIB defaults
            // differ: bvudiv by zero is all-ones, bvurem by zero is the dividend).
            "DIV" => b.eq(&zero).ite(&zero, &a.bvudiv(&b)),
            "SDIV" => b.eq(&zero).ite(&zero, &a.bvsdiv(&b)), // bvsdiv(MIN, -1) = MIN, like the EVM
            "MOD" => b.eq(&zero).ite(&zero, &a.bvurem(&b)),
            "SMOD" => b.eq(&zero).ite(&zero, &a.bvsrem(&b)), // bvsrem: sign of the dividend
            "AND" => a.bvand(&b),
            "OR" => a.bvor(&b),
            "XOR" => a.bvxor(&b),
            "EQ" => a.eq(&b).ite(&one, &zero),
            "LT" => a.bvult(&b).ite(&one, &zero),
            "GT" => a.bvugt(&b).ite(&one, &zero),
            "SLT" => a.bvslt(&b).ite(&one, &zero),
            "SGT" => a.bvsgt(&b).ite(&one, &zero),
            "SHL" => b.bvshl(&a), // a = shift (top), b = value
            "SHR" => b.bvlshr(&a),
            "SAR" => b.bvashr(&a),
            // BYTE indexes from the most significant byte; index > 31 yields zero.
            "BYTE" => {
                let last = BV::from_u64(31, WORD);
                let eight = BV::from_u64(8, WORD);
                let mask = BV::from_u64(0xff, WORD);
                let shift = last.bvsub(&a).bvmul(&eight);
                a.bvugt(&last)
                    .ite(&zero, &b.bvlshr(&shift).bvand(&mask))
            }
            // SIGNEXTEND extends from bit 8k+7 (a = k, the byte index); k > 30 is the identity.
            "SIGNEXTEND" => {
                let cap = BV::from_u64(30, WORD);
                let eight = BV::from_u64(8, WORD);
                let bits = a.bvmul(&eight);
                let sign = b.bvlshr(&bits.bvadd(&BV::from_u64(7, WORD))).bvand(&one);
                let low = one.bvshl(&bits.bvadd(&eight)).bvsub(&one);
                let ext = sign.eq(&one).ite(&b.bvor(&low.bvnot()), &b.bvand(&low));
                a.bvugt(&cap).ite(&b, &ext)
            }
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

/// Enumerate candidate programs (the empty program, then length 1..=`max_synth`) over the
/// source alphabet, capped at [`MAX_CANDIDATES`].
fn candidates(run: &[Instr], max_synth: usize) -> Vec<Vec<Instr>> {
    let alpha = alphabet(run);
    // Up to the source length (not one less): a same-length candidate can still be strictly cheaper
    // (e.g. `PUSH0 DUP1` -> `PUSH0 PUSH0`, swapping a 3-gas `DUP1` for a 2-gas `PUSH0`).
    let max_len = run.len().min(max_synth);
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

/// Concrete input stacks used to refute wrong candidates without a solver call: distinct small
/// odd words, an extremes mix, all-zeros (the divisor/shift special cases), large powers of two,
/// the sign bit, and small consecutive values. The richer the pool, the fewer coincidental
/// survivors reach the (expensive) symbolic proof. Must run inside a [`with_z3_config`] scope.
fn vectors(n: usize) -> Vec<Vec<BV>> {
    let max = BV::from_u64(0, WORD).bvnot();
    let bit = |b: u64| BV::from_u64(1, WORD).bvshl(&BV::from_u64(b, WORD));
    (0..6)
        .map(|k| {
            (0..n)
                .map(|i| match k {
                    0 => BV::from_u64((2 * i + 3) as u64, WORD),
                    1 if i % 2 == 0 => max.clone(),
                    1 => BV::from_u64(1, WORD),
                    2 => BV::from_u64(0, WORD),
                    3 => bit((i as u64 * 61 + 13) % 250),
                    4 if i % 2 == 0 => bit(255),
                    4 => BV::from_u64(2, WORD),
                    _ => BV::from_u64(i as u64 + 1, WORD),
                })
                .collect()
        })
        .collect()
}

/// Whether two fully concrete final stacks are equal, decided by the rewriter alone (no solver).
/// A mismatch on a concrete input is a definitive disproof of equivalence, so this is a sound and
/// near-free pre-filter in front of the symbolic proof.
fn ground_equiv(a: &[BV], b: &[BV]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.eq(y).simplify().as_bool() == Some(true))
}

/// Whether two final stacks are provably equal on every input. Per-slot independent checks: the
/// stacks differ iff *some* slot can differ, so equivalence holds iff every slot's inequality is
/// `unsat`. Each check spends one unit of `budget`; an exhausted budget reads as "not proven"
/// (fail safe). Must run inside a [`with_z3_config`] scope.
fn stacks_equiv(a: &[BV], b: &[BV], budget: &mut usize) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let s = Solver::new();
    for (x, y) in a.iter().zip(b.iter()) {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
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

/// The cheapest SMT-proven-equivalent program for a pure `run` under the given search `limits`,
/// or `None` if the run is not a valid superopt block or nothing strictly cheaper was proven. The
/// returned program is guaranteed to cost less gas than `run` and to leave the identical final
/// stack on every input.
pub fn optimize_block(run: &[Instr], limits: &Limits) -> Option<Vec<Instr>> {
    if run.len() < 2 || run.len() > limits.max_block || !run.iter().all(is_eligible) {
        return None;
    }
    let n = inputs_needed(run)?;
    let src_gas = block_gas(run)?;
    // Cheapest-first: the search proves the optimum early, and the incumbent test then skips the
    // (exponentially many) costlier candidates without a solver call. Stable, so enumeration
    // order still breaks gas ties deterministically.
    let mut cands = candidates(run, limits.max_synth);
    cands.sort_by_cached_key(|c| block_gas(c).unwrap_or(u32::MAX));

    let mut cfg = Config::new();
    cfg.set_timeout_msec(limits.timeout_ms as u64);

    let chosen = with_z3_config(&cfg, || {
        let inputs: Vec<BV> = (0..n)
            .map(|i| BV::new_const(format!("in{i}"), WORD))
            .collect();
        let src_out = symexec(run, &inputs)?;
        let probes = vectors(n);
        let src_ground: Vec<Vec<BV>> = probes
            .iter()
            .map(|v| symexec(run, v))
            .collect::<Option<_>>()?;
        let src_shape = shape(run)?;
        let mut budget = limits.max_checks;
        let mut best: Option<(u32, usize)> = None;
        for (idx, cand) in cands.iter().enumerate() {
            let cg = match block_gas(cand) {
                Some(g) if g < src_gas => g,
                _ => continue,
            };
            if best.map(|(bg, _)| cg >= bg).unwrap_or(false) {
                continue; // cannot beat the incumbent — skip the solver call
            }
            // Integer-only shape gate: a different net height can never leave the same final
            // stack, and reading deeper than the source's inputs underflows — both decided
            // without building a single term.
            match shape(cand) {
                Some((net, need)) if net == src_shape.0 && need <= n => {}
                _ => continue,
            }
            let refuted = probes.iter().zip(&src_ground).any(|(v, sg)| {
                symexec(cand, v).is_none_or(|out| !ground_equiv(sg, &out))
            });
            if refuted {
                continue; // a concrete counterexample disproves the candidate solver-free
            }
            if budget == 0 {
                break; // solver-hostile block — keep whatever was proven so far (fail safe)
            }
            if let Some(out) = symexec(cand, &inputs) {
                if stacks_equiv(&src_out, &out, &mut budget) {
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
