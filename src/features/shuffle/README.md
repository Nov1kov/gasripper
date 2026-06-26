# Feature `shuffle` — stack-shuffle minimization

Project-wide safety model: [main README](../../../README.md).

## What it does

Replaces a maximal run of pure stack-scheduling ops (`POP`/`DUPn`/`SWAPn`) that a compiler left
non-minimal with the **cheapest equivalent** run producing the identical stack. It computes no value
and touches no memory/storage, so — unlike [`guards`](../guards/README.md) — it needs **no trusted
caller**. It is the first always-safe pass.

## When it fires

A minimal Vyper contract: two products subtracted.

```vyper
@external
def foo(a: uint256, b: uint256, c: uint256, d: uint256) -> uint256:
    return a * b - c * d
```

Vyper 0.4.3 (`venom`) schedules the operand juggling as a five-op window that is a roundabout way to
duplicate two stack slots; gasripper proves it equals a two-op window and emits the shorter one:

```text
SWAP1 DUP2 SWAP1 DUP1 SWAP3   (15 gas, 5 bytes)   →   DUP2 DUP2   (6 gas, 2 bytes)
```

```bash
gasripper foo.vy --input-kind vyper --disable guards --emit-creation out.hex
#   [25..29] shuffle -> SWAP1 DUP2 SWAP1 DUP1 SWAP3
```

venom leaves such windows wherever independent subexpressions merge through a commutative/associative
reduction (`a*b - c*d`, `a*b + b*c`, a loop accumulator). Other reschedules it finds:
`SWAP1 POP SWAP1 POP → SWAP2 POP POP` (−3), `SWAP1 SWAP1 → ∅` (−6), `DUP2 POP → ∅` (−5).

## Savings per language

Measured on a real EVM (revm) and a multi-contract firing sweep; the result is always unchanged.

| Compiler | Fires? | Representative win |
|---|---|---|
| Vyper 0.4.3 (venom, `GAS`) | **yes** | `a*b - c*d`: −9 gas / −3 bytes per window. Loop `for i in range(n): s += i*i`: call gas 22049 → 22031 (−18 over 5 iterations), creation 169 → 167 — e2e [`e2e.rs`](e2e.rs). |
| Solidity solc 0.8.24 (`--optimize`) | **no — 0 firings** | solc schedules its own stack; nothing is left to reschedule (0 across a 10-contract sweep). |

`shuffle` is a Vyper-effective pass; on Solidity the gate is correct but finds nothing, and that
language's gas wins come from [`guards`](../guards/README.md).

## How it is sound

Stack ops move/copy/drop slots by position without inspecting values, so two windows are equivalent
iff they map an **all-distinct** stack identically (one test vector decides it). The engine
([`core::stack::minimize_shuffle`](../../core/stack.rs)):

1. computes the window's minimal safe input depth `D` (a replacement reaching deeper underflows, so
   it is rejected);
2. runs the window over an all-distinct stack of height `D` for its net effect (`target`);
3. Dijkstra by gas over `POP`/`DUPn`/`SWAPn` for the cheapest sequence reaching `target` without
   going below `D`;
4. returns it **only** if strictly cheaper than the input.

Equality at depth `D` proves equality on every taller stack (deeper slots are untouched) and step (3)
never reaches below the window's footprint, so live values below it are never disturbed — the rewrite
is gas-monotone by construction.

## Scope

A window is a maximal run of consecutive `POP`/`DUPn`/`SWAPn`; any other op, a `PUSH`, or a label
ends it, so every window is basic-block-local. The pass runs on **symbolic input only**: a length
change shifts `JUMPDEST` offsets and gasripper has no linker, so it relies on the sidecar/compiler to
relink — the orchestrator [`features::optimize`](../mod.rs) gates it on `core::asm::is_symbolic`, and
on concrete `.hex`/`.bin` it never fires. A shuffle window overlapping a guard span is dropped.

## Search bound and progress

The Dijkstra state space grows roughly factorially in the window's depth, so a window deeper than
`MAX_RESCHEDULE_DEPTH` (6 — every observed venom win is shallower) is **skipped, not searched**:
optimizing it is computationally infeasible (a large real contract emits depth-12+ permutation
windows that would each need ~1e29 brute-force ops). Within the bound a per-window node cap is a
further backstop. Correctness is unaffected — the bound only limits *which* windows are searched,
never the proof that a returned rewrite is equivalent and strictly cheaper.

On a large program the pass logs an up-front estimate (searchable windows, rough total search steps
and time, and how many are too deep) and periodic progress with an ETA, so a long run is never a
silent hang — e.g. a ~12 k-instruction contract reports `315 windows — 279 searchable (~1.3 s),
36 too deep (skipped)` and finishes its search in ~1 s.
