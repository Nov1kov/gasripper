//! End-to-end proof for the `math` feature on a real EVM (revm, test-only dep).
//!
//! Uses the shared harness ([`crate::features::e2e_harness`]). The `math` category
//! covers runs containing arithmetic (`ADD`/`SUB`/`MUL`/`DIV`/`MOD`/`EXP`/`SHL`).
//!
//! Solidity proves a real win: its ABI decoder validates calldata length with an
//! arithmetic (`SUB`/`SLT`) check that reads its inputs via `DUP` — a stack
//! identity, so it strips. Vyper documents the safety boundary: its gas-optimized
//! venom computes overflow checks by CONSUMING their operands, so they are not
//! stack identities and are correctly preserved. A third test drops the auth
//! wrapper. Each test SKIPS when its toolchain is unavailable.

use crate::core::Category;
use crate::features::e2e_harness::{
    assert_preserved_and_smaller, assert_rejects_stranger, assert_win, encode_call, measure, write_temp,
};
use crate::sidecar::{Backend, Lang};

use revm::primitives::U256;

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

/// Same body, no trusted-caller wrapper.
const SOLIDITY_NO_AUTH: &str = "\
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;
contract C {
    function foo(uint256 a, uint256 b) external pure returns (uint256) {
        return a + b;
    }
}
";

#[test]
fn solidity_math_strip_saves_gas_and_preserves_behavior() {
    // The ABI decoder's calldata-length check (`... SUB SLT ISZERO`) is math
    // category and a stack identity, so the inverse-idiom guard strips.
    let path = write_temp("gasripper_math_e2e.sol", SOLIDITY_CONTRACT);
    let calldata = encode_call("foo(uint256,uint256)", &[3, 4]);
    let r = match measure(&Backend::new(Lang::Solidity), &path, Category::Math, calldata) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP solidity math e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert_win(&r, "solidity", 7, 23842, 23811);
    assert_rejects_stranger(&r.creation_opt, encode_call("foo(uint256,uint256)", &[3, 4]));
}

#[test]
fn vyper_math_only_is_noop_overflow_is_assert_category() {
    // The `a + b` overflow guard IS now removed (residue strip), but its removable
    // run is the assertion `SWAP1 DUP2 LT revert JUMPI` — the arithmetic `ADD` is
    // kept as live logic, so the run carries no math opcode and classifies as
    // `assert`, not `math`. Hence `math`-only is a no-op here; the overflow check is
    // stripped once `assert` (or the default all) is enabled. `foo(3, 4)` stays 7.
    let path = write_temp("gasripper_math_e2e.vy", VYPER_CONTRACT);
    let calldata = encode_call("foo(uint256,uint256)", &[3, 4]);
    let r = match measure(&Backend::new(Lang::Vyper), &path, Category::Math, calldata) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP vyper math e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert_eq!(r.stripped, 0, "math-only: the overflow assertion classifies as `assert`, not `math`");
    assert_eq!(U256::from_be_slice(&r.out_base), U256::from(7u64), "foo(3,4) must be 7");
    assert_eq!(r.gas_opt, r.gas_base, "no-op build must not change gas");
    assert_eq!(r.gas_base, 23631, "no-op call gas drifted from pinned 23631 to {}", r.gas_base);
    assert_rejects_stranger(&r.creation_opt, encode_call("foo(uint256,uint256)", &[3, 4]));
}

#[test]
fn solidity_math_strips_without_auth_wrapper() {
    // No `require(msg.sender == owner)`: the math-category calldata-length check is
    // still stripped, the result preserved, and creation bytecode shrinks.
    let path = write_temp("gasripper_math_noauth.sol", SOLIDITY_NO_AUTH);
    let calldata = encode_call("foo(uint256,uint256)", &[3, 4]);
    let r = match measure(&Backend::new(Lang::Solidity), &path, Category::Math, calldata) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP solidity math no-auth e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert_preserved_and_smaller(&r, "solidity", 7, 21860, 21860);
}
