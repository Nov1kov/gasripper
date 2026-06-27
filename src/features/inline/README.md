# Feature `inline` — relocate a small internal function into its call sites

Project-wide safety model: [main README](../../../README.md).

## What it does

Vyper's venom backend keeps every `@internal` function that has **two or more call sites** as a
separate runtime block, reached by a fixed call convention:

```text
  <pushsym ret>           ; the return address (a continuation label just after the call)
  <pushsym entry>         ; the function's entry symbol
  JUMP                    ; transfer to the function
  <ret> JUMPDEST          ; control returns here; the body JUMPs back to <ret>
```

Each call therefore pays a fixed indirection — `pushsym entry` (3 gas), the call `JUMP` (8 gas),
and the entry `JUMPDEST` (1 gas) = **12 gas per call** — plus the return: `pushsym ret` (3 gas),
the body's return `JUMP` (8 gas), and the `ret JUMPDEST` (1 gas). This pass splices the body into
each call site, dropping the `pushsym entry` and the call `JUMP`, and once every call site is
inlined, deletes the now-unreachable function definition. It uses one of two strategies per
function:

- **De-threaded tail-return (straight-line functions).** When the body is a single basic block
  ending in the return `JUMP`, the return address is eliminated completely: the `pushsym ret`, the
  body's return `JUMP`, and the stack shuffle that raised the address for the return are all dropped,
  the body falls through to the continuation, and every `DUP`/`SWAP` that reached past the (now
  removed) return-address slot is renumbered one shallower. This removes the **entire** call and
  return indirection (~15 gas/call) and is small enough that the bytecode usually shrinks.

- **De-threaded diamond (single-merge `if`/`else`).** venom lays an `if`/`else` out as a header that
  branches to one of two straight-line arms, both rejoining at a single merge block that returns via
  the threaded address — and on the branch arm it emits a wasteful *double jump* (jump to the merge,
  which immediately jumps again to return). The merge sits physically between the two arms, so the
  pass deletes it and joins both arms at a fresh fall-through label (`_sym_inl<copy>_<k>`) placed at
  the body end: the fall-through arm jumps there, the branch arm falls into it, and the merge's
  de-threaded tail (its address-raising `SWAP` dropped, any real ops kept) runs once at the join. No
  reordering is needed. This eliminates the return indirection **and** the branch arm's double jump
  (a return value, if any, sits below the address and falls through unchanged) — strictly beating the
  verbatim relocation on both gas and size.

- **Verbatim relocation (other branching bodies).** When the body is neither a tail-return block nor
  a single-merge diamond (a nested branch, a loop, multiple merges), its return cannot become a
  fall-through. The body is relocated **verbatim** — keeping the `pushsym ret` and the return `JUMP`
  — with its internal labels renamed uniquely (`_sym_inl<copy>_<k>`) so duplicating it across several
  call sites cannot collide. This still removes the 12-gas call indirection.

## When it fires

Only on a function venom did not already inline (a single-call-site function is inlined by venom
itself, so there is nothing left to do), whose body is **self-contained** and within the size
threshold. A two-call-site helper on a hot path is the canonical case:

```vyper
@internal
@pure
def _pick(x: uint256, y: uint256) -> uint256:   # two call sites → venom keeps it separate
    if x < y:
        return x
    return y

@external
@view
def a(x: uint256, y: uint256, n: uint256) -> uint256:
    s: uint256 = 0
    for i: uint256 in range(n, bound=128):
        s += self._pick(x, y)                    # call site 1 (hot loop)
    return s

@external
@view
def b(x: uint256, y: uint256) -> uint256:
    return self._pick(y, x)                       # call site 2
```

**Vyper only.** The pass keys on venom's `_sym_internal … _runtime` entry symbol and call
convention. solc is not handled, and not for lack of trying: under `--optimize` solc **already
inlines** its internal functions (a multi-call `internal` helper leaves no separate callable block —
its optimized runtime contains no function-call `pushsym`/`JUMP` convention at all, only revert
handlers), and its labels are undifferentiated `_sym_tag_N` shared by branches, loops, and calls, so
there is no reliable signal to key on. There is therefore nothing to inline on Solidity output and no
safe way to detect it — like [`cmpnorm`](../cmpnorm/README.md) (Vyper-only) and
[`foldshift`](../fold_shift/README.md) (Solidity-only), `inline` is a single-language pass.

A function is **left untouched** (skipped, never miscompiled) when any safety premise fails:
its body jumps outside itself, calls another internal function or recurses, carries a `_mem_`/`_OFST`
operand (whose symbol the sidecar dump does not preserve), has a malformed call site, or exceeds the
size threshold.

