//! Progressive end-to-end proofs: enabling more passes lowers call gas monotonically.
//!
//! Each test compiles one real contract, then measures call gas on a real EVM (revm) with a
//! growing set of enabled passes — `guards`, then `guards`+`shuffle`, and so on. The result
//! is identical at every stage while gas only falls, showing each pass's incremental win on
//! the same hot path. The per-stage gas is pinned so any drift fails. Skips when the
//! toolchain is unavailable.

use std::collections::HashSet;

use revm::primitives::U256;

use crate::core::Category;
use crate::features::e2e_harness::{encode_call, measure_set, write_temp};
use crate::sidecar::{Backend, Lang};

/// One cumulative stage: a label, the passes enabled at this point, and the pinned call gas.
struct Stage {
    label: &'static str,
    categories: &'static [Category],
    gas: u64,
}

/// Measure each cumulative `stages` entry on a real EVM and assert the call result is
/// `expected` throughout, call gas never rises from one stage to the next, the last stage is
/// strictly cheaper than the first, and each stage's gas matches its pin. Returns `Err` only
/// when the toolchain is unavailable (caller SKIPs); panics on a violated invariant.
fn assert_descending(
    backend: &Backend,
    path: &str,
    calldata: &[u8],
    expected: u64,
    stages: &[Stage],
) -> Result<(), String> {
    let mut prev: Option<u64> = None;
    let mut first = 0u64;
    for stage in stages {
        let set: HashSet<Category> = stage.categories.iter().copied().collect();
        let r = measure_set(backend, path, &set, calldata.to_vec())?;
        assert_eq!(
            U256::from_be_slice(&r.out_opt),
            U256::from(expected),
            "{}: optimization changed the call result",
            stage.label
        );
        if let Some(p) = prev {
            assert!(
                r.gas_opt <= p,
                "enabling {} must not raise call gas: {p} -> {}",
                stage.label,
                r.gas_opt
            );
        } else {
            first = r.gas_opt;
        }
        assert_eq!(
            r.gas_opt, stage.gas,
            "{}: call gas drifted from pinned {} to {}",
            stage.label, stage.gas, r.gas_opt
        );
        tracing::info!("progressive [{}]: call gas {}", stage.label, r.gas_opt);
        prev = Some(r.gas_opt);
    }
    let last = prev.expect("at least one stage");
    assert!(
        last < first,
        "the full pass set must be strictly cheaper than the first stage: {first} -> {last}"
    );
    Ok(())
}

// A Vyper loop body that exercises four passes on its hot path every iteration: a checked
// `+`/`*` overflow guard (`guards`), `~(~x)` (`involution`), `chain.id * chain.id` reading the
// same env value twice (`recompute`), and the `x | y | i` reduction venom leaves a non-minimal
// stack window for (`shuffle`). `chain.id == 1` on revm mainnet, so the arithmetic stays clean.
const VYPER_CONTRACT: &str = r#"
@external
@view
def f(x: uint256, y: uint256, n: uint256) -> uint256:
    s: uint256 = 0
    for i: uint256 in range(n, bound=128):
        a: uint256 = ~(~x)
        b: uint256 = chain.id * chain.id
        c: uint256 = x | y | i
        s += a + b + c
    return s
"#;

#[test]
fn vyper_gas_falls_as_passes_enable() {
    let path = write_temp("s_vy_progressive.vy", VYPER_CONTRACT);
    let calldata = encode_call("f(uint256,uint256,uint256)", &[3, 5, 5]);
    // f(3,5,5) = sum over i in 0..5 of (a + b + c) with a=~(~3)=3, b=chain.id^2=1, c=3|5|i = 55.
    // Each pass shaves the same hot loop: guards 22374 -> +shuffle 22326 (-48) -> +involution
    // 22296 (-30) -> +recompute 22291 (-5). cmpnorm finds nothing here (the comparison's SWAP1
    // is absorbed by a shuffle window — see vyper_cmpnorm_adds_a_step for where it does fire).
    let stages = [
        Stage {
            label: "guards",
            categories: &[Category::Guard],
            gas: 22374,
        },
        Stage {
            label: "+shuffle",
            categories: &[Category::Guard, Category::Shuffle],
            gas: 22326,
        },
        Stage {
            label: "+involution",
            categories: &[Category::Guard, Category::Shuffle, Category::Involution],
            gas: 22296,
        },
        Stage {
            label: "+recompute",
            categories: &[
                Category::Guard,
                Category::Shuffle,
                Category::Involution,
                Category::Recompute,
            ],
            gas: 22291,
        },
    ];
    let expected = 55;
    if let Err(e) = assert_descending(
        &Backend::new(Lang::Vyper),
        &path,
        &calldata,
        expected,
        &stages,
    ) {
        tracing::warn!("SKIP vyper progressive e2e (toolchain unavailable): {e}");
    }
}

