//! End-to-end proofs for the `guards` feature on a real EVM (revm, test-only dep).
//!
//! Each test uses the shared harness ([`crate::features::e2e_harness`]): compile a
//! real contract, strip the guard category, re-assemble creation bytecode, deploy
//! baseline + optimized, call `foo`, and assert the result is unchanged while gas
//! drops (and, for the auth contracts, that a stranger is still rejected). The cases
//! span both languages, the arithmetic guards (`+ - * /`), the range/cast guard
//! (`convert` / `require(a < 256)`), and with / without a trusted-caller wrapper.
//! Each test SKIPS when its toolchain is unavailable.

use std::collections::HashSet;

use crate::core::asm::Kind;
use crate::core::{Category, strip_guards};
use crate::features::e2e_harness::{
    assert_preserved_and_smaller, assert_rejects_stranger, assert_win, deploy_and_call,
    deploy_then_call, encode_call, measure, owner_addr, write_temp,
};
use crate::sidecar::{Backend, Lang};

/// A Vyper `foo` returning `ret_expr`, optionally wrapped in a trusted-caller assert.
fn vyper(auth: bool, sig: &str, ret: &str, ret_expr: &str) -> String {
    if auth {
        format!(
            "owner: public(address)\n\n@deploy\ndef __init__():\n    self.owner = msg.sender\n\n@external\ndef foo({sig}) -> {ret}:\n    assert msg.sender == self.owner\n    return {ret_expr}\n"
        )
    } else {
        format!("@external\ndef foo({sig}) -> {ret}:\n    return {ret_expr}\n")
    }
}

/// A Solidity `foo` with the given `body`, optionally wrapped in a `require(msg.sender
/// == owner)`. Auth reads storage (`view`); without it the function is `pure`.
fn solidity(auth: bool, args: &str, body: &str) -> String {
    if auth {
        format!(
            "pragma solidity ^0.8.20;\ncontract C {{\n    address public owner;\n    constructor() {{ owner = msg.sender; }}\n    function foo({args}) external view returns (uint256) {{\n        require(msg.sender == owner);\n        {body}\n    }}\n}}\n"
        )
    } else {
        format!(
            "pragma solidity ^0.8.20;\ncontract C {{\n    function foo({args}) external pure returns (uint256) {{\n        {body}\n    }}\n}}\n"
        )
    }
}

/// Strip the guards, prove `foo(args) == expected` with a gas win, and confirm the
/// auth guard still rejects a stranger. Skips on a missing toolchain.
fn win(
    lang: &str,
    backend: Backend,
    filename: &str,
    source: &str,
    sig: &str,
    args: &[u64],
    expected: u64,
    gas_base: u64,
    gas_opt: u64,
) {
    let path = write_temp(filename, source);
    let r = match measure(&backend, &path, Category::Guard, encode_call(sig, args)) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP {lang} {filename} e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert_win(&r, lang, expected, gas_base, gas_opt);
    assert_rejects_stranger(&r.creation_opt, encode_call(sig, args));
}

/// Strip the guards on a contract with NO auth wrapper: result preserved, bytecode
/// shrinks (call gas drops only when the guard is on this dispatcher's hot path).
fn no_auth(
    lang: &str,
    backend: Backend,
    filename: &str,
    source: &str,
    sig: &str,
    args: &[u64],
    expected: u64,
    gas_base: u64,
    gas_opt: u64,
) {
    let path = write_temp(filename, source);
    let r = match measure(&backend, &path, Category::Guard, encode_call(sig, args)) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP {lang} {filename} e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert_preserved_and_smaller(&r, lang, expected, gas_base, gas_opt);
}

fn vy() -> Backend {
    Backend::new(Lang::Vyper)
}
fn so() -> Backend {
    Backend::new(Lang::Solidity)
}

const ARG2: &str = "a: uint256, b: uint256";
const SARG2: &str = "uint256 a, uint256 b";
const SIG2: &str = "foo(uint256,uint256)";
const SIG1: &str = "foo(uint256)";

// --- Post-strip DCE: a real contract's orphaned revert handler is removed ---

