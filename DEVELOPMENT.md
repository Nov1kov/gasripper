# Development

Building, testing, and extending gasripper. User-facing docs: [README.md](README.md).

## Build

```bash
cargo build              # debug -> target/debug/gasripper
cargo build --release    # release -> target/release/gasripper
```

The shipped binary has **no external dependencies** (pure `std`, builds offline). The only
dependency is `revm`, declared in `[dev-dependencies]` — it is used by the e2e tests and is never
linked into the binary.

## Tests

```bash
cargo test                       # everything (e2e auto-skips without toolchains)
cargo test strip_assert          # one module by name substring
cargo test range_assert_removed  # a single test by name
```

Two layers:

- **Unit tests** (`src/features/*`, `src/core/*`) pin down, with hand-written assembly, exactly
  which pattern each feature strips and which it preserves (auth / side effects / inputs-consuming
  checks). They need no compilers and always run.
- **End-to-end tests** (`src/features/*/e2e.rs`) prove the optimization on a **real EVM** (`revm`):
  compile a contract, strip one category, re-assemble creation bytecode, deploy the baseline and
  optimized bytecode, call a function on each, and assert the result is unchanged while gas drops.
  They go through the shared harness [`src/features/e2e_harness.rs`](src/features/e2e_harness.rs)
  (`measure`, `deploy_then_call`, `deploy_and_call`, `assert_win`, `assert_preserved_and_smaller`,
  `assert_rejects_stranger`, `encode_call`, `write_temp`) — reused by every feature and both
  languages.

Three e2e layers per feature (where the toolchain strips that category):

- **auth + gas win** — strip on a trusted-caller contract; assert behavior preserved *and* gas
  drops, then `assert_rejects_stranger` confirms the strip **kept the auth guard** (a non-owner
  caller still reverts on the optimized bytecode).
- **no-auth** (`assert_preserved_and_smaller`) — strip the same guard with no auth wrapper; proves
  the auth check is irrelevant to what is removed.

Note that **stripping always shrinks the creation bytecode, but call gas drops only when the
stripped guard is on the call's hot path** — e.g. the calldata-size guard becomes hot once the
contract has a real selector dispatcher (two+ functions, as the `owner()` getter adds). So the
auth-wrapped tests show a gas win; the single-function no-auth tests show a bytecode win.

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

Measured wins (real EVM), stripping a single category, result unchanged:

| Feature | Vyper | Solidity |
|---|---|---|
| `abi`    | 23631 → 23605 gas, 191 → 181 bytes | 23842 → 23821 gas, 324 → 317 bytes |
| `assert` | 23479 → 23445 gas, 187 → 168 bytes | 23617 → 23576 gas, 283 → 264 bytes |
| `math`   | no-op (consuming overflow checks preserved) | 23842 → 23811 gas, 324 → 311 bytes |

## Adding a feature

Follow the reference feature `src/features/strip_abi/` and the project skill
`.claude/skills/add-gasripper-feature/SKILL.md`, which encodes the full procedure: add a `Category`,
extend `category()`, create the module (`mod.rs` + `README.md` + `e2e.rs`), reuse the shared
engine / sidecar / e2e harness (DRY — add new shared helpers to those, don't inline), register in
`features::registry()`, and update the explicit category lists. Keep the binary pure-`std` and
warning-free.
