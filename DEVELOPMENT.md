# Development

Building, testing, and extending gasripper. User-facing docs: [README.md](README.md).

## Build

```bash
cargo build              # debug -> target/debug/gasripper
cargo build --release    # release -> target/release/gasripper
```

The shipped binary's runtime dependencies are `clap` (argument parsing and `--help`/`--version`
generation) and `tracing` / `tracing-subscriber` (diagnostic logging to stderr, level via
`RUST_LOG`); the config parser and core stay on `std` and build offline. `revm`, declared in
`[dev-dependencies]`, is used by the e2e tests and is never linked into the binary.

## Tests

```bash
cargo test                       # everything (e2e auto-skips without toolchains)
cargo test guards                # one module by name substring
cargo test range_assert_removed  # a single test by name
```

Two layers:

- **Unit tests** (`src/features/*`, `src/core/*`) pin down, with hand-written assembly, exactly
  which pattern each feature strips and which it preserves (auth / side effects / inputs-consuming
  checks). They need no compilers and always run.
- **End-to-end tests** (`src/features/<feature>/e2e.rs`) prove the optimization on a **real EVM**
  (`revm`): compile a contract, run the feature's pass, re-assemble creation bytecode, deploy the
  baseline and optimized bytecode, call a function on each, and assert the result is unchanged while
  gas drops. `guards/e2e.rs` covers the guard strips; `shuffle/e2e.rs` covers stack rescheduling.
  They go through the shared harness [`src/features/e2e_harness.rs`](src/features/e2e_harness.rs)
  (`measure`, `deploy_then_call`, `deploy_and_call`, `assert_win`, `assert_preserved_and_smaller`,
  `assert_rejects_stranger`, `encode_call`, `write_temp`) — reused by every feature and both
  languages.

Two e2e layers per scenario:

- **auth + gas win** — strip on a trusted-caller contract; assert behavior preserved *and* gas
  drops, then `assert_rejects_stranger` confirms the strip **kept the auth guard** (a non-owner
  caller still reverts on the optimized bytecode).
- **no-auth** (`assert_preserved_and_smaller`) — strip the same guard with no auth wrapper; proves
  the auth check is irrelevant to what is removed.

Note that **stripping always shrinks the creation bytecode, but call gas drops only when the
stripped guard is on the call's hot path** — e.g. the calldata-size guard becomes hot once the
contract has a real selector dispatcher (two+ functions, as the `owner()` getter adds). So the
auth-wrapped tests show a gas win; the single-function no-auth tests show a bytecode win.

`assert_win`/`assert_preserved_and_smaller` take the **exact** call gas before and after the strip
(`gas_base`, `gas_opt`) and pin them with `assert_eq!`, so the numbers in the table below are not
just documented — any drift of a single gas unit (a compiler-version bump, an engine change) fails
the test. Update the pins in each `e2e.rs` and this table together.

### Running the full e2e (with toolchains)

The e2e tests **skip cleanly** when their toolchain is absent. To run them for real, point the tool
at the compilers via the environment:

```bash
# Vyper: a Python with `vyper` importable (tested on 0.4.3) — the venom assembler
# is a Python library function with no CLI, so this backend uses a sidecar script
export GASRIPPER_VYPER_PYTHON=/path/to/python-with-vyper
# Solidity: just the solc binary (the asm-json round-trip is native Rust, no Python)
export GASRIPPER_SOLC=/path/to/solc

cargo test --bin gasripper -- --nocapture     # prints per-case gas/bytes saved
```

Measured wins (real EVM), stripping the guards, result unchanged. The full set is the 18 cases in
`src/features/guards/e2e.rs` (both languages × `+ - * /` and range/cast × auth / no-auth);
representative auth rows (Vyper 0.4.3, solc 0.8.24):

| Case | Vyper | Solidity |
|---|---|---|
| `a + b`           | 23631 → 23593 gas | 23843 → 23793 gas |
| `a * b`           | 23671 → 23633 gas | 23859 → 23809 gas |
| range/cast guard  | 23479 → 23419 gas (`convert`) | 23617 → 23545 gas (`require`) |

The `shuffle` pass is proven the same way (`src/features/shuffle/e2e.rs`): a Vyper 0.4.3 loop
(`for i in range(n): s += i*i`) reschedules a window in the loop body, dropping call gas 22049 →
22031 (saved 18 over 5 iterations) with the result unchanged and the creation bytecode 169 → 167.