#[test]
fn vyper_dce_cuts_the_real_revert_handler() {
    // On a REAL compiled contract (not hand-written asm): a plain `a + b` shares ONE
    // compiler revert handler (`_sym___revert: PUSH0 DUP1 REVERT`) that every guard
    // branches to on failure (selector mismatch, calldata bounds, overflow). Stripping
    // all those guards orphans it, and post-strip DCE must select that very block for
    // deletion — proving the hand-asm unit tests match real compiler output. Behavior
    // and gas on a real EVM are already proven for this contract by `vyper_add_no_auth`.
    let path = write_temp("g_vy_dce.vy", &vyper(false, ARG2, "uint256", "a + b"));
    let dump = match vy().dump(&path, None) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("SKIP vyper DCE e2e (toolchain unavailable): {e}");
            return;
        }
    };

    let only: HashSet<Category> = [Category::Guard].into_iter().collect();
    let (opt, spans) = strip_guards(&dump.instrs, &only);

    // One stripped span starts at the revert label — the orphaned handler block, deleted
    // outright (a guard span instead ends at its JUMPI and is never a Label start).
    let handler = spans
        .iter()
        .find(|s| {
            dump.instrs[s.start].kind == Kind::Label
                && dump.instrs[s.start].mnem().contains("revert")
        })
        .expect("DCE did not select the orphaned revert handler block for deletion");
    let block: Vec<&str> = dump.instrs[handler.start..=handler.end]
        .iter()
        .map(|i| i.mnem())
        .collect();
    assert_eq!(
        block.last(),
        Some(&"REVERT"),
        "the deleted block is not a revert handler: {block:?}"
    );
    assert!(
        handler.replacement.is_empty(),
        "the dead revert handler must be deleted, not replaced"
    );
    assert!(
        !opt.iter()
            .any(|i| i.kind == Kind::Label && i.mnem().contains("revert")),
        "a revert handler label survived DCE in the optimized stream"
    );
}

// --- State-dependent guard must NOT be stripped ---

// `execute(target)` asserts a stored threshold (set to 5 in the constructor) is at least
// `target`, then returns `target`. The `>=` lowers to `<state load> LT/GT _sym_*revert* JUMPI`,
// a pure stack-identity the strip engine wrongly deleted before state reads became a barrier —
// the storage analog of the reported `assert staticcall balanceOf(self) >= target`.
const VYPER_STATE: &str = r#"
threshold: public(uint256)

@deploy
def __init__():
    self.threshold = 5

@external
def execute(target: uint256) -> uint256:
    assert self.threshold >= target
    return target
"#;

const SOLIDITY_STATE: &str = r#"
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;
contract C {
    uint256 threshold;
    constructor() { threshold = 5; }
    function execute(uint256 target) external view returns (uint256) {
        require(threshold >= target);
        return target;
    }
}
"#;

const IN_RANGE: u64 = 3;
const OVER_RANGE: u64 = 10;

/// Prove on a real EVM that the state-dependent guard in `source` survives the strip: after
/// stripping guards, the optimized build must still return `IN_RANGE` for an in-range call AND
/// still revert for the over-threshold call, exactly as the baseline does. A stripped guard
/// would let the over-threshold call through. Skips on a missing toolchain.
fn state_guard_preserved(lang: &str, backend: Backend, filename: &str, source: &str) {
    let path = write_temp(filename, source);
    let dump = match backend.dump(&path, None) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("SKIP {lang} state-guard e2e (toolchain unavailable): {e}");
            return;
        }
    };
    let only: HashSet<Category> = [Category::Guard].into_iter().collect();
    let (_opt, spans) = strip_guards(&dump.instrs, &only);
    let base = backend.build(&path, &[], None).expect("baseline build");
    let opt = backend.build(&path, &spans, None).expect("optimized build");
    assert_eq!(
        base.creation_hex, base.reference_hex,
        "{lang}: baseline build must match the compiler reference bytecode"
    );

    let sig = "execute(uint256)";
    let mut expected = vec![0u8; 32];
    expected[24..].copy_from_slice(&IN_RANGE.to_be_bytes());

    // target <= threshold: both builds succeed and agree on the returned value.
    let (_g_b, out_base) = deploy_and_call(
        &base.creation_hex,
        owner_addr(),
        encode_call(sig, &[IN_RANGE]),
    );
    let (_g_o, out_opt) = deploy_and_call(
        &opt.creation_hex,
        owner_addr(),
        encode_call(sig, &[IN_RANGE]),
    );
    assert_eq!(
        out_base, expected,
        "{lang}: baseline lost the in-range return value"
    );
    assert_eq!(
        out_base, out_opt,
        "{lang}: optimized output diverged from baseline on an in-range call"
    );

    // target > threshold: the assert must fire on BOTH builds — the strip must not remove it.
    let base_over = deploy_then_call(
        &base.creation_hex,
        owner_addr(),
        owner_addr(),
        encode_call(sig, &[OVER_RANGE]),
    );
    assert!(
        !base_over.is_success(),
        "{lang}: baseline must revert when target exceeds the stored threshold"
    );
    let opt_over = deploy_then_call(
        &opt.creation_hex,
        owner_addr(),
        owner_addr(),
        encode_call(sig, &[OVER_RANGE]),
    );
    assert!(
        !opt_over.is_success(),
        "{lang}: the state-dependent threshold assert was stripped — the optimized build let an over-threshold call through"
    );
}

#[test]
fn vyper_state_dependent_guard_not_stripped() {
    state_guard_preserved("vyper", vy(), "g_vy_state.vy", VYPER_STATE);
}

#[test]
fn solidity_state_dependent_guard_not_stripped() {
    state_guard_preserved("solidity", so(), "g_so_state.sol", SOLIDITY_STATE);
}

// --- Vyper, trusted-caller (auth) ---

#[test]
fn vyper_add_strips() {
    win(
        "vyper",
        vy(),
        "g_vy_add.vy",
        &vyper(true, ARG2, "uint256", "a + b"),
        SIG2,
        &[3, 4],
        7,
        23631,
        23593,
    );
}

