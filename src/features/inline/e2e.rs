//! End-to-end proof for the `inline` feature via the shared harness.
//!
//! Each contract has a self-contained internal helper called from two externals — exactly the
//! shape venom keeps as a separate callable block — and exercises one inline strategy:
//!   * `_pick` / `_advance_to`: a single-merge diamond (`if`/`else`) → DE-THREADED (the return
//!     address and the merge's return `JUMP` eliminated, the two arms joined at a fall-through);
//!   * `_hi`: a straight-line tail-return body → DE-THREADED (the return indirection eliminated);
//!   * `_clamp`: a nested branch (not a single-merge diamond) → relocated VERBATIM (the return
//!     convention kept).
//! Every case relocates the body into both call sites: behavior is unchanged and the per-call
//! indirection is removed, lowering gas. Calling `a` runs the helper once per loop iteration, so
//! the saving is paid on a hot path — enough execution gas to clear the EIP-7623 calldata floor.

use std::collections::HashSet;

use crate::core::Category;
use crate::features::e2e_harness::{
    assert_cheaper_larger, assert_same_size_cheaper, assert_smaller_cheaper, encode_call, measure,
    measure_set, measure_with, write_temp,
};
use crate::sidecar::{Backend, Lang};
use revm::primitives::U256;

// `_pick` is a value-returning single-merge diamond (`if x<y: return x; return y`) with TWO call
// sites, so venom keeps it as a separate callable block. The diamond de-thread eliminates the
// return indirection on both arms (the merge's address-raising `SWAP1` and the dynamic return JUMP
// dropped), so it beats the verbatim relocation on both gas and size.
const VYPER_CONTRACT: &str = r#"
@internal
@pure
def _pick(x: uint256, y: uint256) -> uint256:
    if x < y:
        return x
    return y

@external
@view
def a(x: uint256, y: uint256, n: uint256) -> uint256:
    s: uint256 = 0
    for i: uint256 in range(n, bound=128):
        s += self._pick(x, y)
    return s

@external
@view
def b(x: uint256, y: uint256) -> uint256:
    return self._pick(y, x)
"#;

#[test]
fn vyper_inline_dethreads_a_value_returning_diamond() {
    let path = write_temp("gasripper_inline_e2e.vy", VYPER_CONTRACT);
    // a(3, 4, 5) = sum over 5 iterations of min(3, 4) = 5 * 3 = 15.
    let calldata = encode_call("a(uint256,uint256,uint256)", &[3, 4, 5]);
    let r = match measure(
        &Backend::new(Lang::Vyper),
        &path,
        Category::Inline,
        calldata,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP vyper inline e2e (toolchain unavailable): {e}");
            return;
        }
    };
    // De-threading `_pick` removes 37 gas/iteration (x5 = 185); the two de-threaded copies happen
    // to match the original function plus its call/return glue, so the bytecode is unchanged (229).
    assert_same_size_cheaper(&r, "vyper", 15, 22440, 22255);
}

// `_advance_to` is a VOID single-merge diamond writing transient storage (the user's case): an
// `if`/`else` with no return value, no trailing code, an empty merge. Its body exceeds the default
// size threshold, so the e2e raises it. The contract returns `t_ops_meta` (read back within the tx)
// so the result is observable.
const VYPER_DIAMOND_CONTRACT: &str = r#"
t_ops_meta: transient(uint256)

@internal
def _advance_to(new_offset: uint256, size: uint256):
    if new_offset >= size:
        self.t_ops_meta = (1 << 22) | (size << 11)
    else:
        self.t_ops_meta = (size << 11) | new_offset

@external
def a(x: uint256, y: uint256, n: uint256) -> uint256:
    for i: uint256 in range(n, bound=128):
        self._advance_to(x, y)
    return self.t_ops_meta

@external
def b(x: uint256, y: uint256) -> uint256:
    self._advance_to(y, x)
    return self.t_ops_meta
"#;