The `involution` pass is proven the same way (`src/features/involution/e2e.rs`): a Vyper 0.4.3 loop
(`for i in range(n): s += ~(~i)`) cancels the `NOT NOT` venom leaves in the loop body, dropping call
gas 21784 → 21754 (saved 30 over 5 iterations) with the result unchanged and the creation bytecode
153 → 151. A single-call `~(~x)` shrinks the bytecode too but shows no transaction-gas drop — its
body runs below the EIP-7623 calldata floor — which is why the e2e (like `shuffle`'s) uses a loop.

The `recompute` pass is proven the same way (`src/features/recompute/e2e.rs`) on both languages, with
the creation bytecode the **same size** (a single-byte opcode swapped for another): solc 0.8.24 — the
non-payable `CALLVALUE DUP1` guard that runs once per call is rewritten to a second `CALLVALUE`,
dropping call gas 22103 → 22102; Vyper 0.4.3 venom — a per-iteration `CHAINID DUP1` in a loop body
(`s += chain.id * chain.id`) is rewritten to a second `CHAINID`, dropping call gas 22099 → 22094 (−5
over 5 iterations). Unlike the others it is length-preserving, so it also lowers gas on raw concrete
`.hex`/`.bin` bytecode (`--emit-bytecode`), where no compiler relinks.

The `foldshift` pass is proven on Solidity (`src/features/fold_shift/e2e.rs`): solc 0.8.24 materializes
the address-cleaning mask `1 << 160` as `PUSH1 0x01 PUSH1 0xa0 SHL` to keep bytecode small; folding it
to a single `PUSH21` literal drops a `transfer(address,uint256)` call (two masked address arguments)
26518 → 26506 gas (−12) while **growing** the creation bytecode 473 → 532 (+59) — the one pass that
trades size for gas. It is solc-specific: Vyper's venom does not emit the idiom.

The `cmpnorm` pass is proven on Vyper (`src/features/cmpnorm/e2e.rs`): venom 0.4.3 compares two
freshly-computed subexpressions with `SWAP1 LT`; folding the per-iteration `(x * i) < (y * i)`
comparison in a loop body to a single `GT` drops call gas 22783 → 22768 (−15 = 3 gas over 5 iterations)
while shrinking the creation bytecode 203 → 202. It is Vyper-specific: solc selects operand order via
`DUP` depth and never emits the idiom.

**Cross-pass progressive proofs** (`src/features/progressive_e2e.rs`) compile one real contract and
measure call gas on revm with a growing set of enabled passes, asserting the result is unchanged while
gas only falls: Vyper guards 22794 → +shuffle 22731 → +involution 22701 → +recompute 22696; Solidity
recompute 44801 → +guards 44562 → +foldshift 44550. They exercise the pass-merge precedence
(inline > guards > shuffle > involution > foldshift > cmpnorm > recompute; an overlapping later span is
dropped) and lock the guards+foldshift regression: foldshift may fold the `PUSH sel PUSH 0xe0 SHL`
Panic-selector inside an inverse guard's inline revert block that guards' DCE deletes — the solc
sidecar's `_apply_edits` must drop that index rather than strand the folded push (else `InvalidJump`).

## Adding a feature

A feature is one independent gas-reduction pass. Seven ship today: `guards` (trusted-caller revert
removal, `src/features/guards/`), `shuffle` (always-safe stack rescheduling, `src/features/shuffle/`),
`involution` (always-safe `NOT NOT` cancelling, `src/features/involution/`), `recompute`
(always-safe `OP DUP1` → `OP OP` recompute, `src/features/recompute/`), `foldshift` (always-safe
constant `PUSH a PUSH b SHL/SHR` folding, `src/features/fold_shift/`), `cmpnorm` (always-safe
`SWAP1 LT` → `GT` comparison normalization, `src/features/cmpnorm/`), and `inline` (always-safe
relocation of a small Vyper `@internal` function with 2+ call sites into its call sites — analysis in
`core::inline`, orchestration in `src/features/inline/` — the first feature with a numeric parameter,
`--inline-max-body`) — each a reference module of `mod.rs` + `README.md` + `e2e.rs`. `shuffle`,
`involution`, and `recompute` show a pass need not be guard-removal: each owns its own `Category` and
runs its own `scan` instead of `strip_guards`, and the orchestrator (`features::optimize_with`) runs
every enabled pass and merges their edit spans (a later pass yields to an earlier one on an overlap,
via `merge_nonoverlapping`). To add another pass, create
a module, reuse the shared engine / sidecar / e2e harness (DRY — add new shared helpers to those,
don't inline), register its `META` in `features::registry()`, and run it from `features::optimize`.
Keep the binary pure-`std` and warning-free.
