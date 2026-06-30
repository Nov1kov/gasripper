# CLAUDE.md

**gasripper** is an aggressive EVM gas optimizer: it lowers a contract's gas by any provably-safe
transformation that does not change live execution. The technique implemented today is removing
redundant revert-guards (overflow, ABI/calldata bounds, range/cast asserts), but the goal is gas
reduction by any means — the architecture is meant to host other gas-saving passes too, not only
guard removal.

## Language

You may reply in whatever language is convenient, but all code and every file in the project
(source, comments, identifiers, CLI help text, strings, README, Cargo.toml, docs) MUST be written
strictly in English.

## Development rules

### CORE
- Ask questions if I did not specify the task clearly enough.
- **Do NOT present guesses about code behavior as fact.** Before stating what a flag/function/field/pipeline stage does (especially when concluding about the cause of a bug), OPEN the source and verify. If you cannot verify, or it is an assumption, explicitly mark it as "hypothesis"/"assumption" instead of writing it as a statement of fact.
- At the end, try to verify the code is free of errors via `cargo check` or `cargo test`.
- IF there is not enough data for a given spot, mark it with a TODO pointing to the place to return to later.
- If you fixed a bug, ALWAYS lock in the fix with a new test case.
- Every bug must be reproduced by a unit test before being fixed. **Order: first write a test that FAILS on the current code (reproduces the bug), then fix the code so the test passes. If the test does not fail without the fix, it does not reproduce the bug and you must pick different input data.**
- Every change must be covered by a unit test to guarantee repeatability.
- The existing code structure must not be changed without a strong reason.
- Minor inconsistencies and typos in the existing code may be fixed.
- Constantly apply the boy-scout rule. If you touch some code, leave it better than it was.
- **Keep the documentation up to date.** On major architecture changes, adding new modules/crates, or changing the data format — you MUST update the corresponding documents (`README.md`, `DEVELOPMENT.md`, and the per-feature READMEs under `src/features/`). Documentation must not diverge from the code. Check the description you add against the actual code instead of writing it from memory.
- **All `README.md` files and project documentation MUST be as concise as possible.** Before adding anything, search the document (and the other project docs) for an existing section on that topic — if it is already covered, update that spot instead of writing a new paragraph, and never repeat a fact that was already stated elsewhere.

