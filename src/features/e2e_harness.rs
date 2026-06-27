//! Shared end-to-end harness for feature gas/behavior proofs (test-only).
//!
//! Every feature proves itself the same way: compile a real contract, strip one
//! category, re-assemble creation bytecode, then on a real EVM (revm — a test-only
//! dev-dependency, never linked into the binary) deploy the baseline and optimized
//! bytecode, call a function on each, and check the result is unchanged while gas
//! drops. That whole flow lives here once and is reused across features and across
//! languages (Vyper / Solidity), so a feature's `e2e.rs` only supplies a contract,
//! a category, and a call.
//!
//! [`measure`] returns `Err` (so the test can SKIP) when the language toolchain is
//! unavailable, and panics only on a genuine EVM failure (a broken optimization).

use std::collections::HashSet;
use std::io::Write;

use crate::core::Category;
use crate::features::optimize;
use crate::sidecar::Backend;

use revm::context::result::{ExecutionResult, Output};
use revm::context::TxEnv;
use revm::database::InMemoryDB;
use revm::primitives::{keccak256, Address, Bytes, TxKind, U256};
use revm::{Context, ExecuteCommitEvm, MainBuilder, MainContext};
use tracing_subscriber::EnvFilter;

/// Install a test-scoped log subscriber (idempotent across parallel tests) so the
/// gas/SKIP diagnostics are captured by libtest and shown on failure or `--nocapture`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .try_init();
}

/// Outcome of an end-to-end feature measurement.
pub struct Report {
    /// Number of guards stripped of the requested category.
    pub stripped: usize,
    pub gas_base: u64,
    pub gas_opt: u64,
    pub out_base: Vec<u8>,
    pub out_opt: Vec<u8>,
    pub bytes_before: usize,
    pub bytes_after: usize,
    /// Optimized creation bytecode (for follow-up checks, e.g. auth still rejects a stranger).
    pub creation_opt: String,
}

/// The address that deploys (and thus owns) the contract under test.
pub fn owner_addr() -> Address {
    Address::from([0x11u8; 20])
}

impl Report {
    pub fn gas_saved(&self) -> i64 {
        self.gas_base as i64 - self.gas_opt as i64
    }
    pub fn bytes_saved(&self) -> i64 {
        self.bytes_before as i64 - self.bytes_after as i64
    }
}

/// Standard assertions for a "strip win" + a one-line gas/bytes log. Shared by
/// every feature's e2e so the proof is asserted identically everywhere: at least
/// one guard stripped, the call returns `expected` before and after, and the
/// optimized call uses strictly less gas. `gas_base`/`gas_opt` pin the exact
/// measured call gas before and after the strip, so any drift in a single gas unit
/// fails the test (the per-feature READMEs document these same numbers).
pub fn assert_win(r: &Report, lang: &str, expected: u64, gas_base: u64, gas_opt: u64) {
    assert!(r.stripped >= 1, "{lang}: expected at least one check to strip");
    assert_eq!(U256::from_be_slice(&r.out_base), U256::from(expected), "{lang}: wrong result");
    assert_eq!(r.out_base, r.out_opt, "{lang}: optimized output must match baseline");
    assert!(r.gas_opt < r.gas_base, "{lang}: strip should reduce call gas: {} -> {}", r.gas_base, r.gas_opt);
    assert_eq!(r.gas_base, gas_base, "{lang}: baseline call gas drifted from pinned {gas_base} to {}", r.gas_base);
    assert_eq!(r.gas_opt, gas_opt, "{lang}: optimized call gas drifted from pinned {gas_opt} to {}", r.gas_opt);
    tracing::info!(
        "{lang}: call gas {} -> {} (saved {}), creation {} -> {} bytes (saved {})",
        r.gas_base, r.gas_opt, r.gas_saved(), r.bytes_before, r.bytes_after, r.bytes_saved(),
    );
}

