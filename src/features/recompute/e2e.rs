//! End-to-end proof for the `recompute` feature on a real EVM (revm, test-only dep).
//!
//! Compile a real contract whose hot path executes a `<cheap-nullary-op> DUP1` (Vyper's
//! venom emits `ADDRESS DUP1` per loop iteration; solc emits the `CALLVALUE DUP1`
//! non-payable guard plus `PUSH0 DUP1` idioms once per call), recompute the `DUP1` into a
//! second copy of the opcode, re-assemble creation bytecode via the sidecar, deploy
//! baseline + optimized, call `f`, and assert the result is unchanged while the call gas
//! drops and the creation bytecode keeps the **same size** (the rewrite swaps one
//! single-byte opcode for another). Like `shuffle`/`involution` this needs no auth
//! wrapper — the rewrite is always safe. Skips when a toolchain is unavailable.

use crate::core::Category;
use crate::features::e2e_harness::{assert_same_size_cheaper, encode_call, measure, write_temp};
use crate::sidecar::{Backend, Lang};

#[test]
fn vyper_chainid_dup1_recomputed_with_gas_win() {
    // venom 0.4.3 (GAS) keeps `CHAINID DUP1 CHAINID` in the loop body when an environment
    // value is read twice per iteration, and does NOT hoist it; recompute rewrites the
    // `DUP1` to a second `CHAINID` (-1 gas), and the loop compounds the saving past the
    // EIP-7623 calldata floor. `chain.id == 1` on revm mainnet, so the result is clean.
    let src = "@external\n@view\ndef f(n: uint256) -> uint256:\n    s: uint256 = 0\n    for i: uint256 in range(n, bound=128):\n        s += chain.id * chain.id\n    return s\n";
    let path = write_temp("s_vy_recompute_chainid.vy", src);
    let calldata = encode_call("f(uint256)", &[5]);
    let r = match measure(
        &Backend::new(Lang::Vyper),
        &path,
        Category::Recompute,
        calldata,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP vyper recompute e2e (toolchain unavailable): {e}");
            return;
        }
    };
    // sum of (chain.id * chain.id) == 1 per iteration over 5 iterations = 5; one
    // CHAINID DUP1 -> CHAINID per iteration saves 5 gas over the loop.
    assert_same_size_cheaper(&r, "vyper", 5, 22099, 22094);
}

#[test]
fn solidity_callvalue_dup1_recomputed_with_gas_win() {
    // solc 0.8.x emits the non-payable guard `CALLVALUE DUP1 ISZERO …`, executed once per
    // call on the happy path; recompute rewrites that `DUP1` to a second `CALLVALUE`. The
    // loop keeps the call's execution gas above the EIP-7623 calldata floor so the saving
    // shows (the other recomputed `PUSH0 DUP1` idioms sit in unexecuted revert blocks).
    let src = "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\ncontract C {\n    function f(uint256 n) external pure returns (uint256 s) {\n        for (uint256 i = 0; i < n; i++) { s += i; }\n    }\n}\n";
    let path = write_temp("s_sol_recompute_callvalue.sol", src);
    let calldata = encode_call("f(uint256)", &[5]);
    let r = match measure(
        &Backend::new(Lang::Solidity),
        &path,
        Category::Recompute,
        calldata,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP solidity recompute e2e (toolchain unavailable): {e}");
            return;
        }
    };
    // f(5) = 0+1+2+3+4 = 10; one CALLVALUE DUP1 -> CALLVALUE CALLVALUE on the call path.
    assert_same_size_cheaper(&r, "solidity", 10, 22103, 22102);
}