### CODE DESIGN
- **DRY (Don't Repeat Yourself)**: if you see repeated code (3+ duplicated lines), you MUST extract it into a separate method/function. Code duplication is not acceptable. **DRY ≠ reducing arguments**: cutting the number of parameters by fixing the rest is not deduplication — it is a hidden restriction of flexibility.
- If you need to parallelize or run some block of code in threads, extract that block into a separate function.
- If you need to add some functionality, always extract it into a separate function. IF it is a code fix, fix it in place, without creating a function.
- If you need to store some string or number, extract it into a `const` when possible. **Exception: error messages in `.expect()` and `.unwrap_or_else()` calls do not need to be moved to a `const` — leave the string literal in place.**
- Do not add methods and functions you are not going to use right now.
- Comments in code are only for explaining non-obvious places.
- Do not write comments if the name of the function / method / struct / class already reflects the action or purpose performed.
- Do not write comments that refer to the work done, for example:
  - "After refactoring, A is done here, which does B." INSTEAD write: "A does B."
  - "Tests moved into a separate file because of size (~2900 lines)." Just do not write such a line — it does not relate to the code.
- Tombstone comments about absent code are forbidden. Marker words: "previously", "used to be", "lived here", "no longer needed", "before the refactoring", "removed". If the code is deleted, delete the mention of it too. A comment describes what the code does NOW, not its history.
- Do NOT write comments about which implementation stage this currently is.
- A comment ALWAYS describes the behavior of the current piece of code, not the developer's task or a stream of consciousness about what we are doing right now.
- Leave and do not touch comments if one of these words is next to them: debug, todo, note.
- Variable names must be single nouns, never compound or composite.
- Method names must be single verbs, never compound or composite.
- Favor "fail fast" paradigm over "fail safe": throw exception earlier.
- Constructors may not contain any code except assignment statements.
- A class name must reflect strictly the functionality it provides.
- Setters must be avoided, as they make objects mutable.
- Immutable objects must be favored over mutable ones.
- Every class may have only one primary constructor; any secondary constructor must delegate to it.
- Every class may encapsulate no more than four attributes.
- Every class must encapsulate at least one attribute.
- Utility classes are strictly prohibited.
- Static methods in classes are strictly prohibited.
- Method names must respect the CQRS principle: they must be either nouns or verbs.
- **No "post-constructors"** (`let x = T::new(...); x.with_foo(foo)` or chains like `T::new(...).with_foo(foo).with_bar(bar)`). If a type already has a `new` constructor, add the new parameter directly to its signature and update all callers. If there is no constructor, fill the struct literal completely in one place. Builder/`with_*` methods are allowed only if there are at least 3+ of them and the type genuinely needs flexible construction.
- **Separate module responsibilities**: the infrastructure layer (cache, IO, serde) must not contain domain logic. Infrastructure returns primitives (a file name, opening a writer, finalization) — coordinating the construction of entities lives in the domain module.
- **A function lives where it is used**: if a function is called only from one module, define it in that module as private, rather than `pub`/`pub(crate)` in a foreign one.

### Rust specific
- Always add `#[inline]` if a function or method body contains only a single line of code. **Exception: do not add `#[inline]` inside `mod tests`, `#[cfg(test)]` blocks, and mock modules (`mod mock`).**
- Try not to use `unwrap()`. Only where it is guaranteed there is no error at that spot.
- **`panic!` or `expect` is appropriate for invariant violations** — situations that must not occur under correct use of the code (a programming error, not a runtime error). Do not replace `panic!` with graceful error handling in such places.
- **Do not delete code marked with `// @keep`** — intentionally kept debug utilities.

### HOW TO WRITE UNIT TEST
- Briefly state in a comment what the test checks and what it expects.
- Every assertion must include a failure message that is a negatively toned claim about the error.
- IF tests fail with a "429 Rate Limit" error during a run, run that test alone separately and make sure it works.
- **Do NOT save real files in tests** — it interferes with parallel test execution. Use in-memory structures and `to_json()`/`from_json()` methods to check serialization.
- **Do NOT write tests for code inside `#[cfg(test)]` and mock files.**
- **Do NOT write tests that merely check a constant value** (`assert_eq!(CONST, 42)`). A test must check logic, not a definition.
- **Do NOT write tests that only cover `match`/`switch` mapping branches** (e.g. `category_key(Category::X) == "x"`). Mappings are declarative data, not logic. Such tests are equivalent to checking a constant and provide no value.
- **Do NOT write tests for Display, logging, or print functions.** It is important to test business logic and its edge cases.
- **A test must check a SUCCESSFUL result**, not swap one error for another. If a fix repairs error X, the test must show the operation completes successfully (expect/unwrap), not that error Y now occurs instead of X. If it is impossible to write a test with a successful result — say so directly, rather than masking the problem with an assert on a different error.
- **Do NOT use local imports (block-scoped / function-scoped `use`) inside tests and test helpers.** All `use crate::...` / `use super::...` must be at the top of `mod tests` (or at the module level for a helper function). A local `use` inside a `#[test]` or a helper function hides the test's dependencies, hinders grepping "who uses what", and breeds duplicates when a test is copied. Exception — resolving a name collision or `use ... as alias` for one or two calls.

Run the CLI: `./target/debug/gasripper <input>` (or `cargo run -- <input>`). With only an
input path, all features are on and it prints a strip report. Key flags: `--emit-asm`,
`--emit-bytecode`, `--disable guards`, `--config <file>`, `--input-kind`, `--list-features`,
`--inline-max-body <n>` (inline body-size threshold, default 30).

## Project documentation

Before working and when making changes, cross-check against these documents (they live in the
repository root):

- [README.md](README.md) — project overview, safety model, features, installation, usage,
  environment variables.
- [DEVELOPMENT.md](DEVELOPMENT.md) — full testing/e2e setup, toolchain env vars, the real-EVM
  harness, and how to add a feature.

Rule: whenever you change a feature, you MUST check and update its own documentation — the
`README.md` inside that feature's folder (`src/features/<feature>/README.md`).

## Architecture

Pipeline: **input frontend → instructions → `features::optimize` (feature-gated passes) → report / emit**.

- `src/core/` — `asm.rs` (the `Instr` representation + parser), `stack.rs::strip_residue` (the
  safety criterion: a guard is removable only if its fall-through stack keeps live values intact,
  returning the minimal `POP`/`SWAP` shuffle), `strip.rs::strip_guards` (the engine that finds
  `<cond> _sym_*revert* JUMPI` runs and rewrites them when the feature is enabled; then runs
  post-strip DCE — `dead_revert_spans` deletes any `_sym_*revert*` block orphaned by the strip
  (no remaining reference + unreachable by fall-through), always-safe dead-code removal),
  `bytecode.rs`, `opcodes.rs`.
- `src/features/` — one module per gas-reduction pass, each owning its `META` + a rewrite fn + tests.
  Seven ship by default, plus `superopt` in an `smt`-feature build: `guards` (all revert-guard removal via `strip_guards`; the former `abi`/`math`/`assert`
  split was a leaky opcode-sniff and was merged), `shuffle` (always-safe stack-shuffle rescheduling
  via `core::stack::minimize_shuffle`, symbolic input only), `involution` (always-safe `NOT NOT`
  cancelling, symbolic only), `recompute` (always-safe `OP DUP1` → `OP OP` for a cheap result-invariant
  nullary opcode; length-preserving, so it runs on BOTH symbolic and concrete input — the one pass that
  also lowers concrete `.hex`/`.bin` gas), `fold_shift` (always-safe precompute of a constant
  `PUSH a PUSH b SHL/SHR` into one push; length-changing/symbolic, and it GROWS bytecode to lower
  per-call gas), `cmpnorm` (always-safe fold of a `SWAP1` before a strict-order comparison into the
  mirrored comparator — `SWAP1 LT` → `GT`; length-changing/symbolic), and `inline` (relocate a small
  `@internal` function with 2+ call sites into its call sites; `core::inline` analyses + de-threads,
  `features::inline` orchestrates; three strategies — DE-THREAD a straight-line tail-return body
  (`dethread_tail_return`), DE-THREAD a single-merge `if`/`else` diamond (`dethread_diamond`: deletes
  the merge, joins both arms at a fall-through label, also drops venom's branch-arm double jump),
  relocate any other branching body VERBATIM; length-changing/symbolic; the FIRST
  feature with a numeric parameter — body-size threshold `inline_max_body`, default 30, via
  `--inline-max-body`; runs FIRST so its spans take precedence, and optimizes each relocated body with
  the other passes so it never raises gas), and `superopt` (**opt-in, `smt` Cargo feature**) — SMT block
  superoptimization via Z3: for a pure straight-line block (`core::superopt`, only stack moves +
  interpreted arithmetic) it search-and-proves the cheapest gas-equivalent sequence; length-changing/
  symbolic. The Z3 dep is optional so the default binary stays pure-`std`; gated by `#[cfg(feature = "smt")]`
  on `Category::Superopt`, the module, the registry entry and the `optimize_with` call. **Which passes actually FIRE on which compiler, the
  venom/solc asm idioms and call/return conventions, the toolchain env vars, and the e2e gotchas live
  in the `gasripper-vyper` and `gasripper-solidity` skills — invoke the matching one before working on
  a language's output.** `features::optimize`/`optimize_with` run the enabled passes and merge their
  edit spans via `merge_nonoverlapping` (a later pass yields to an earlier one on overlap). Add a pass:
  a module here, register its `META` in `features::registry()`, and run it from `features::optimize_with`.
- `src/config.rs` — `FeatureConfig` with precedence defaults → config file → CLI; `enabled_categories()`
  feeds the engine; the one numeric parameter (`inline_max_body`) is read alongside the feature toggles.
- `src/input/` — frontends produce `Loaded { instrs, symbolic, kind }`. `raw_asm`/`bytecode` are
  supported; `vyper`/`solidity` shell out to the compiler and are **experimental**.

## Critical constraint: symbolic vs. concrete

Guards are detected **by symbolic revert labels** (`_sym_*revert*`), so stripping works on symbolic
assembly only — not on resolved raw bytecode/Solidity. `Loaded.symbolic` gates emission: symbolic
programs allow `--emit-asm` only; concrete `.hex`/`.bin` round-trip to `--emit-bytecode`. There is
deliberately **no hand-written linker** (wrong bytecode in a gas tool is dangerous).

`--emit-creation` (Vyper/Solidity) produces deployable creation bytecode and the **compiler's own
assembler** re-links it (constructor untouched), so gasripper writes no linker. `src/sidecar.rs`
holds the shared `Backend` and dispatches per language:
- **Solidity** is **native Rust** (`src/solc.rs`, no Python): it drives the `solc` binary's
  `--asm-json` ⇄ `--import-asm-json` round-trip directly, parsing/editing the asm JSON with
  `serde_json` (the only reason `serde_json` is a dependency). A user with just `solc` on PATH needs
  no Python.
- **Vyper** still uses a Python sidecar (`scripts/vyper_sidecar.py`) because venom's assembler
  (`compile_ir.assembly_to_evm`) is a Python library function with **no CLI equivalent**; the backend
  shells out to a Python with the `vyper` package importable. The script is embedded in the binary via
  `include_str!` and materialized to a temp cache at runtime, so `cargo install` ships no loose files.

A baseline invariant (assemble with no deletions == the compiler's reference bytecode) fails fast on
drift. Toolchains come from `GASRIPPER_*` env vars (exact paths, the revert-idiom normalization, and
the version-pinned gas caveat are in the `gasripper-vyper` / `gasripper-solidity` skills). `revm` is a
**dev-dependency only** (e2e gas proofs); the shipped binary depends on `clap`/`tracing`/`serde_json`.

## Safety invariants (do not break)

`is_auth`/`is_side` in `strip.rs` are the preservation sets. Stripping is only safe under a trusted
caller; the README disclaimer (first line) and these sets are load-bearing, not cosmetic.
