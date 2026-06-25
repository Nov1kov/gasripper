//! End-to-end proof for the `assert` feature on a real EVM (revm, test-only dep).
//!
//! Uses the shared harness ([`crate::features::e2e_harness`]). Vyper proves the gas
//! win via a `convert(a, uint128)` range-check; Solidity via a `require(a < 256)`
//! range-check whose INVERSE revert idiom the solc sidecar normalizes
//! (`_sym_revert_inv_*`). The auth-wrapped tests also check the strip kept the
//! caller guard (a stranger is still rejected); a no-auth layer shows the guard
//! strips without any auth wrapper. Each test SKIPS when its toolchain is absent.

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
def foo(a: uint256) -> uint128:
    assert msg.sender == self.owner
    return convert(a, uint128)
";

const SOLIDITY_CONTRACT: &str = "\
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;
contract C {
    address public owner;
    constructor() { owner = msg.sender; }
    function foo(uint256 a) external view returns (uint256) {
        require(msg.sender == owner);
        require(a < 256);
        return a;
    }
}
";

const VYPER_NO_AUTH: &str = "\
@external
def foo(a: uint256) -> uint128:
    return convert(a, uint128)
";

const SOLIDITY_NO_AUTH: &str = "\
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;
contract C {
    function foo(uint256 a) external pure returns (uint256) {
        require(a < 256);
        return a;
    }
}
";

fn assert_win_and_auth(lang: &str, source: &str, backend: Backend, filename: &str) {
    let path = write_temp(filename, source);
    let r = match measure(&backend, &path, Category::Assert, encode_call("foo(uint256)", &[3])) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIP {lang} assert e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert_win(&r, lang, 3); // foo(3) == 3, preserved, with less gas
    assert_rejects_stranger(&r.creation_opt, encode_call("foo(uint256)", &[3]));
}

fn assert_no_auth(lang: &str, source: &str, backend: Backend, filename: &str) {
    let path = write_temp(filename, source);
    let r = match measure(&backend, &path, Category::Assert, encode_call("foo(uint256)", &[3])) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIP {lang} assert no-auth e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert_preserved_and_smaller(&r, lang, 3);
}

#[test]
fn vyper_assert_strip_saves_gas_and_preserves_behavior() {
    assert_win_and_auth("vyper", VYPER_CONTRACT, Backend::new(Lang::Vyper), "gasripper_assert_e2e.vy");
}

#[test]
fn solidity_assert_strip_saves_gas_and_preserves_behavior() {
    assert_win_and_auth("solidity", SOLIDITY_CONTRACT, Backend::new(Lang::Solidity), "gasripper_assert_e2e.sol");
}

#[test]
fn vyper_assert_strips_without_auth_wrapper() {
    assert_no_auth("vyper", VYPER_NO_AUTH, Backend::new(Lang::Vyper), "gasripper_assert_noauth.vy");
}

#[test]
fn solidity_assert_strips_without_auth_wrapper() {
    assert_no_auth("solidity", SOLIDITY_NO_AUTH, Backend::new(Lang::Solidity), "gasripper_assert_noauth.sol");
}
