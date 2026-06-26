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
# Vyper: a Python with `vyper` importable (tested on 0.4.3)
export GASRIPPER_VYPER_PYTHON=/path/to/python-with-vyper
# Solidity: any stdlib Python plus the solc binary
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

## Adding a feature

A feature is one independent gas-reduction pass. Two ship today: `guards` (trusted-caller revert
removal, `src/features/guards/`) and `shuffle` (always-safe stack rescheduling,
`src/features/shuffle/`) — each a reference module of `mod.rs` + `README.md` + `e2e.rs`. `shuffle`
shows a pass need not be guard-removal: it owns `Category::Shuffle`, runs its own engine
(`core::stack::minimize_shuffle`) instead of `strip_guards`, and the orchestrator
(`features::optimize`) runs both passes and merges their edit spans. To add another pass, create a
module, reuse the shared engine / sidecar / e2e harness (DRY — add new shared helpers to those,
don't inline), register its `META` in `features::registry()`, and run it from `features::optimize`.
Keep the binary pure-`std` and warning-free.
