//! End-to-end gas proof for the `foldshift` pass on a real EVM (revm).
//!
//! Solidity materializes the address-cleaning mask `1 << 160` as `PUSH1 0x01 PUSH1 0xa0
//! SHL` on every address argument. A function with an `address` parameter runs that idiom
//! on its hot path, so folding it lowers call gas — at the cost of a wider literal in the
//! creation bytecode (the size-for-gas trade this feature makes).

use crate::core::Category;
use crate::features::e2e_harness::{assert_cheaper_larger, encode_call, measure, write_temp};
use crate::sidecar::{Backend, Lang};

// Two address arguments (`to`, `msg.sender`) are each cleaned with `1 << 160`, so the fold
// sits on the call's hot path.
const SOLIDITY_CONTRACT: &str = "\
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;
contract C {
    mapping(address => uint256) public bal;
    function transfer(address to, uint256 amt) external returns (bool) {
        bal[msg.sender] -= amt;
        bal[to] += amt;
        return true;
    }
}";

#[test]
fn solidity_foldshift() {
    let path = write_temp("gasripper_foldshift_e2e.sol", SOLIDITY_CONTRACT);
    let calldata = encode_call("transfer(address,uint256)", &[0xABCD, 0]);
    let r = match measure(
        &Backend::new(Lang::Solidity),
        &path,
        Category::FoldShift,
        calldata,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP solidity foldshift e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert_cheaper_larger(&r, "solidity", 1, 26518, 26506);
}