#[test]
fn vyper_sub_strips() {
    win(
        "vyper",
        vy(),
        "g_vy_sub.vy",
        &vyper(true, ARG2, "uint256", "a - b"),
        SIG2,
        &[7, 4],
        3,
        23634,
        23596,
    );
}

#[test]
fn vyper_mul_strips() {
    win(
        "vyper",
        vy(),
        "g_vy_mul.vy",
        &vyper(true, ARG2, "uint256", "a * b"),
        SIG2,
        &[3, 4],
        12,
        23671,
        23633,
    );
}

#[test]
fn vyper_div_strips() {
    win(
        "vyper",
        vy(),
        "g_vy_div.vy",
        &vyper(true, ARG2, "uint256", "a // b"),
        SIG2,
        &[12, 4],
        3,
        23627,
        23570,
    );
}

#[test]
fn vyper_convert_strips() {
    win(
        "vyper",
        vy(),
        "g_vy_cvt.vy",
        &vyper(true, "a: uint256", "uint128", "convert(a, uint128)"),
        SIG1,
        &[3],
        3,
        23479,
        23419,
    );
}

// --- Vyper, no auth wrapper ---

#[test]
fn vyper_add_no_auth() {
    no_auth(
        "vyper",
        vy(),
        "g_vy_add_na.vy",
        &vyper(false, ARG2, "uint256", "a + b"),
        SIG2,
        &[3, 4],
        7,
        21860,
        21860,
    );
}

#[test]
fn vyper_sub_no_auth() {
    no_auth(
        "vyper",
        vy(),
        "g_vy_sub_na.vy",
        &vyper(false, ARG2, "uint256", "a - b"),
        SIG2,
        &[7, 4],
        3,
        21860,
        21860,
    );
}

#[test]
fn vyper_mul_no_auth() {
    no_auth(
        "vyper",
        vy(),
        "g_vy_mul_na.vy",
        &vyper(false, ARG2, "uint256", "a * b"),
        SIG2,
        &[3, 4],
        12,
        21860,
        21860,
    );
}

#[test]
fn vyper_convert_no_auth() {
    no_auth(
        "vyper",
        vy(),
        "g_vy_cvt_na.vy",
        &vyper(false, "a: uint256", "uint128", "convert(a, uint128)"),
        SIG1,
        &[3],
        3,
        21510,
        21510,
    );
}

// --- Solidity, trusted-caller (auth) ---

#[test]
fn solidity_add_strips() {
    win(
        "solidity",
        so(),
        "g_so_add.sol",
        &solidity(true, SARG2, "return a + b;"),
        SIG2,
        &[3, 4],
        7,
        23843,
        23793,
    );
}

#[test]
fn solidity_sub_strips() {
    win(
        "solidity",
        so(),
        "g_so_sub.sol",
        &solidity(true, SARG2, "return a - b;"),
        SIG2,
        &[7, 4],
        3,
        23843,
        23793,
    );
}

#[test]
fn solidity_mul_strips() {
    win(
        "solidity",
        so(),
        "g_so_mul.sol",
        &solidity(true, SARG2, "return a * b;"),
        SIG2,
        &[3, 4],
        12,
        23859,
        23809,
    );
}

#[test]
fn solidity_div_strips() {
    win(
        "solidity",
        so(),
        "g_so_div.sol",
        &solidity(true, SARG2, "return a / b;"),
        SIG2,
        &[12, 4],
        3,
        23823,
        23757,
    );
}

#[test]
fn solidity_range_strips() {
    win(
        "solidity",
        so(),
        "g_so_rng.sol",
        &solidity(true, "uint256 a", "require(a < 256); return a;"),
        SIG1,
        &[3],
        3,
        23617,
        23545,
    );
}

// --- Solidity, no auth wrapper ---

#[test]
fn solidity_add_no_auth() {
    no_auth(
        "solidity",
        so(),
        "g_so_add_na.sol",
        &solidity(false, SARG2, "return a + b;"),
        SIG2,
        &[3, 4],
        7,
        21860,
        21860,
    );
}

#[test]
fn solidity_sub_no_auth() {
    no_auth(
        "solidity",
        so(),
        "g_so_sub_na.sol",
        &solidity(false, SARG2, "return a - b;"),
        SIG2,
        &[7, 4],
        3,
        21860,
        21860,
    );
}

#[test]
fn solidity_mul_no_auth() {
    no_auth(
        "solidity",
        so(),
        "g_so_mul_na.sol",
        &solidity(false, SARG2, "return a * b;"),
        SIG2,
        &[3, 4],
        12,
        21860,
        21860,
    );
}

#[test]
fn solidity_range_no_auth() {
    no_auth(
        "solidity",
        so(),
        "g_so_rng_na.sol",
        &solidity(false, "uint256 a", "require(a < 256); return a;"),
        SIG1,
        &[3],
        3,
        21510,
        21510,
    );
}
