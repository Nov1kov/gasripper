# Feature `cmpnorm` — fold a `SWAP1` before a comparison into the mirrored comparator

Project-wide safety model: [main README](../../../README.md).

## What it does

Replaces a `SWAP1` immediately followed by a strict-order comparison with the single mirrored
comparator, dropping the `SWAP1`:

```text
SWAP1 LT    →   GT
SWAP1 GT    →   LT
SWAP1 SLT   →   SGT
SWAP1 SGT   →   SLT
```

`SWAP1` exchanges the top two stack words — exactly the two operands the comparison is about to
consume — so swapping them and then comparing equals comparing in the reversed direction without the
swap. The two-instruction window collapses to one opcode, saving the `SWAP1` (3 gas, one byte) on
**every** executed occurrence. The window takes the same two words in and leaves the same boolean out,
so the rewrite is value- and stack-identical regardless of surrounding code: it needs **no trusted
caller**. It is an always-safe pass.

`EQ` is deliberately **not** handled: equality is symmetric, so the compiler already drops any `SWAP1`
before `EQ` and the idiom never reaches us — an inert rule would be pure noise.

## When it fires

When a comparison's two operands are independent freshly-computed subexpressions, Vyper's venom
backend lands them on the stack in evaluation order and emits a `SWAP1` to put them in comparison
order, rather than re-scheduling the producers:

```vyper
@external
@view
def f(x: uint256, y: uint256, n: uint256) -> uint256:
    s: uint256 = 0
    for i: uint256 in range(n, bound=128):
        if (x * i) < (y * i):   # two computed products → `... SWAP1 LT ...`
            s += 1
    return s
```

solc instead selects operand order via `DUP` depth and never emits the idiom, so the pass never fires
on Solidity output. The `SWAP2`-before-comparison idiom co-occurs in venom output but is left
untouched: `SWAP2` exchanges `s0` and `s2`, not the two comparison operands, so folding it would
change the result.

## Savings — bytecode shrinks, gas drops

Folding two instructions into one removes the `SWAP1` byte (the bytecode shrinks) and saves its 3 gas
on every executed occurrence. Measured on a real EVM (revm); the result is always unchanged.

Measured (Vyper 0.4.3 venom `OptimizationLevel.GAS`, e2e [`e2e.rs`](e2e.rs)): the per-iteration
`(x * i) < (y * i)` comparison in the loop above drops call gas **22783 → 22768** (−15 = 3 gas over 5
iterations) while the creation bytecode shrinks **203 → 202** bytes.

## How it is sound

`LT` pops the top word `a`, then the next word `b`, and pushes `a < b`; `GT` pushes `a > b`. A
preceding `SWAP1` exchanges those top two words, so `SWAP1 LT` computes `b < a` — which is exactly
`GT` on the original order. Symmetrically `SWAP1 GT == LT`, and the signed forms `SWAP1 SLT == SGT`,
`SWAP1 SGT == SLT` compare the same two words. The comparison must directly follow the `SWAP1` (the
two words it swaps are the two the comparison consumes); a label or any other op between them breaks
the match, so the rewrite is basic-block-local and never crosses a jump target.

## Scope — length-changing, symbolic (relinkable) input only

Folding two instructions into one shifts every later `JUMPDEST` offset, so — like
[`shuffle`](../shuffle/README.md), [`involution`](../involution/README.md), and
[`foldshift`](../fold_shift/README.md) — the pass runs **only on symbolic programs**, where the
compiler's own assembler relinks via the sidecar (`--emit-creation`). The mirrored comparator is
carried to the sidecar as a bare-opcode edit token. A cmpnorm span overlapping a guard, shuffle,
involution, recompute, or foldshift span is dropped.