/// Assertions for a feature applied to a contract WITHOUT a trusted-caller (auth)
/// wrapper: the guard is still stripped and behavior preserved, and the creation
/// bytecode shrinks. Call gas drops only when the stripped guard sits on the call's
/// hot path (which depends on the dispatcher shape); the bytecode always shrinks.
/// This proves the auth check is irrelevant to what the feature removes.
/// `gas_base`/`gas_opt` pin the exact measured call gas before and after the strip
/// (equal here, since a single-function contract has no hot selector dispatcher),
/// so any drift in a single gas unit fails the test.
pub fn assert_preserved_and_smaller(r: &Report, lang: &str, expected: u64, gas_base: u64, gas_opt: u64) {
    assert!(r.stripped >= 1, "{lang}: expected a guard to strip without an auth wrapper");
    assert_eq!(U256::from_be_slice(&r.out_base), U256::from(expected), "{lang}: wrong result");
    assert_eq!(r.out_base, r.out_opt, "{lang}: optimized output must match baseline");
    assert!(r.bytes_after < r.bytes_before, "{lang}: creation bytecode must shrink: {} -> {}", r.bytes_before, r.bytes_after);
    assert!(r.gas_opt <= r.gas_base, "{lang}: gas must not increase: {} -> {}", r.gas_base, r.gas_opt);
    assert_eq!(r.gas_base, gas_base, "{lang}: baseline call gas drifted from pinned {gas_base} to {}", r.gas_base);
    assert_eq!(r.gas_opt, gas_opt, "{lang}: optimized call gas drifted from pinned {gas_opt} to {}", r.gas_opt);
    tracing::info!(
        "{lang} (no auth): stripped {}, call gas {} -> {}, creation {} -> {} bytes (saved {})",
        r.stripped, r.gas_base, r.gas_opt, r.bytes_before, r.bytes_after, r.bytes_saved(),
    );
}

/// Assertions for a LENGTH-PRESERVING feature (`recompute`): the rewrite swaps one
/// single-byte opcode for another, so the creation bytecode is **exactly the same size**
/// while the call uses strictly less gas. At least one rewrite applied and behavior
/// preserved. `gas_base`/`gas_opt` pin the exact measured call gas before and after, so
/// any single-gas-unit drift fails the test.
pub fn assert_same_size_cheaper(r: &Report, lang: &str, expected: u64, gas_base: u64, gas_opt: u64) {
    assert!(r.stripped >= 1, "{lang}: expected at least one recompute rewrite");
    assert_eq!(U256::from_be_slice(&r.out_base), U256::from(expected), "{lang}: wrong result");
    assert_eq!(r.out_base, r.out_opt, "{lang}: optimized output must match baseline");
    assert_eq!(
        r.bytes_after, r.bytes_before,
        "{lang}: recompute must preserve bytecode size: {} -> {}", r.bytes_before, r.bytes_after
    );
    assert!(r.gas_opt < r.gas_base, "{lang}: recompute should reduce call gas: {} -> {}", r.gas_base, r.gas_opt);
    assert_eq!(r.gas_base, gas_base, "{lang}: baseline call gas drifted from pinned {gas_base} to {}", r.gas_base);
    assert_eq!(r.gas_opt, gas_opt, "{lang}: optimized call gas drifted from pinned {gas_opt} to {}", r.gas_opt);
    tracing::info!(
        "{lang} (recompute): rewrote {}, call gas {} -> {} (saved {}), creation {} bytes (unchanged)",
        r.stripped, r.gas_base, r.gas_opt, r.gas_saved(), r.bytes_after,
    );
}

