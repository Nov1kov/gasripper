# Feature `foldshift` — precompute a constant `PUSH a PUSH b SHL/SHR`

Project-wide safety model: [main README](../../../README.md).

## What it does

Replaces a `PUSH a PUSH b SHL` (or `SHR`) of two concrete literals with a single push of the
precomputed 256-bit result:

```text
PUSH a PUSH b SHL   →   PUSH (a << b)        (a, b concrete literals)
PUSH a PUSH b SHR   →   PUSH (a >> b)
```

Two `PUSH` (3 gas each) plus the shift (3 gas) = 9 gas computed on **every** execution; one push of
the constant is 3 gas — a flat **6 gas per occurrence** saved. A `PUSH a PUSH b SHL` window takes no
stack input and leaves one output (the constant), so a single push of that same constant is value-
and stack-identical regardless of surrounding code: the fold needs **no trusted caller**. It is an
always-safe pass. Only the shift family is folded — general `PUSH a PUSH b <arith>` is left to the
compiler (it already folds it, and 256-bit-wrapping arithmetic folds are the easiest place to ship a
bug; see [`todo-ebso-features/04`](../../../todo-ebso-features/04-constant-folding-and-arithmetic-simplification.md)).

## When it fires

solc deliberately materializes large constants with this idiom to keep the **bytecode small** — the
address-cleaning mask `1 << 160` is emitted as `PUSH1 0x01 PUSH1 0xa0 SHL` (5 bytes) instead of
`PUSH21 0x0100…00` (22 bytes). Any function with an `address` argument runs that mask on its hot path:

```solidity
// each address argument is cleaned with `and(addr, sub(shl(160, 1), 1))` — the shl(160,1) is the
// PUSH1 1 PUSH1 0xa0 SHL idiom, folded here to a single PUSH21 literal
contract C {
    mapping(address => uint256) public bal;
    function transfer(address to, uint256 amt) external returns (bool) {
        bal[msg.sender] -= amt;   // mask on msg.sender
        bal[to] += amt;           // mask on `to`
        return true;
    }
}
```

Vyper's venom does not emit this idiom, so the pass never fires on Vyper output.

## Savings — size traded for gas

gasripper is an aggressive **gas** optimizer, so it makes the opposite trade to the compiler: it
spends bytecode to lower the per-call gas. The fold therefore **grows the creation bytecode** while
lowering runtime gas. Measured on a real EVM (revm); the result is always unchanged.

Measured (solc 0.8.24 `--optimize`, e2e [`e2e.rs`](e2e.rs)): the `1 << 160` address mask on
`transfer(address,uint256)` drops call gas **26518 → 26506** (−12, the mask runs twice — on `to` and
`msg.sender`) while growing the creation bytecode **473 → 532** (+59).

> **Why the saving is six gas per executed occurrence.** Only the folds actually executed save gas;
> solc also materializes revert-string selectors with the same idiom (`PUSH3 sel PUSH1 0xe5 SHL`), but
> those live in revert paths a successful call never enters — folding them only grows the bytecode. The
> pass folds them anyway (it cannot cheaply tell hot from cold), which is why the bytecode grows more
> than the hot-path saving alone would imply.

## How it is sound

A `PUSH a PUSH b SHL` window pushes two literals and pops exactly them, leaving one value — the
compile-time constant `a << b mod 2^256`. Replacing the three instructions with a single push of that
constant produces the identical stack with the identical value, so the rewrite is correct in any
context with no stack model and no trusted caller. The result is computed with exact 256-bit EVM
semantics (a shift `≥ 256` yields `0`); a window whose result is `0` is left alone (the
materialization idiom never produces it). Detection requires **two concrete literal operands** — a
symbolic / linker-resolved push (jump target, code size, library address) carries no literal value and
is never folded.

## Scope — length-changing, symbolic (relinkable) input only

Folding three instructions (≥ 5 bytes) into one push (up to 33 bytes) shifts every later `JUMPDEST`
offset, so — like [`shuffle`](../shuffle/README.md) and [`involution`](../involution/README.md) — the
pass runs **only on symbolic programs**, where the compiler's own assembler relinks via the sidecar
(`--emit-creation`). The folded literal is carried to the sidecar as a `#<hex>` edit token; the
sidecars stay dumb and just emit the push. A foldshift span overlapping a guard, shuffle, involution,
or recompute span is dropped.
