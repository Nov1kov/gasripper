# Feature `involution` — cancel involutive op runs

Project-wide safety model: [main README](../../../README.md).

## What it does

Replaces a maximal run of consecutive `NOT` opcodes with its net effect. `NOT` is an **involution**
(`NOT(NOT(x)) == x` for every 256-bit `x`), so an even-length run is a no-op (deleted entirely) and
an odd-length run equals a single `NOT`. It computes no value and touches no memory/storage, so —
like [`shuffle`](../shuffle/README.md) — it needs **no trusted caller**. It is an always-safe pass.

## When it fires

A Vyper contract that double-complements a value:

```vyper
@external
def f(x: uint256) -> uint256:
    return ~(~x)
```

Vyper 0.4.3 (`venom`, `OptimizationLevel.GAS`) does **not** fold the double complement — it lowers
`~(~x)` to a literal `NOT NOT` on the runtime path:

```text
... CALLDATALOAD NOT NOT PUSH1 0x40 MSTORE ...   →   ... CALLDATALOAD PUSH1 0x40 MSTORE ...
```

`~(~x)`-style double complements survive wherever the source writes them (directly, or via a helper
that returns `~y` fed into another `~`). The same shape from a loop body (`s += ~(~i)`) keeps the
`NOT NOT` inside the loop, so the saving compounds per iteration.

## Savings per language

Measured on a real EVM (revm); the result is always unchanged.

| Compiler | Fires? | Representative win |
|---|---|---|
| Vyper 0.4.3 (venom, `GAS`) | **yes** | loop `for i in range(n): s += ~(~i)`: call gas 21784 → 21754 (−30 over 5 iterations), creation 153 → 151 bytes — e2e [`e2e.rs`](e2e.rs). |
| Solidity solc 0.8.24 (`--optimize`) | **no — 0 firings** | solc's optimizer folds `~~x` to nothing before the asm stage; no `NOT NOT` survives. |

`involution` is a Vyper-effective pass; on Solidity the gate is correct but finds nothing.

> **Why a single-call `~(~x)` shows no gas drop but a loop does.** Under EIP-7623 a transaction pays a
> floor priced on its calldata, and a one-shot `~(~x)` body executes below that floor — so removing
> two `NOT`s (6 gas) is hidden by the floor at the transaction level (the bytecode still shrinks). In
> a loop the body runs every iteration, clearing the floor into a measurable call-gas drop. The e2e
> uses the loop shape for exactly this reason (as `shuffle`'s does).

## How it is sound

`NOT` pops one word and pushes its bitwise complement, leaving every deeper stack slot untouched. Two
in a row restore the original word, so cancelling a pair is value-preserving for all inputs — no
stack model and no trusted caller needed. A maximal run contains only `NOT` opcodes (any other op or
a label ends it), so the cancellation is basic-block-local and never crosses a value the run does not
itself produce.

## Why not `ISZERO ISZERO`

`ISZERO ISZERO` is **not** an unconditional involution: `ISZERO(ISZERO(x)) == (x != 0)`, which equals
`x` only when `x` is already a boolean (`0`/`1`). On real compiler output the surviving
`ISZERO ISZERO` occurrences are **not locally removable** — every one is either genuinely necessary
(its input is a raw `uint`, e.g. `return a > 0` → `iszero(iszero(a))`, so dropping the pair returns
the raw value — a correctness bug) or redundant only because a boolean was produced in a *different*
basic block reached by an indirect `JUMP`, which a sound local peephole cannot see. Capturing those
needs cross-block boolean-provenance dataflow, not this pass, so `ISZERO ISZERO` is deliberately left
alone. `SWAPn SWAPn` (the other involution) is already handled by [`shuffle`](../shuffle/README.md).

## Scope

A run is a maximal sequence of consecutive `NOT` opcodes; any other op or a label ends it, so every
run is basic-block-local. The pass runs on **symbolic input only**: deleting opcodes shifts
`JUMPDEST` offsets and gasripper has no linker, so it relies on the sidecar/compiler to relink — the
orchestrator [`features::optimize`](../mod.rs) gates it on `core::asm::is_symbolic`, and on concrete
`.hex`/`.bin` it never fires. An involution span overlapping a guard or shuffle span is dropped.
