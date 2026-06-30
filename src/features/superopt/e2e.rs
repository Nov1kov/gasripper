//! End-to-end proofs for the `superopt` feature on a real EVM (revm, test-only dep).
//!
//! Two flavors:
//!
//! 1. [`add_zero_block_collapsed_with_gas_win`] assembles a tiny **jumpless** runtime by hand: a
//!    redundant pure block (`x + 0 + 0 + 0`) the SMT engine proves equivalent to the identity and
//!    collapses to one `PUSH0`. Deployed baseline-vs-optimized on revm, the call returns the same
//!    word and costs exactly the block's gas delta less — a deterministic win that needs no compiler
//!    (robust against compiler-version gas drift) and sidesteps the symbolic-only pipeline gate.
//!
//! 2. [`solidity_wrapping_chain_superoptimized`] / [`vyper_idempotent_and_superoptimized`] compile
//!    **real** Solidity / Vyper, then show superopt firing on the **already-optimized** compiler
//!    output: solc 0.8.24 leaves a wrapping `((a+b)-b)^a` chain (`DUP2 DUP2 ADD SUB DUP2 XOR ADD
//!    SWAP1`) that Z3 proves collapses to `POP SWAP1`; venom 0.4.3 leaves an idempotent `(a&b)&(a&b)`
//!    self-`AND` that Z3 proves is just `a&b`. The block-level gas drop is asserted directly (24→5,
//!    12→6), and revm confirms the contract returns the same result. NB: the *transaction* total is
//!    masked by the EIP-7623 calldata floor for these single-shot wins — the optimization is real and
//!    the executed block is provably cheaper, but the tx-level gas is clamped (see
//!    `gasripper-eip7623-gas-floor-e2e`), so these assert the block-gas drop + preserved behavior
//!    rather than a tx-gas drop.

use super::optimize;
use crate::core::asm::{parse_str, replacement_instr};
use crate::core::bytecode::{assemble, bytes_to_hex};
use crate::core::opcodes::gas;
use crate::core::Category;
use crate::features::e2e_harness::{deploy_and_call, encode_call, measure, owner_addr, write_temp};
use crate::sidecar::{Backend, Lang};
use revm::primitives::U256;

/// Wrap `runtime` in a minimal constructor that returns it as the deployed code.
fn creation_hex(runtime: &[u8]) -> String {
    let len = runtime.len();
    assert!(len < 256, "test runtime must fit a single-byte length");
    // PUSH1 len, DUP1, PUSH1 off, PUSH1 0, CODECOPY, PUSH1 0, RETURN  (11 bytes => off = 11).
    let off = 11u8;
    let mut code = vec![
        0x60, len as u8, 0x80, 0x60, off, 0x60, 0x00, 0x39, 0x60, 0x00, 0xf3,
    ];
    code.extend_from_slice(runtime);
    bytes_to_hex(&code)
}

#[test]
fn add_zero_block_collapsed_with_gas_win() {
    // Runtime: x = calldata[0:32]; compute x+0+0+0 (redundant); store; return 32 bytes.
    // The pure run between CALLDATALOAD and MSTORE is `PUSH1 0 ADD` x3 plus MSTORE's own
    // `PUSH1 0` offset push, leaving [x, 0]; the engine proves `PUSH0` is the cheapest equivalent.
    // The RETURN offset is already `PUSH0` (optimal) so the only superopt target is the +0 block;
    // the MSTORE offset `PUSH1 0` is absorbed into that block's `PUSH0` result.
    let asm = "PUSH1 0 CALLDATALOAD PUSH1 0 ADD PUSH1 0 ADD PUSH1 0 ADD PUSH1 0 MSTORE PUSH1 32 PUSH0 RETURN";
    let base_instrs = parse_str(asm);
    let (opt_instrs, spans) = optimize(&base_instrs);
    assert_eq!(
        spans.len(),
        1,
        "the redundant +0 block was not superoptimized"
    );

    let base_code = assemble(&base_instrs).expect("assemble baseline runtime");
    let opt_code = assemble(&opt_instrs).expect("assemble optimized runtime");
    assert!(
        opt_code.len() < base_code.len(),
        "the optimized runtime must be shorter: {} -> {}",
        base_code.len(),
        opt_code.len()
    );

    let caller = owner_addr();
    // EMPTY calldata on purpose: with no calldata tokens the EIP-7623 floor is just the 21000 base,
    // so `tx_gas_used = 21000 + execution` and the block's execution delta is visible directly (a
    // single-shot saving under a non-empty-calldata floor would be masked — see the recompute e2e).
    // CALLDATALOAD(0) then reads 0, so both runtimes return the word 0.
    let calldata: Vec<u8> = Vec::new();

    let (gas_base, out_base) = deploy_and_call(&creation_hex(&base_code), caller, calldata.clone());
    let (gas_opt, out_opt) = deploy_and_call(&creation_hex(&opt_code), caller, calldata);

    assert_eq!(
        U256::from_be_slice(&out_base),
        U256::ZERO,
        "with empty calldata the runtime must return 0"
    );
    assert_eq!(
        out_base, out_opt,
        "the superoptimized runtime must return the same word as the baseline"
    );
    // Baseline executes PUSH1 0/ADD x3 (= 21 gas) where the optimized runtime executes one PUSH0
    // (= 2 gas); every other opcode is identical, so the execution gas differs by exactly 19.
    assert_eq!(
        gas_base - gas_opt,
        19,
        "expected a 19-gas execution win, got {} -> {}",
        gas_base,
        gas_opt
    );
    tracing::info!("superopt e2e: call gas {gas_base} -> {gas_opt} (saved 19)");
}

