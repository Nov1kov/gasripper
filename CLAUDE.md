# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Language

You may reply in whatever language is convenient, but all code and every file in the project
(source, comments, identifiers, CLI help text, strings, README, Cargo.toml, docs) MUST be written
strictly in English.

## Commands

```bash
cargo build                 # debug build
cargo build --release       # release binary -> target/release/gasripper
cargo test                  # run all unit tests (no external compilers needed)
cargo test strip_math       # run tests in one module by name substring
cargo test overflow_check_removed   # run a single test by name
```

Run the CLI: `./target/debug/gasripper <input>` (or `cargo run -- <input>`). With only an
input path, all features are on and it prints a strip report. Key flags: `--emit-asm`,
`--emit-bytecode`, `--disable math,abi`, `--config <file>`, `--input-kind`, `--list-features`.

No external crates — pure `std`, so builds work offline. Edition 2024 (needs a recent toolchain).
Full testing/e2e setup (toolchain env vars, the real-EVM harness, adding a feature): see
[DEVELOPMENT.md](DEVELOPMENT.md).

## What this tool does

gasripper is an aggressive EVM gas optimizer: it removes provably-safe revert-guards (overflow,
ABI/calldata bounds, range/cast asserts) from contract code without changing live execution.
It is a Rust port of the Python `evm_asm_optimizer/` (kept in-repo for reference); the stack
identity criterion and strip algorithm are ported 1:1.

## Architecture

Pipeline: **input frontend → instructions → strip engine (category-gated) → report / emit**.

- `src/core/` — the core module every feature depends on:
  - `asm.rs` defines `Instr { kind: Kind, tokens: Vec<String> }`, the single representation for
    both concrete ops and symbolic Vyper-venom tokens (`_sym_*`, `_OFST`, `_mem_`). `parse_str`
    is the port of Python `to_instr`. `tokens[0]` is always the mnemonic.
  - `stack.rs::strip_residue` is the safety criterion (generalizes `simulate_identity`, kept for
    reference/tests): simulate a run over slot-ids; a guard is removable iff its fall-through stack
    consists ONLY of input slots (no created value survives into live code). Returns the minimal
    `POP`/`SWAP` shuffle reproducing that residue — `[]` for a pure identity (delete), a few ops for
    a consuming check (e.g. an overflow assertion: keep `ADD`, replace `SWAP1 DUP2 LT revert JUMPI`
    with `SWAP1 POP`). DUP/SWAP modeled exactly; other ops via `(pops, pushes)` from `opcodes.rs`.
  - `strip.rs::strip_guards(instrs, &enabled_categories)` is the engine. It scans for
    `<cond> _sym_*revert* JUMPI`, grows the LONGEST barrier-free suffix that `strip_residue` accepts,
    and rewrites it (delete or shuffle) **only if its `Category` is in the enabled set**. A `Span`
    now carries its `replacement` ops. It always preserves auth (`CALLER`/`ORIGIN`), side-effects
    (`is_side`), and non-terminal `JUMP(I)`; residue strips that DROP a value also require their
    straight-line block (`block_clean_for_residue`) to be free of auth/side-effects, so a
    `msg.sender` check or a call's success flag is never dropped. Because the removed run keeps the
    arithmetic and cuts only the *assertion*, an overflow check classifies as `assert`, not `math`.
  - `bytecode.rs` disassembles raw bytecode and assembles **concrete** programs only.

- `src/features/` — each feature is one `Category` of guard to strip, lives in its own module,
  exposes `META: FeatureMeta` + a thin `strip()`, and owns the tests that pin down exactly what it
  removes vs. preserves. The CLI does **not** run features one-by-one; it collects enabled
  categories from `config` and calls `strip_guards` once. To add a feature: add a `Category`
  variant in `strip.rs`, a module here, and register it in `features::registry()`.

- `src/config.rs` — `FeatureConfig`, precedence defaults → config file → CLI (`--enable`/`--disable`).
  `enabled_categories()` is the bridge to the strip engine.

- `src/input/` — frontends produce a `Loaded { instrs, symbolic, kind }`. `raw_asm` and `bytecode`
  are fully supported; `vyper`/`solidity` shell out to the compiler (presence-checked) and are
  **experimental**.

## Critical constraint: symbolic vs. concrete

The strip engine detects guards **by symbolic revert labels** (`_sym_*revert*`). So real stripping
works on symbolic assembly (raw `.asm`, Vyper-venom output) — not on resolved raw bytecode/Solidity
(no symbolic labels → nothing detected yet).

`Loaded.symbolic` gates emission: symbolic programs can only `--emit-asm` (label relinking with
PUSH-size fixpoint is deliberately **not** implemented — wrong bytecode in a gas tool is dangerous).
Concrete programs (`.hex`/`.bin`) round-trip to `--emit-bytecode`. Do not add a guessed linker; if
extending emission, treat it as a real assembler with a label-resolution fixpoint.

**Creation bytecode** (`--emit-creation`, Vyper only) sidesteps the linker entirely: `src/sidecar.rs`
drives `scripts/vyper_sidecar.py`, which compiles the source, hands the RUNTIME instructions to the
Rust strip engine, then re-assembles the full program (constructor untouched) with **Vyper's own
assembler** (`assembly_to_evm`) — not a hand-written linker. A baseline invariant (assemble with no
deletions == Vyper's reference bytecode) fails fast on compiler drift. The interpreter/script come
from `GASRIPPER_VYPER_PYTHON` / `GASRIPPER_VYPER_SIDECAR`.

**Solidity** is wired the same way (`src/sidecar.rs::Lang::Solidity`, `scripts/solc_sidecar.py`):
`solc --asm-json` ⇄ `--import-asm-json` (byte-identical round-trip), strip the runtime, solc
re-links. Both languages share ONE Rust protocol client (`Backend`) and ONE descriptor format. The
solc sidecar **normalizes both revert idioms** so the unchanged strip engine handles them: *direct*
(`<cond> PUSH[revert_tag] JUMPI`) — the `PUSH [tag]` to a pure-revert block becomes `pushsym
_sym_revert_<n>`; *inverse* (`<cond> PUSH[continue_tag] JUMPI; <inline revert>`, the `require` form)
— the `PUSH [tag]` becomes `pushsym _sym_revert_inv_<n>`, and `build` also deletes the inline revert
block when the guard's JUMPI is deleted (indices stay 1:1; detection is deterministic across
dump/build). `solc` comes from `GASRIPPER_SOLC`; interpreter/script from `GASRIPPER_SOLC_PYTHON` /
`GASRIPPER_SOLC_SIDECAR`.

The `abi` feature (`src/features/strip_abi/`) is the reference feature: a feature README + rich
module docs + unit tests + an `e2e.rs` that proves gas savings on a real EVM (`revm`, a
**dev-dependency only** — the shipped binary stays pure `std`) for BOTH Vyper and Solidity via the
shared harness `src/features/e2e_harness.rs`. Each e2e skips cleanly when its toolchain is absent.

## Safety invariants (do not break)

`is_auth`/`is_side` in `strip.rs` are the preservation sets. Stripping is only safe under a trusted
caller; the README disclaimer (first line) and these sets are load-bearing, not cosmetic.
