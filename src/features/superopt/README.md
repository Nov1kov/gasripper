# Feature `superopt` — SMT block superoptimization (opt-in, `smt` feature)

Project-wide safety model: [main README](../../../README.md).

> **Opt-in.** Pulls in the Z3 solver, so it is gated behind the `smt` Cargo feature and is absent from
> the default pure-`std` binary (Z3 is fetched as a prebuilt `libz3` only here, like `revm` in
> dev-builds). Build/test with `cargo build --features smt` / `cargo test --features smt`.

Because the solver can run for a while on a large contract, the pass logs progress via `tracing` at
the default `info` level — like [`shuffle`](../shuffle/README.md), a periodic line throttled to at
most once every 10 s plus a final summary (silent below 64 blocks). Silence it with `RUST_LOG=warn`.

## What it does

For a maximal **pure straight-line block** (only stack moves `PUSH`/`PUSH0`/`POP`/`DUPn`/`SWAPn` and
the interpreted ops `ADD SUB MUL DIV SDIV MOD SMOD ADDMOD MULMOD SIGNEXTEND AND OR XOR NOT ISZERO EQ
LT GT SLT SGT BYTE SHL SHR SAR`) it searches for a cheaper
instruction sequence and **proves** with Z3 that it leaves the identical final stack on every 256-bit
input. Not keyed on a fixed idiom: it *discovers* the rewrite by search-and-prove, so it catches
identities the compiler's own optimizer missed. The search stays fast by rejecting candidates in
cheap-to-expensive order: static gas (cheapest-first, so the proven incumbent prunes the costlier
rest), integer stack shape, then six concrete input vectors — a mismatch on a concrete input
disproves a candidate without a solver call, so Z3 only sees near-certain rewrites.

## Examples it actually optimizes (after the compiler)

The compilers already fold the easy cases (`x+0`, CSE, …). What survives — and `superopt` removes — is
redundancy the optimizer **can't prove away**: wrapping arithmetic identities and idempotent ops.

**Solidity** (solc 0.8.24 `--optimize`) — wrapping `((a+b)-b)^a == 0`:

```solidity
function f(uint256 a, uint256 b) external pure returns (uint256 r) {
    unchecked {
        uint256 s = a + b;
        uint256 t = s - b;   // == a
        uint256 u = t * 1;   // == a
        uint256 v = u + 0;   // == a
        r = v ^ a;           // == 0
        r = r + a;           // == a
    }
}
```

solc leaves the 8-op block `DUP2 DUP2 ADD SUB DUP2 XOR ADD SWAP1`; Z3 proves it equals `POP SWAP1`.
**Block gas 24 → 5 (−19).** The contract still returns `a`.

**Vyper** (venom 0.4.3) — idempotent `(a & b) & (a & b) == a & b`:

```vyper
@external
@view
def f(a: uint256, b: uint256) -> uint256:
    return (a & b) & (a & b)
```

venom leaves the self-`AND` as `AND DUP1 AND`; Z3 proves it is just `a & b`. **Block gas 17 → 10 (−7)**
across the blocks rewritten. The contract still returns `a & b`.

The newer interpreted opcodes fire the same way (`e2e.rs::*_new_interpreted_ops_superoptimized`):
on solc, the wrapping `((a+b)-b) < a` (always false), `mulmod(a+b, a-b, 1)` (anything mod 1 is 0)
and the idempotent `(a >> 255) >> 255` bodies collapse to `POP POP PUSH0 SWAP1` /
`DUP1 SAR SWAP1`-style rewrites around the threaded return address. On venom, `uint256_mulmod(m,
m, m)` (always 0 — needs the EVM mod-0 special case), the doubled `SAR` and the wrapping `SLT`
collapse likewise. Each e2e also runs a `bench` hot loop whose whole body is a mulmod-by-one
identity, so the saving clears the EIP-7623 floor and is visible at the **transaction** level:
**−5800 tx gas** on solc, **−4000** on venom (200 iterations). Literal pushes reach the engine as
solc's bare `PUSH` mnemonic and are priced/folded like any sized push; a symbolic `PUSH [tag]`
ends the pure run (its value is link-time).

A third, compiler-free proof ([`e2e.rs`](e2e.rs)) is fully deterministic on revm: a hand-assembled
`x + 0 + 0 + 0` block collapses to one `PUSH0` for a measured **−19 gas** at the transaction level
(empty calldata, so no EIP-7623 floor to mask it).

> The two real-code proofs assert the **block**-gas drop plus unchanged behavior on revm, not a
> tx-gas drop: these single-shot wins sit under the EIP-7623 calldata floor, which clamps the tx total
> even though the executed block is provably cheaper. On already-optimized output surviving redundancy
> is small and often cold, so a reliable tx-level win is not available — as the project's prior
> experiments predicted.

## How it is sound

Only side-effect-free, control-flow-free, fully concrete opcodes are eligible, so a block-local
replacement is valid in any surrounding program (ebso's replacement lemma). The interpreted opcodes
map exactly onto EVM mod-2^256 semantics, and a rewrite is emitted **only on a Z3 `unsat` proof** of
non-equivalence — a timeout or anything unproven leaves the block untouched (wrong bytecode in a gas
tool is dangerous).

## Bounds & scope

All four search limits are user-tunable — CLI flags `--superopt-max-block`, `--superopt-max-synth`,
`--superopt-timeout-ms`, `--superopt-max-checks`, or the same `superopt_*` keys in a `--config`
file. The values below are the defaults (and what the e2e gas pins are proven against); raising
them trades scan time for search power.

- Runs > **24** instructions are skipped; synthesized candidates are ≤ **4** instructions (a
  same-length candidate still counts when cheaper, e.g. `PUSH1 0` → `PUSH0`; four covers the smallest
  solc-shaped rewrite, `POP POP PUSH0 SWAP1` around the threaded return address); each proof has a
  **500 ms** timeout and a block spends at most **128** solver checks (an exhausted budget leaves the
  block unoptimized — refuting wrong candidates over nonlinear 512-bit terms is what runs long, not
  the proofs themselves).
- The EVM special cases are modeled exactly: division/mod by zero → 0, `ADDMOD`/`MULMOD` reduce the
  full 512-bit intermediate, `BYTE` past index 31 → 0, `SIGNEXTEND` past byte 30 is the identity.
- Excluded ops: `EXP` (no closed bit-vector form, dynamic gas) and anything with a side effect,
  memory/storage access, or control flow.
- **Symbolic input only** in the pipeline: a cheaper block has a different length and shifts later
  `JUMPDEST` offsets, so — like [`shuffle`](../shuffle/README.md)/[`cmpnorm`](../cmpnorm/README.md) —
  it runs only where the compiler relinks; an overlapping earlier span wins.