#[test]
fn vyper_inline_dethreads_a_void_diamond() {
    let backend = Backend::new(Lang::Vyper);
    let path = write_temp("gasripper_inline_diamond.vy", VYPER_DIAMOND_CONTRACT);
    // ELSE path: a(3,4,5) -> 3<4 every iteration -> t_ops_meta = (4<<11)|3 = 8195. The diamond
    // de-thread removes 34 gas/iteration (x5 = 170); duplicating the diamond body grows the
    // bytecode (236 -> 254), a size-for-gas trade like the verbatim relocation.
    let calldata = encode_call("a(uint256,uint256,uint256)", &[3, 4, 5]);
    let r = match measure_with(&backend, &path, Category::Inline, calldata, 40) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP vyper inline diamond e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert_cheaper_larger(&r, "vyper", 8195, 22888, 22718);

    // THEN path correctness: a(5,4,1) -> 5>=4 -> t_ops_meta = (1<<22)|(4<<11) = 4202496. The
    // de-threaded then-arm must compute the identical value as the baseline.
    let then = encode_call("a(uint256,uint256,uint256)", &[5, 4, 1]);
    let r2 = measure_with(&backend, &path, Category::Inline, then, 40)
        .expect("the toolchain was available a moment ago");

    assert_eq!(
        U256::from_be_slice(&r2.out_opt),
        U256::from(4202496u64),
        "vyper: diamond then-arm result wrong"
    );
    assert_eq!(
        r2.out_base, r2.out_opt,
        "vyper: diamond then-arm changed the result"
    );
}

// `_hi` is a straight-line tail-return helper (no branches), so inlining ELIMINATES the return
// indirection entirely: the `pushsym ret`, the body's return `JUMP`, and the stack shuffle that
// raised the return address are all dropped, and the body falls through to the continuation.
const VYPER_TAIL_CONTRACT: &str = r#"
@internal
@pure
def _hi(x: uint256, y: uint256) -> uint256:
    return (x | y) & 255

@external
@view
def a(x: uint256, y: uint256, n: uint256) -> uint256:
    s: uint256 = 0
    for i: uint256 in range(n, bound=128):
        s += self._hi(x, y)
    return s

@external
@view
def b(x: uint256, y: uint256) -> uint256:
    return self._hi(y, x)
"#;

#[test]
fn vyper_inline_dethreads_tail_return() {
    let path = write_temp("gasripper_inline_tail.vy", VYPER_TAIL_CONTRACT);
    // a(3,4,5) = sum over 5 iterations of (3 | 4) & 255 = 5 * 7 = 35.
    let calldata = encode_call("a(uint256,uint256,uint256)", &[3, 4, 5]);
    let r = match measure(
        &Backend::new(Lang::Vyper),
        &path,
        Category::Inline,
        calldata,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP vyper inline tail-return e2e (toolchain unavailable): {e}");
            return;
        }
    };
    // Eliminating the return indirection removes 15 gas/iteration (x5 = 75), and the de-threaded
    // bodies are small enough that even two copies plus the deleted definition shrink the bytecode.
    assert_smaller_cheaper(&r, "vyper", 35, 22285, 22210);
}

// `_clamp` has a NESTED branch (an `if` inside an `if`), so it is neither a tail-return body nor a
// single-merge diamond — the inline pass relocates it VERBATIM, keeping the return convention.
const VYPER_VERBATIM_CONTRACT: &str = r#"
@internal
@pure
def _clamp(x: uint256, y: uint256) -> uint256:
    if x < y:
        if x < 100:
            return 100
        return x
    return y

@external
@view
def a(x: uint256, y: uint256, n: uint256) -> uint256:
    s: uint256 = 0
    for i: uint256 in range(n, bound=128):
        s += self._clamp(x, y)
    return s

@external
@view
def b(x: uint256, y: uint256) -> uint256:
    return self._clamp(y, x)
"#;