/// Assertions for a SIZE-FOR-GAS feature (`foldshift`): precomputing a constant grows the
/// creation bytecode (a literal push is wider than the `PUSH a PUSH b SHL` idiom) while the
/// call uses strictly less gas. At least one fold applied and behavior preserved.
/// `gas_base`/`gas_opt` pin the exact measured call gas before and after, so any single-gas
/// drift fails the test.
pub fn assert_cheaper_larger(r: &Report, lang: &str, expected: u64, gas_base: u64, gas_opt: u64) {
    assert!(r.stripped >= 1, "{lang}: expected at least one constant shift to fold");
    assert_eq!(U256::from_be_slice(&r.out_base), U256::from(expected), "{lang}: wrong result");
    assert_eq!(r.out_base, r.out_opt, "{lang}: optimized output must match baseline");
    assert!(
        r.bytes_after > r.bytes_before,
        "{lang}: foldshift trades size for gas — bytecode must grow: {} -> {}", r.bytes_before, r.bytes_after
    );
    assert!(r.gas_opt < r.gas_base, "{lang}: foldshift should reduce call gas: {} -> {}", r.gas_base, r.gas_opt);
    assert_eq!(r.gas_base, gas_base, "{lang}: baseline call gas drifted from pinned {gas_base} to {}", r.gas_base);
    assert_eq!(r.gas_opt, gas_opt, "{lang}: optimized call gas drifted from pinned {gas_opt} to {}", r.gas_opt);
    tracing::info!(
        "{lang} (foldshift): folded {}, call gas {} -> {} (saved {}), creation {} -> {} bytes (+{})",
        r.stripped, r.gas_base, r.gas_opt, r.gas_saved(), r.bytes_before, r.bytes_after, -r.bytes_saved(),
    );
}

