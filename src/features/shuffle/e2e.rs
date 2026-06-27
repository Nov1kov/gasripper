//! End-to-end proof for the `shuffle` feature on a real EVM (revm, test-only dep).
//!
//! Compile a real Vyper contract, reschedule its stack-shuffle windows, re-assemble
//! creation bytecode via the sidecar (which relinks jumps after the length change),
//! deploy baseline + optimized, call `foo`, and assert the result is unchanged while
//! both the creation bytecode and the call gas drop. Unlike `guards`, this needs no
//! auth wrapper — the reschedule is always safe. Skips when Vyper is unavailable.

use crate::core::Category;
use crate::features::e2e_harness::{
    assert_preserved_and_smaller, encode_call, measure, write_temp,
};
use crate::sidecar::{Backend, Lang};

#[test]
fn vyper_shuffle_reschedules_with_gas_win() {
    // venom's stack scheduler leaves a non-minimal DUP/SWAP/POP window inside the
    // loop body; the reschedule pass rewrites it to a cheaper equivalent. The window
    // runs once per iteration, so the saving multiplies over the loop — a real call-
    // gas drop (not just a creation-bytecode shrink). Pinned to Vyper 0.4.3.
    let src = "@external\ndef foo(n: uint256) -> uint256:\n    s: uint256 = 0\n    for i: uint256 in range(n, bound=128):\n        s += i * i\n    return s\n";
    let path = write_temp("s_vy_shuffle_loop.vy", src);
    let calldata = encode_call("foo(uint256)", &[5]);
    let r = match measure(
        &Backend::new(Lang::Vyper),
        &path,
        Category::Shuffle,
        calldata,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP vyper shuffle e2e (toolchain unavailable): {e}");
            return;
        }
    };
    // sum of i*i for i in 0..5 = 0+1+4+9+16 = 30; call gas 22049 -> 22031 (saved 18
    // over 5 iterations), creation 169 -> 167 bytes.
    assert_preserved_and_smaller(&r, "vyper", 30, 22049, 22031);
}