// A Vyper loop comparing two freshly-computed products `(x * i) < (y * i)`, which venom lowers
// with a `SWAP1 LT` not shadowed by a shuffle window — so `cmpnorm` fires on top of `guards`
// (the multiply/add overflow checks) on the hot path.
const VYPER_CMP_CONTRACT: &str = r#"
@external
@view
def f(x: uint256, y: uint256, n: uint256) -> uint256:
    s: uint256 = 0
    for i: uint256 in range(n, bound=128):
        if (x * i) < (y * i):
            s += 1
    return s
"#;

#[test]
fn vyper_cmpnorm_adds_a_step() {
    let path = write_temp("s_vy_progressive_cmp.vy", VYPER_CMP_CONTRACT);
    let calldata = encode_call("f(uint256,uint256,uint256)", &[2, 3, 5]);
    // f(2,3,5) = 4 (2*i < 3*i for i in 1..=4). guards strips the per-iteration overflow checks;
    // cmpnorm then folds the loop's `SWAP1 LT` to `GT`, -3 gas/iteration over 5 iterations.
    let stages = [
        Stage {
            label: "guards",
            categories: &[Category::Guard],
            gas: 22355,
        },
        Stage {
            label: "+cmpnorm",
            categories: &[Category::Guard, Category::CmpNorm],
            gas: 22340,
        },
    ];
    let expected = 4;
    if let Err(e) = assert_descending(
        &Backend::new(Lang::Vyper),
        &path,
        &calldata,
        expected,
        &stages,
    ) {
        tracing::warn!("SKIP vyper cmpnorm progressive e2e (toolchain unavailable): {e}");
    }
}

// A Solidity function whose hot path feeds three passes: the non-payable `CALLVALUE DUP1` guard
// (`recompute`), the loop's checked `+` overflow guard (`guards`), and the `1 << 160` address-clean
// mask on `to` (`foldshift`). guards subsumes recompute's `CALLVALUE` target (it strips the whole
// non-payable check), so the stages are ordered recompute -> +guards -> +foldshift to show each
// addition lower gas. This is also the regression guard for the guards+foldshift InvalidJump bug:
// foldshift folds the `PUSH 0x4e487b71 PUSH 0xe0 SHL` Panic-selector inside an inverse guard's
// inline revert block, which guards' DCE drops — the solc sidecar must drop it rather than strand
// the folded push (see scripts/solc_sidecar.py::_apply_edits).
const SOLIDITY_CONTRACT: &str = r#"
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;
contract C {
    mapping(address => uint256) bal;
    function f(address to, uint256 n) external returns (uint256 s) {
        for (uint256 i = 0; i < n; i++) { s += i; }
        bal[to] += s;
        s = bal[to];
    }
}
"#;

#[test]
fn solidity_gas_falls_as_passes_enable() {
    let path = write_temp("s_sol_progressive.sol", SOLIDITY_CONTRACT);
    let calldata = encode_call("f(address,uint256)", &[0xBEEF, 5]);
    // f(0xBEEF,5) = 0+1+2+3+4 = 10, stored to bal[to] and read back. recompute saves the call's
    // CALLVALUE DUP1 (-1); enabling guards strips the per-iteration overflow checks and the whole
    // non-payable guard (subsuming recompute's target); foldshift then folds the address mask.
    let stages = [
        Stage {
            label: "recompute",
            categories: &[Category::Recompute],
            gas: 44801,
        },
        Stage {
            label: "+guards",
            categories: &[Category::Recompute, Category::Guard],
            gas: 44562,
        },
        Stage {
            label: "+foldshift",
            categories: &[Category::Recompute, Category::Guard, Category::FoldShift],
            gas: 44550,
        },
    ];
    let expected = 10;
    if let Err(e) = assert_descending(
        &Backend::new(Lang::Solidity),
        &path,
        &calldata,
        expected,
        &stages,
    ) {
        tracing::warn!("SKIP solidity progressive e2e (toolchain unavailable): {e}");
    }
}
