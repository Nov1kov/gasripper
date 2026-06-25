//! End-to-end proofs for the `guards` feature on a real EVM (revm, test-only dep).
//!
//! Each test uses the shared harness ([`crate::features::e2e_harness`]): compile a
//! real contract, strip the guard category, re-assemble creation bytecode, deploy
//! baseline + optimized, call `foo`, and assert the result is unchanged while gas
//! drops (and, for the auth contracts, that a stranger is still rejected). The cases
//! span both languages, the arithmetic guards (`+ - * /`), the range/cast guard
//! (`convert` / `require(a < 256)`), and with / without a trusted-caller wrapper.
//! Each test SKIPS when its toolchain is unavailable.

use crate::core::Category;
use crate::features::e2e_harness::{
    assert_preserved_and_smaller, assert_rejects_stranger, assert_win, encode_call, measure, write_temp,
};
use crate::sidecar::{Backend, Lang};

/// A Vyper `foo` returning `ret_expr`, optionally wrapped in a trusted-caller assert.
fn vyper(auth: bool, sig: &str, ret: &str, ret_expr: &str) -> String {
    if auth {
        format!("owner: public(address)\n\n@deploy\ndef __init__():\n    self.owner = msg.sender\n\n@external\ndef foo({sig}) -> {ret}:\n    assert msg.sender == self.owner\n    return {ret_expr}\n")
    } else {
        format!("@external\ndef foo({sig}) -> {ret}:\n    return {ret_expr}\n")
    }
}

/// A Solidity `foo` with the given `body`, optionally wrapped in a `require(msg.sender
/// == owner)`. Auth reads storage (`view`); without it the function is `pure`.
fn solidity(auth: bool, args: &str, body: &str) -> String {
    if auth {
        format!("// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\ncontract C {{\n    address public owner;\n    constructor() {{ owner = msg.sender; }}\n    function foo({args}) external view returns (uint256) {{\n        require(msg.sender == owner);\n        {body}\n    }}\n}}\n")
    } else {
        format!("// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\ncontract C {{\n    function foo({args}) external pure returns (uint256) {{\n        {body}\n    }}\n}}\n")
    }
}

/// Strip the guards, prove `foo(args) == expected` with a gas win, and confirm the
/// auth guard still rejects a stranger. Skips on a missing toolchain.
fn win(lang: &str, backend: Backend, filename: &str, source: &str, sig: &str, args: &[u64], expected: u64, gas_base: u64, gas_opt: u64) {
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
fn no_auth(lang: &str, backend: Backend, filename: &str, source: &str, sig: &str, args: &[u64], expected: u64, gas_base: u64, gas_opt: u64) {
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

fn vy() -> Backend { Backend::new(Lang::Vyper) }
fn so() -> Backend { Backend::new(Lang::Solidity) }

const ARG2: &str = "a: uint256, b: uint256";
const SARG2: &str = "uint256 a, uint256 b";
const SIG2: &str = "foo(uint256,uint256)";
const SIG1: &str = "foo(uint256)";

// --- Vyper, trusted-caller (auth) ---

#[test]
fn vyper_add_strips() {
    win("vyper", vy(), "g_vy_add.vy", &vyper(true, ARG2, "uint256", "a + b"), SIG2, &[3, 4], 7, 23631, 23593);
}

#[test]
fn vyper_sub_strips() {
    win("vyper", vy(), "g_vy_sub.vy", &vyper(true, ARG2, "uint256", "a - b"), SIG2, &[7, 4], 3, 23634, 23596);
}

#[test]
fn vyper_mul_strips() {
    win("vyper", vy(), "g_vy_mul.vy", &vyper(true, ARG2, "uint256", "a * b"), SIG2, &[3, 4], 12, 23671, 23633);
}

#[test]
fn vyper_div_strips() {
    win("vyper", vy(), "g_vy_div.vy", &vyper(true, ARG2, "uint256", "a // b"), SIG2, &[12, 4], 3, 23627, 23570);
}

#[test]
fn vyper_convert_strips() {
    win("vyper", vy(), "g_vy_cvt.vy", &vyper(true, "a: uint256", "uint128", "convert(a, uint128)"), SIG1, &[3], 3, 23479, 23419);
}

// --- Vyper, no auth wrapper ---

#[test]
fn vyper_add_no_auth() {
    no_auth("vyper", vy(), "g_vy_add_na.vy", &vyper(false, ARG2, "uint256", "a + b"), SIG2, &[3, 4], 7, 21860, 21860);
}

#[test]
fn vyper_sub_no_auth() {
    no_auth("vyper", vy(), "g_vy_sub_na.vy", &vyper(false, ARG2, "uint256", "a - b"), SIG2, &[7, 4], 3, 21860, 21860);
}

#[test]
fn vyper_mul_no_auth() {
    no_auth("vyper", vy(), "g_vy_mul_na.vy", &vyper(false, ARG2, "uint256", "a * b"), SIG2, &[3, 4], 12, 21860, 21860);
}

#[test]
fn vyper_convert_no_auth() {
    no_auth("vyper", vy(), "g_vy_cvt_na.vy", &vyper(false, "a: uint256", "uint128", "convert(a, uint128)"), SIG1, &[3], 3, 21510, 21510);
}

// --- Solidity, trusted-caller (auth) ---

#[test]
fn solidity_add_strips() {
    win("solidity", so(), "g_so_add.sol", &solidity(true, SARG2, "return a + b;"), SIG2, &[3, 4], 7, 23843, 23793);
}

#[test]
fn solidity_sub_strips() {
    win("solidity", so(), "g_so_sub.sol", &solidity(true, SARG2, "return a - b;"), SIG2, &[7, 4], 3, 23843, 23793);
}

#[test]
fn solidity_mul_strips() {
    win("solidity", so(), "g_so_mul.sol", &solidity(true, SARG2, "return a * b;"), SIG2, &[3, 4], 12, 23859, 23809);
}

#[test]
fn solidity_div_strips() {
    win("solidity", so(), "g_so_div.sol", &solidity(true, SARG2, "return a / b;"), SIG2, &[12, 4], 3, 23823, 23757);
}

#[test]
fn solidity_range_strips() {
    win("solidity", so(), "g_so_rng.sol", &solidity(true, "uint256 a", "require(a < 256); return a;"), SIG1, &[3], 3, 23617, 23545);
}

// --- Solidity, no auth wrapper ---

#[test]
fn solidity_add_no_auth() {
    no_auth("solidity", so(), "g_so_add_na.sol", &solidity(false, SARG2, "return a + b;"), SIG2, &[3, 4], 7, 21860, 21860);
}

#[test]
fn solidity_sub_no_auth() {
    no_auth("solidity", so(), "g_so_sub_na.sol", &solidity(false, SARG2, "return a - b;"), SIG2, &[7, 4], 3, 21860, 21860);
}

#[test]
fn solidity_mul_no_auth() {
    no_auth("solidity", so(), "g_so_mul_na.sol", &solidity(false, SARG2, "return a * b;"), SIG2, &[3, 4], 12, 21860, 21860);
}

#[test]
fn solidity_range_no_auth() {
    no_auth("solidity", so(), "g_so_rng_na.sol", &solidity(false, "uint256 a", "require(a < 256); return a;"), SIG1, &[3], 3, 21510, 21510);
}
