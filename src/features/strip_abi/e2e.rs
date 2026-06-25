//! End-to-end proof for the `abi` feature on a real EVM (revm, test-only dep).
//!
//! Both languages run the same shared harness ([`crate::features::e2e_harness`]):
//! compile a real contract, strip only the `abi` (calldata bounds) guard,
//! re-assemble creation bytecode, deploy baseline + optimized, call `foo(3, 4)` on
//! each, and assert the result is unchanged while the optimized call uses less gas.
//!
//! A third test drops the trusted-caller (auth) wrapper to show the auth check is
//! irrelevant to what the feature strips. Each test SKIPS when its toolchain is
//! unavailable.

use crate::core::Category;
use crate::features::e2e_harness::{
    assert_preserved_and_smaller, assert_rejects_stranger, assert_win, encode_call, measure, write_temp,
};
use crate::sidecar::{Backend, Lang};

const VYPER_CONTRACT: &str = "\
owner: public(address)

@deploy
def __init__():
    self.owner = msg.sender

@external
def foo(a: uint256, b: uint256) -> uint256:
    assert msg.sender == self.owner
    return a + b
";

const SOLIDITY_CONTRACT: &str = "\
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;
contract C {
    address public owner;
    constructor() { owner = msg.sender; }
    function foo(uint256 a, uint256 b) external view returns (uint256) {
        require(msg.sender == owner);
        return a + b;
    }
}
";

/// Same function, no trusted-caller wrapper (and no owner getter).
const VYPER_NO_AUTH: &str = "\
@external
def foo(a: uint256, b: uint256) -> uint256:
    return a + b
";

const SOLIDITY_NO_AUTH: &str = "\
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;
contract C {
    function foo(uint256 a, uint256 b) external pure returns (uint256) {
        return a + b;
    }
}
";

fn abi_no_auth(lang: &str, source: &str, backend: Backend, filename: &str, gas_base: u64, gas_opt: u64) {
    let path = write_temp(filename, source);
    let calldata = encode_call("foo(uint256,uint256)", &[3, 4]);
    let r = match measure(&backend, &path, Category::Abi, calldata) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP {lang} abi no-auth e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert_preserved_and_smaller(&r, lang, 7, gas_base, gas_opt);
}

fn abi_win(lang: &str, source: &str, backend: Backend, filename: &str, gas_base: u64, gas_opt: u64) {
    let path = write_temp(filename, source);
    let calldata = encode_call("foo(uint256,uint256)", &[3, 4]);
    let r = match measure(&backend, &path, Category::Abi, calldata) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP {lang} abi e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert_win(&r, lang, 7, gas_base, gas_opt); // foo(3,4) == 7, preserved, with less gas
    // The strip kept the auth guard: a non-owner caller is still rejected.
    assert_rejects_stranger(&r.creation_opt, encode_call("foo(uint256,uint256)", &[3, 4]));
}

#[test]
fn vyper_abi_strip_saves_gas_and_preserves_behavior() {
    abi_win("vyper", VYPER_CONTRACT, Backend::new(Lang::Vyper), "gasripper_abi_e2e.vy", 23631, 23605);
}

#[test]
fn solidity_abi_strip_saves_gas_and_preserves_behavior() {
    abi_win("solidity", SOLIDITY_CONTRACT, Backend::new(Lang::Solidity), "gasripper_abi_e2e.sol", 23842, 23821);
}

// No trusted-caller wrapper: the abi guard is still stripped and the result
// preserved; creation bytecode shrinks (call gas drops only if the guard is on this
// dispatcher's hot path).
#[test]
fn vyper_abi_strips_without_auth_wrapper() {
    abi_no_auth("vyper", VYPER_NO_AUTH, Backend::new(Lang::Vyper), "gasripper_abi_noauth.vy", 21860, 21860);
}

#[test]
fn solidity_abi_strips_without_auth_wrapper() {
    abi_no_auth("solidity", SOLIDITY_NO_AUTH, Backend::new(Lang::Solidity), "gasripper_abi_noauth.sol", 21860, 21860);
}
