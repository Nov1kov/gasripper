//! End-to-end gas proof for the `cmpnorm` pass on a real EVM (revm, test-only dep).
//!
//! Vyper's venom compares two independent freshly-computed subexpressions by landing them
//! on the stack and emitting `SWAP1 LT` rather than re-scheduling the producers. A loop
//! body that compares `(x * i) < (y * i)` runs that idiom every iteration, so folding it
//! to a single `GT` lowers call gas (and shrinks the bytecode by the dropped `SWAP1`). The
//! loop keeps execution gas above the EIP-7623 calldata floor so the per-iteration saving
//! shows. solc never emits the idiom, so this proof is Vyper-only.

use crate::core::Category;
use crate::features::e2e_harness::{assert_smaller_cheaper, encode_call, measure, write_temp};
use crate::sidecar::{Backend, Lang};

#[test]
fn vyper_swap1_lt_folded_with_gas_win() {
    // venom 0.4.3 (GAS) emits `SWAP1 LT` for `(x * i) < (y * i)` in the loop body; cmpnorm
    // folds it to a single `GT` (-3 gas), compounded over the loop past the calldata floor.
    let src = "@external\n@view\ndef f(x: uint256, y: uint256, n: uint256) -> uint256:\n    s: uint256 = 0\n    for i: uint256 in range(n, bound=128):\n        if (x * i) < (y * i):\n            s += 1\n    return s\n";
    let path = write_temp("s_vy_cmpnorm_lt.vy", src);
    let calldata = encode_call("f(uint256,uint256,uint256)", &[2, 3, 5]);
    let r = match measure(
        &Backend::new(Lang::Vyper),
        &path,
        Category::CmpNorm,
        calldata,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP vyper cmpnorm e2e (toolchain unavailable): {e}");
            return;
        }
    };
    // f(2,3,5): 2*i < 3*i holds for i in 1..=4 (false at i=0) = 4; one SWAP1 LT -> GT per
    // iteration saves 3 gas, executed over 5 iterations = 15 gas (22783 -> 22768).
    assert_smaller_cheaper(&r, "vyper", 4, 22783, 22768);
}
