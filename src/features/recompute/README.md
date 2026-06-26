# Feature `recompute` — recompute a cheap nullary opcode instead of `DUP`-ing it

Project-wide safety model: [main README](../../../README.md).

## What it does

Replaces a `DUP1` that duplicates the result of an immediately-preceding cheap, result-invariant
nullary opcode with a second copy of that opcode:

```text
OP DUP1   →   OP OP        (OP ∈ the allow-list below)
```

`DUP1` costs 3 gas (`G_verylow`); each allow-listed opcode costs only 2 (`G_base`) and reads nothing,
so re-executing it leaves the **identical** stack one gas cheaper. It computes no value and touches no
memory/storage, so — like [`shuffle`](../shuffle/README.md) and [`involution`](../involution/README.md)
— it needs **no trusted caller**. It is an always-safe pass.

## When it fires

Both compilers leave the pattern in their optimized output:

```text
# Solidity: the non-payable guard, once per call (and PUSH0 DUP1 in every revert block)
CALLVALUE DUP1 ISZERO PUSH[tag] JUMPI ...   →   CALLVALUE CALLVALUE ISZERO PUSH[tag] JUMPI ...
PUSH0 DUP1 REVERT                           →   PUSH0 PUSH0 REVERT

# Vyper: an environment value used more than once (here `self`)
ADDRESS DUP1 ADDRESS ADD ...                →   ADDRESS ADDRESS ADDRESS ADD ...
```

```solidity
// every external non-payable function carries solc's CALLVALUE DUP1 guard
function f(uint256 n) external pure returns (uint256 s) {
    for (uint256 i = 0; i < n; i++) { s += i; }
}
```

```vyper
# venom keeps <env-op> DUP1 in the loop body when an env value is read twice per iteration,
# and does NOT hoist it (e.g. ADDRESS DUP1 ADDRESS for `self`, CHAINID DUP1 for `chain.id`)
@external
@view
def f(n: uint256) -> uint256:
    s: uint256 = 0
    for i: uint256 in range(n, bound=128):
        s += chain.id * chain.id
    return s
```

Neither compiler folds `OP DUP1` into `OP OP`, so this is a real, ubiquitous one-gas-per-occurrence
win.

## Savings per language

Measured on a real EVM (revm); the result is always unchanged and the creation bytecode keeps the
**same size** (a single-byte opcode swapped for another).

| Compiler | Fires? | Measured win (e2e [`e2e.rs`](e2e.rs)) |
|---|---|---|
| Solidity solc 0.8.24 (`--optimize`) | **yes** | the call-path `CALLVALUE DUP1` non-payable guard → `CALLVALUE CALLVALUE`: `f(uint256)` call gas 22103 → 22102 (−1), creation bytecode unchanged in size. |
| Vyper 0.4.3 (venom, `GAS`) | **yes** | per-iteration `CHAINID DUP1` → `CHAINID` in the loop body: `f(uint256)` call gas 22099 → 22094 (−5 over 5 iterations), creation bytecode unchanged in size. Same shape with `ADDRESS DUP1 ADDRESS` when `self` is read twice. |

> **Why the saving is one gas per executed occurrence.** Only the opcodes actually executed save gas;
> solc's other recomputed idioms (`PUSH0 DUP1 REVERT`/`RETURN`) live in revert blocks a successful
> call never enters, so on the happy path only the single `CALLVALUE DUP1` guard fires (−1). Under
> EIP-7623 a one-shot saving below the calldata floor is hidden at the transaction level, so each e2e
> uses a loop body to keep execution above the floor — exactly as [`shuffle`](../shuffle/README.md) and
> [`involution`](../involution/README.md) do (the Vyper case recomputes once per iteration, −5 over 5).

## How it is sound

Every opcode in the allow-list is **nullary** (pops nothing, pushes one word) and **result-invariant
within a transaction**: `PUSH0` is the constant `0`, and the environment opcodes return the same word
throughout one execution. So `OP` immediately followed by `DUP1` pushes that word twice — exactly what
`OP OP` does — leaving every deeper stack slot untouched. A maximal run of `DUP1` directly after the
opcode duplicates the same invariant word each time, so every `DUP1` in the run is rewritten. The
proof is purely local and needs no stack model and no trusted caller.

**Allow-list** (`G_base` = 2 gas, nullary, transaction-invariant, single-byte):
`PUSH0`, `ADDRESS`, `ORIGIN`, `CALLER`, `CALLVALUE`, `CALLDATASIZE`, `CODESIZE`, `GASPRICE`,
`COINBASE`, `TIMESTAMP`, `NUMBER`, `PREVRANDAO`, `GASLIMIT`, `CHAINID`, `BASEFEE`, `BLOBBASEFEE`.

**Excluded on purpose:** `GAS`/`PC` (change per executed op / position), `MSIZE`/`RETURNDATASIZE`
(change with memory / after a `CALL`), `BALANCE`/`SELFBALANCE` (state-dependent and not `G_base`), and
`PUSH1..PUSH32` (recomputing them would need the immediate and would **grow** the bytecode, since they
are multi-byte). Only `DUP1` is rewritten — a deeper `DUPn` duplicates a different slot, not the
opcode's value. No gas table keyed by EVM version is needed: every allow-listed opcode is a stable
2-gas `G_base` op, and if it appears in the input the target fork already supports it.

## Scope — length-preserving, runs on symbolic AND concrete input

Unlike `shuffle`/`involution`, this rewrites one single-byte opcode (`DUP1`, `0x80`) into another
single-byte opcode, so it **never shifts a `JUMPDEST` offset**. The orchestrator
[`features::optimize`](../mod.rs) therefore runs it on **every** program — not only the symbolic
(sidecar) path but also raw concrete `.hex`/`.bin` bytecode, where no compiler relinks for us. It is
the one shipped pass that lowers gas on hand-written / already-deployed bytecode. A recompute span
overlapping a guard, shuffle, or involution span is dropped.