/// Assertions for a LENGTH-REDUCING always-safe feature (e.g. `cmpnorm`): folding a window
/// to fewer instructions shrinks the creation bytecode while the call uses strictly less gas.
/// At least one fold applied and behavior preserved. `gas_base`/`gas_opt` pin the exact
/// measured call gas before and after, so any single-gas drift fails the test.
pub fn assert_smaller_cheaper(r: &Report, lang: &str, expected: u64, gas_base: u64, gas_opt: u64) {
    assert!(r.stripped >= 1, "{lang}: expected at least one window to fold");
    assert_eq!(U256::from_be_slice(&r.out_base), U256::from(expected), "{lang}: wrong result");
    assert_eq!(r.out_base, r.out_opt, "{lang}: optimized output must match baseline");
    assert!(
        r.bytes_after < r.bytes_before,
        "{lang}: folding a window must shrink the bytecode: {} -> {}", r.bytes_before, r.bytes_after
    );
    assert!(r.gas_opt < r.gas_base, "{lang}: the fold should reduce call gas: {} -> {}", r.gas_base, r.gas_opt);
    assert_eq!(r.gas_base, gas_base, "{lang}: baseline call gas drifted from pinned {gas_base} to {}", r.gas_base);
    assert_eq!(r.gas_opt, gas_opt, "{lang}: optimized call gas drifted from pinned {gas_opt} to {}", r.gas_opt);
    tracing::info!(
        "{lang} (cmpnorm): folded {}, call gas {} -> {} (saved {}), creation {} -> {} bytes (saved {})",
        r.stripped, r.gas_base, r.gas_opt, r.gas_saved(), r.bytes_before, r.bytes_after, r.bytes_saved(),
    );
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// Write `src` to a uniquely-named temp file and return its path string.
pub fn write_temp(filename: &str, src: &str) -> String {
    let path = std::env::temp_dir().join(filename);
    let mut f = std::fs::File::create(&path).expect("create temp contract");
    f.write_all(src.as_bytes()).expect("write temp contract");
    path.to_str().expect("temp path utf-8").to_string()
}

/// Build calldata for a function: 4-byte selector of `signature` + 32-byte args.
pub fn encode_call(signature: &str, args: &[u64]) -> Vec<u8> {
    let sel = keccak256(signature.as_bytes());
    let mut data = sel[..4].to_vec();
    for &a in args {
        data.extend_from_slice(&U256::from(a).to_be_bytes::<32>());
    }
    data
}

/// Deploy `creation` from `deployer`, then call it from `caller` with `calldata`,
/// returning the raw execution result (may be a revert — for auth checks).
pub fn deploy_then_call(creation: &str, deployer: Address, caller: Address, calldata: Vec<u8>) -> ExecutionResult {
    let mut evm = Context::mainnet().with_db(InMemoryDB::default()).build_mainnet();

    let deploy = TxEnv::builder()
        .caller(deployer)
        .kind(TxKind::Create)
        .data(Bytes::from(hex_to_bytes(creation)))
        .gas_limit(10_000_000)
        .gas_price(0)
        .nonce(0)
        .build()
        .unwrap();
    let addr = match evm.transact_commit(deploy).unwrap() {
        ExecutionResult::Success { output: Output::Create(_, Some(a)), .. } => a,
        other => panic!("deploy failed: {other:?}"),
    };

    let call = TxEnv::builder()
        .caller(caller)
        .kind(TxKind::Call(addr))
        .data(Bytes::from(calldata))
        .gas_limit(10_000_000)
        .gas_price(0)
        .nonce(if caller == deployer { 1 } else { 0 })
        .build()
        .unwrap();
    evm.transact_commit(call).unwrap()
}

/// Deploy `creation` from `caller`, call it with `calldata`, return (gas_used, output).
pub fn deploy_and_call(creation: &str, caller: Address, calldata: Vec<u8>) -> (u64, Vec<u8>) {
    match deploy_then_call(creation, caller, caller, calldata) {
        ExecutionResult::Success { gas, output: Output::Call(out), .. } => (gas.tx_gas_used(), out.to_vec()),
        other => panic!("call failed: {other:?}"),
    }
}

/// Assert that the (optimized) contract still **rejects a non-owner caller** — i.e.
/// the auth guard was preserved by the strip. Deploys from the owner, then calls
/// from a different address and expects a revert.
pub fn assert_rejects_stranger(creation: &str, calldata: Vec<u8>) {
    let stranger = Address::from([0x22u8; 20]);
    let res = deploy_then_call(creation, owner_addr(), stranger, calldata);
    assert!(
        !res.is_success(),
        "auth guard must still reject a non-owner caller after strip; got {res:?}"
    );
}

/// Compile `source_path`, strip only `only`, re-assemble baseline and optimized
/// creation bytecode, and run both on revm with `calldata`.
///
/// Returns `Err` when the toolchain is unavailable (the test should SKIP) or when
/// the baseline invariant fails; panics on an actual EVM execution failure.
pub fn measure(
    backend: &Backend,
    source_path: &str,
    only: Category,
    calldata: Vec<u8>,
) -> Result<Report, String> {
    init_tracing();
    // 1. Compile + read runtime instructions (Err -> caller skips).
    let dump = backend.dump(source_path, None)?;

    // 2. Run only the requested category's pass.
    let set: HashSet<Category> = [only].into_iter().collect();
    let (_optimized, spans) = optimize(&dump.instrs, &set);

    // 3. Re-assemble baseline and optimized creation bytecode.
    let base = backend.build(source_path, &[], None)?;
    let opt = backend.build(source_path, &spans, None)?;
    if base.creation_hex != base.reference_hex {
        return Err("baseline build does not match the compiler reference bytecode".into());
    }

    // 4. Run both on a real EVM (the owner deploys/calls each, isolated state).
    let caller = owner_addr();
    let (gas_base, out_base) = deploy_and_call(&base.creation_hex, caller, calldata.clone());
    let (gas_opt, out_opt) = deploy_and_call(&opt.creation_hex, caller, calldata);

    Ok(Report {
        stripped: spans.len(),
        gas_base,
        gas_opt,
        out_base,
        out_opt,
        bytes_before: base.bytes_before,
        bytes_after: opt.bytes_after,
        creation_opt: opt.creation_hex,
    })
}