## Configurable threshold (the first numeric feature)

Inline is the first feature with a numeric parameter: only functions whose body is at most
`inline_max_body` instructions are inlined (default **20**), since duplicating a large body across
many call sites costs more bytecode than the saved indirection is worth. Set it with
`--inline-max-body N` or `inline_max_body = N` in a `--config` file.

## Savings

Every executed call loses its indirection; how the bytecode size moves depends on the strategy and
the call-site count. Measured on a real EVM (revm), Vyper 0.4.3 venom `OptimizationLevel.GAS`, e2e
[`e2e.rs`](e2e.rs); the result is always unchanged:

| Function | Strategy | Call gas | Creation bytes |
|---|---|---|---|
| `_hi` (`(x\|y) & 255`) | de-threaded tail-return | 22285 → 22210 (−75 = 15 × 5 iters) | 217 → 205 (−12, indirection gone) |
| `_pick` (`if x<y: return x; return y`) | de-threaded diamond | 22440 → 22255 (−185 = 37 × 5 iters) | 229 → 229 (unchanged) |
| `_advance_to` (void `if`/`else`, else path) | de-threaded diamond | 22888 → 22718 (−170 = 34 × 5 iters) | 236 → 254 (+18, second copy) |
| `_clamp` (nested `if`) | verbatim relocation | 22575 → 22515 (−60 = 12 × 5 iters) | 240 → 261 (+21, second copy) |

De-threading removes enough indirection that the savings dwarf the verbatim relocation (which only
drops the 12-gas call indirection); whether the bytecode grows depends on the body size and call-site
count, so a de-thread can shrink (`_hi`), break even (`_pick`), or — when the duplicated body is
large — still grow (`_advance_to`), a size-for-gas trade like [`foldshift`](../fold_shift/README.md).
Diamond savings are larger on the branch arm, where venom's double jump is removed.

## How it is sound

The body threads the return address through the stack and returns by `JUMP`ing to it.

**Verbatim relocation** keeps the `pushsym ret` and the body's return `JUMP` exactly as they were and
relocates the body with only its internal labels renamed — so the only change is dropping the
`pushsym entry` and the call `JUMP` that selected the function. No value the body consumes moves on
the stack (the return address still sits where the body expects it), so no stack renumbering is
needed and the relocation is a behavior-preserving identity.

**Tail-return de-threading** applies only to a single basic block ending in the return `JUMP`. The
return address enters on top of the stack, so the pass simulates the block tracking that address's
depth: it drops the `pushsym ret`, the return `JUMP`, and the shuffle that raised the address, and
renumbers each `DUP`/`SWAP` whose reach crossed the removed slot one shallower. Removing exactly one
stack slot that is never read except by the final return `JUMP` leaves every other value — and the
computed result — in place, so the de-threaded body computes the identical result and falls through to
the continuation the return would have jumped to.

**Diamond de-threading** runs the same per-segment simulation over each of the three straight-line
segments — the header, the two arms — requiring both arms to reach the merge at the same
return-address depth (which valid venom guarantees) and the merge tail to bring the address to the top
exactly as the return would. Because the merge block lies physically between the arms, deleting it and
appending the join at the body end makes the fall-through arm jump to the join and the branch arm fall
into it — preserving both control-flow edges with no reordering — and the merge's de-threaded tail
runs once at the join, so every path computes the identical result before falling through to the
continuation.

Both de-threads refuse (and fall back to verbatim) the moment the body strays from the proven shape —
an unexpected jump, a second branch, mismatched arm depths, or any op that would consume or duplicate
the return address.

The other enabled passes are run over each relocated body first, so an inlined copy is at least as
optimized as the shared original would have been — enabling inline never raises gas.

## Scope — runs first, length-changing, symbolic (relinkable) input only

Inline runs **first** in the pass pipeline so its definition-deletion and call-site spans take
precedence; a later pass cannot rewrite code inline relocates or deletes. Relocating and renaming a
body changes instruction lengths and emits fresh symbolic labels, so — like
[`shuffle`](../shuffle/README.md), [`involution`](../involution/README.md),
[`foldshift`](../fold_shift/README.md), and [`cmpnorm`](../cmpnorm/README.md) — the pass runs **only
on symbolic programs**, where the compiler's own assembler relinks via the sidecar
(`--emit-creation`). The relocated body is carried to the sidecar one instruction per edit token; the
sidecars stay dumb and just splice.
