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

## Post-strip dead-block elimination (DCE)

Removing the guards above deletes the `<cond> _sym_*revert* JUMPI` runs that jumped to a
revert block. When a shared `_sym_*revert*` block loses its **last** reference, it becomes
unreachable dead weight. After the strip, gasripper deletes any `_sym_*revert*` block that
(1) is no longer the target of a remaining `PushSym`/`_OFST`, and (2) cannot be reached by
fall-through (its predecessor halts — `RETURN`/`REVERT`/… — or is an unconditional `JUMP`):

```text
... RETURN  _sym___revert: JUMPDEST PUSH0 DUP1 REVERT   →   ... RETURN
            └─ no JUMPI targets it after the strip ─┘        (block deleted)
```

This is **always-safe** (deleting unreachable code cannot change execution) and the compiler's
own assembler relinks the surviving jumps. A revert block still reached by a NON-stripped guard
(e.g. an auth `assert msg.sender == owner`) keeps its reference and is preserved.

Solidity is unaffected by this pass: its revert blocks are labelled `_sym_tag_*` (no `revert`
substring), and its `require`-form (inverse-idiom) inline reverts are already dropped during the
strip by the solc sidecar.

## One feature, not three

Earlier versions split this into `abi` / `math` / `assert` by sniffing which opcodes sat in the
removed run. That label was fragile and **leaked**: the same calldata bounds check classified as
`abi` on one compiler and `math` on another (e.g. solc 0.8.24 keeps `CALLDATASIZE` live and the
removed run carries only `SUB`/`SLT`), so `--disable abi` never reliably "kept calldata checks".
The three classes were one mechanism — the strip engine — so they are now one feature. This is the
analog of rewriting `a <op> b` as Vyper's `unsafe_<op>(a, b)` or wrapping it in Solidity's
`unchecked { … }`, applied across overflow, bounds, and range guards at once.

## Safety

Safe **only** under a trusted caller that always supplies well-formed calldata and in-range inputs.

Everything rests on a stack criterion. For each `<cond> _sym_*revert* JUMPI` gasripper grows the
longest barrier-free suffix it can cut by **reproducing that run's live-stack residue** (simulated
over slot-ids), so the fall-through (non-reverting) stack is byte-for-byte unchanged and only the
revert is gone — a stack **identity** is deleted outright, a **consuming** check is replaced by the
minimal `POP`/`SWAP` residue (the two cases under [patterns](#bytecode-patterns-before--after)). The
only behavioral change is "no longer revert on an input the trusted caller never sends".

A run is removed only if its residue consists solely of input slots (it creates no value that
survives into live code). A residue strip that *drops* a value is additionally refused when its
straight-line block contains an auth (`CALLER`/`ORIGIN`) or side-effect opcode, so a `msg.sender`
check or a call's success flag is never dropped.

**Always preserved** (regardless of enabled features):

- authorization — any run touching `CALLER`/`ORIGIN` (`msg.sender == owner`);
- side effects — `SSTORE`/`CALL`/`MSTORE`/`LOG*`/`RETURN`/…;
- checks that consume their own input (not a stack identity — possible profit guards);
- any suffix containing a label or a non-terminal `JUMP(I)`.

The preservation sets `is_auth`/`is_side` live in [`src/core/strip.rs`](../../core/strip.rs).

## Measured (real EVM, revm — `e2e.rs`)

18 cases span both languages × (`+ - * /`, range/cast) × (auth / no-auth). Examples (Vyper 0.4.3,
solc 0.8.24), result unchanged:

| Case | call gas | notes |
|---|---|---|
| Vyper `a + b` (auth) | 23631 → 23593 | overflow assertion removed |
| Vyper `a * b` (auth) | 23671 → 23633 | overflow (`product / a == b`) removed |
| Solidity `a - b` (auth) | 23843 → 23793 | `Panic(0x11)` underflow guard removed |
| Solidity `require(a < 256)` (auth) | 23617 → 23545 | range check removed |

No-auth single-function contracts show a bytecode win (the guard is off the hot path); when the
strip orphans the shared revert block, post-strip DCE removes it too — e.g. a Vyper
`convert(data, uint256)` contract goes 142 → 120 bytes from the guard strip, then 142 → 116 with
the orphaned `_sym___revert` block eliminated.