#[test]
fn vyper_inline_relocates_a_nested_branch_verbatim() {
    let path = write_temp("gasripper_inline_verbatim.vy", VYPER_VERBATIM_CONTRACT);
    // a(3,4,5): 3<4 and 3<100 -> 100 every iteration, x5 = 500.
    let calldata = encode_call("a(uint256,uint256,uint256)", &[3, 4, 5]);
    let r = match measure_with(
        &Backend::new(Lang::Vyper),
        &path,
        Category::Inline,
        calldata,
        40,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP vyper inline verbatim e2e (toolchain unavailable): {e}");
            return;
        }
    };
    // Verbatim relocation removes only the 12-gas call indirection per iteration (x5 = 60) and
    // duplicates the body across the two call sites (240 -> 261 bytes) — the return JUMP is kept.
    assert_cheaper_larger(&r, "vyper", 500, 22575, 22515);
}

// The two call sites venom needs to keep `_advance_to` separate are BOTH in one function (no loop),
// so this proves inline keys on the call-site COUNT, not a loop — and the diamond de-thread still
// fires on each site. Two storage-writing calls plus the dispatcher push execution above the
// EIP-7623 calldata floor, so the (small, loop-free) saving stays visible.
const VYPER_TWICE_CONTRACT: &str = r#"
t_ops_meta: transient(uint256)

@internal
def _advance_to(new_offset: uint256, size: uint256):
    if new_offset >= size:
        self.t_ops_meta = (1 << 22) | (size << 11)
    else:
        self.t_ops_meta = (size << 11) | new_offset

@external
def a(x: uint256, y: uint256) -> uint256:
    self._advance_to(x, y)
    r: uint256 = self.t_ops_meta
    self._advance_to(y, x)
    return r + self.t_ops_meta
"#;

#[test]
fn vyper_inline_dethreads_two_call_sites_in_one_function() {
    let path = write_temp("gasripper_inline_twice.vy", VYPER_TWICE_CONTRACT);
    // a(3,4): call 1 = _advance_to(3,4) -> else -> (4<<11)|3 = 8195 (r); call 2 = _advance_to(4,3)
    // -> then -> (1<<22)|(3<<11) = 4200448; returns r + meta = 8195 + 4200448 = 4208643.
    let calldata = encode_call("a(uint256,uint256)", &[3, 4]);
    let r = match measure_with(&Backend::new(Lang::Vyper), &path, Category::Inline, calldata, 40) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP vyper inline twice-in-one e2e (toolchain unavailable): {e}");
            return;
        }
    };
    // De-threading both sites (one else arm, one then arm) saves 46 gas across the two calls; the
    // two relocated diamond copies grow the bytecode (182 -> 200).
    assert_cheaper_larger(&r, "vyper", 4208643, 22035, 21989);
}

#[test]
fn vyper_inline_composes_with_every_pass() {
    // Inline optimizes each relocated body with the other passes, so running the FULL pass set
    // must still produce correct bytecode (a broken inline would corrupt jumps -> revm failure)
    // and never cost more than inline alone (22255). This guards the body-composition path.
    let path = write_temp("gasripper_inline_compose.vy", VYPER_CONTRACT);
    let all: HashSet<Category> = [
        Category::Inline,
        Category::Guard,
        Category::Shuffle,
        Category::Involution,
        Category::Recompute,
        Category::FoldShift,
        Category::CmpNorm,
    ]
    .into_iter()
    .collect();
    let calldata = encode_call("a(uint256,uint256,uint256)", &[3, 4, 5]);
    let r = match measure_set(&Backend::new(Lang::Vyper), &path, &all, calldata) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("SKIP vyper inline compose e2e (toolchain unavailable): {e}");
            return;
        }
    };

    assert_eq!(
        U256::from_be_slice(&r.out_opt),
        U256::from(15u64),
        "vyper: inline+all changed the result"
    );
    assert_eq!(
        r.out_base, r.out_opt,
        "vyper: optimized output must match baseline under the full pass set"
    );
    assert!(
        r.gas_opt <= 22255,
        "vyper: the full pass set must not cost more than inline alone: {}",
        r.gas_opt
    );
}
