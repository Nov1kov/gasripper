//! End-to-end proof for the `involution` feature on a real EVM (revm, test-only dep).
//!
//! Compile a real Vyper contract whose loop body double-complements the loop variable,
//! cancel the `NOT NOT` run, re-assemble creation bytecode via the sidecar (which
//! relinks jumps after the length change), deploy baseline + optimized, call `f`, and
//! assert the result is unchanged while both the creation bytecode and the call gas
//! drop. Like `shuffle`, this needs no auth wrapper — the cancellation is always safe.
//! The window runs once per iteration, so the saving clears the EIP-7623 calldata
//! floor that hides a single op's gas. Skips when Vyper is unavailable.

use crate::core::Category;
use crate::features::e2e_harness::{assert_preserved_and_smaller, encode_call, measure, write_temp};
use crate::sidecar::{Backend, Lang};

#[test]
fn vyper_double_not_cancelled_with_gas_win() {
    // venom 0.4.3 (GAS) does not fold a double bitwise complement: `~(~i)` lowers to a
    // literal `NOT NOT` in the loop body, run once per iteration. Cancelling it removes
    // two `NOT`s (3 gas each) from the hot path; over the loop the saving compounds past
    // the calldata floor into a measurable call-gas drop. Pinned to Vyper 0.4.3.
    let src = "@external\ndef f(n: uint256) -> uint256:\n    s: uint256 = 0\n    for i: uint256 in range(n, bound=128):\n        s += ~(~i)\n    return s\n";
    let path = write_temp("s_vy_involution_loopnot.vy", src);
    let calldata = encode_call("f(uint256)", &[5]);
    let r = match measure(&Backend::new(Lang::Vyper), &path, Category::Involution, calldata) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP vyper involution e2e (toolchain unavailable): {e}");
            return;
        }
    };
    // sum of ~(~i) == i for i in 0..5 = 0+1+2+3+4 = 10; two NOTs leave the loop body,
    // saved 6 gas per iteration over 5 iterations.
    assert_preserved_and_smaller(&r, "vyper", 10, 21784, 21754);
}