/// Compile `path`, run superopt over the real compiler output, and sum the static gas of every
/// block it rewrites before vs. after. Returns `(spans, gas_before, gas_after, first_block_shown)`,
/// or `Err` when the toolchain is unavailable (the caller SKIPs).
fn block_gas_drop(backend: &Backend, path: &str) -> Result<(usize, u32, u32, String), String> {
    let dump = backend.dump(path, None)?;
    let (_out, spans) = optimize(&dump.instrs);
    let mut before = 0u32;
    let mut after = 0u32;
    let mut shown = String::new();
    for sp in &spans {
        let orig = &dump.instrs[sp.start..=sp.end];
        for ins in orig {
            before += gas(ins.mnem()).unwrap_or(0);
        }
        let repl: Vec<String> = sp
            .replacement
            .iter()
            .map(|t| replacement_instr(t).mnem().to_string())
            .collect();
        for m in &repl {
            after += gas(m).unwrap_or(0);
        }
        if shown.is_empty() {
            let from: Vec<&str> = orig.iter().map(|i| i.mnem()).collect();
            shown = format!("{from:?} => {repl:?}");
        }
    }
    Ok((spans.len(), before, after, shown))
}

#[test]
fn solidity_wrapping_chain_superoptimized() {
    // solc 0.8.24 --optimize cannot prove the unchecked wrapping identity ((a+b)-b)^a == 0, so it
    // leaves the 8-op block `DUP2 DUP2 ADD SUB DUP2 XOR ADD SWAP1`; Z3 proves it equals `POP SWAP1`.
    let src = r#"// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;
contract C {
    function f(uint256 a, uint256 b) external pure returns (uint256 r) {
        unchecked {
            uint256 s = a + b;
            uint256 t = s - b;
            uint256 u = t * 1;
            uint256 v = u + 0;
            r = v ^ a;
            r = r + a;
        }
    }
}
"#;
    let path = write_temp("s_sopt_real.sol", src);
    let backend = Backend::new(Lang::Solidity);
    let (spans, before, after, shown) = match block_gas_drop(&backend, &path) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("SKIP solidity superopt e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert!(spans >= 1, "superopt found no block to rewrite in real solc output");
    assert!(
        after < before,
        "the rewritten block(s) must cost less gas: {before} -> {after}"
    );
    // Behavior on a real EVM is unchanged (the chain reduces to `a`).
    let r = measure(
        &backend,
        &path,
        Category::Superopt,
        encode_call("f(uint256,uint256)", &[7, 3]),
    )
    .expect("solidity superopt measure");
    assert_eq!(
        U256::from_be_slice(&r.out_base),
        U256::from(7u64),
        "the wrapping chain must still return its first argument"
    );
    assert_eq!(
        r.out_base, r.out_opt,
        "the superoptimized contract must return the same result"
    );
    tracing::info!("solidity superopt: {shown}; block gas {before} -> {after} (-{})", before - after);
}

#[test]
fn vyper_idempotent_and_superoptimized() {
    // venom 0.4.3 leaves the idempotent self-AND of `(a & b) & (a & b)` as `AND DUP1 AND`; Z3 proves
    // `x & x == x` and drops the `DUP1 AND`.
    let src = r#"
@external
@view
def f(a: uint256, b: uint256) -> uint256:
    return (a & b) & (a & b)
"#;
    let path = write_temp("s_sopt_real.vy", src);
    let backend = Backend::new(Lang::Vyper);
    let (spans, before, after, shown) = match block_gas_drop(&backend, &path) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("SKIP vyper superopt e2e (toolchain unavailable): {e}");
            return;
        }
    };
    assert!(spans >= 1, "superopt found no block to rewrite in real venom output");
    assert!(
        after < before,
        "the rewritten block(s) must cost less gas: {before} -> {after}"
    );
    // Behavior on a real EVM is unchanged (`(a&b)&(a&b)` == `a&b`).
    let r = measure(
        &backend,
        &path,
        Category::Superopt,
        encode_call("f(uint256,uint256)", &[7, 3]),
    )
    .expect("vyper superopt measure");
    assert_eq!(
        U256::from_be_slice(&r.out_base),
        U256::from(7u64 & 3u64),
        "the self-AND must still return a & b"
    );
    assert_eq!(
        r.out_base, r.out_opt,
        "the superoptimized contract must return the same result"
    );
    tracing::info!("vyper superopt: {shown}; block gas {before} -> {after} (-{})", before - after);
}
