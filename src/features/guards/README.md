# Feature `guards` — strip provably-safe revert guards

Project-wide safety model: [main README](../../../README.md).

## What it strips

Every redundant revert guard a compiler inserts before a function body runs, detected as
`<cond> _sym_*revert* JUMPI`:

- overflow / underflow assertions — `a + b`, `a - b`, `a * b`;
- division-by-zero checks — `a // b`;
- ABI / calldata bounds — length and offset validation;
- range / cast asserts — `convert(x, uintN)`, `require(x < N)`.

Each is removed only when its fall-through stack is reproduced exactly — a stack identity
(deleted outright) or a minimal `POP`/`SWAP` residue. Authorization (`CALLER`/`ORIGIN`) and
side effects are never touched.

## Bytecode patterns (before → after)

Two transformation kinds, both verified by stripping the assembly below (`--input-kind asm`).
`R` is a `_sym_*revert*` label.

**1. Identity — the guard reads its inputs via `DUP`/`SWAP` and consumes nothing, so the whole
run is deleted** (the fall-through stack is already unchanged):

```text
; ABI / calldata bounds check
DUP1 CALLDATALOAD PUSH1 32 LT R JUMPI   →   (removed)

; range / cast check  (convert(x, uint128): x >> 128 == 0)
DUP1 PUSH1 128 SHR ISZERO R JUMPI       →   (removed)

; division-by-zero check  (a // b: b != 0) — DIV kept
... DUP1 ISZERO R JUMPI ... DIV         →   ... DIV
```

**2. Consuming — the guard eats a spare operand, so the run is replaced by the minimal `POP`/`SWAP`
that reproduces its residue**; the live arithmetic (`ADD`/`SUB`) is kept:

```text
; a + b overflow assertion  (revert if a+b < b)
... ADD SWAP1 DUP2 LT R JUMPI           →   ... ADD SWAP1 POP

; a - b underflow assertion  (revert if a-b > a)
... SUB SWAP1 DUP2 GT R JUMPI           →   ... SUB SWAP1 POP

; a bare consuming compare  (assert x > 5)
PUSH1 5 GT R JUMPI                      →   POP
```

The `a * b` overflow check (`product / a == b`, an inverse idiom carrying `DIV`/`EQ`/`OR`) is the
same consuming case — its run is replaced by the residue that keeps the product on the stack.

## One feature, not three

Earlier versions split this into `abi` / `math` / `assert` by sniffing which opcodes sat in the
removed run. That label was fragile and **leaked**: the same calldata bounds check classified as
`abi` on one compiler and `math` on another (e.g. solc 0.8.24 keeps `CALLDATASIZE` live and the
removed run carries only `SUB`/`SLT`), so `--disable abi` never reliably "kept calldata checks".
The three classes were one mechanism — the strip engine — so they are now one feature. This is the
analog of rewriting `a <op> b` as Vyper's `unsafe_<op>(a, b)` or wrapping it in Solidity's
`unchecked { … }`, applied across overflow, bounds, and range guards at once.

## Safety

Safe **only** under a trusted caller that always supplies well-formed calldata and in-range
inputs. The removed run's live-stack residue is reproduced exactly, so the only behavioral change
is "no longer revert on an input the trusted caller never sends".

## Measured (real EVM, revm — `e2e.rs`)

18 cases span both languages × (`+ - * /`, range/cast) × (auth / no-auth). Examples (Vyper 0.4.3,
solc 0.8.24), result unchanged:

| Case | call gas | notes |
|---|---|---|
| Vyper `a + b` (auth) | 23631 → 23593 | overflow assertion removed |
| Vyper `a * b` (auth) | 23671 → 23633 | overflow (`product / a == b`) removed |
| Solidity `a - b` (auth) | 23843 → 23793 | `Panic(0x11)` underflow guard removed |
| Solidity `require(a < 256)` (auth) | 23617 → 23545 | range check removed |

No-auth single-function contracts show a bytecode win (the guard is off the hot path).

## Files

| File | Purpose |
|---|---|
| `mod.rs` | `META`, `strip()`, pattern unit tests |
| `e2e.rs` | real-EVM proofs (18 cases) via `features::e2e_harness` |
