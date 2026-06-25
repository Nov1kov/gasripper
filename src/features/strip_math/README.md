# Feature `math` — strip overflow / underflow / arithmetic guards

Concise reference. Project-wide safety model: [main README](../../../README.md).
Feature template: [`strip_abi`](../strip_abi/README.md).

## What it strips

Provably-safe checks whose run contains arithmetic — `ADD`/`SUB`/`MUL`/`DIV`/`MOD`/
`EXP`/`SHL` — plus a conditional revert. Canonical shape (a bound checked on a value
read via `DUP`, so the value survives):

```text
DUP1 PUSH1 1 ADD PUSH1 100 LT  <revert> JUMPI    ; revert if x+1 >= 100
```

Removed **only** when cutting it is a stack identity (reads its inputs via
`DUP`/`SWAP` without consuming them — `core::stack::simulate_identity`).
`CALLER`/`ORIGIN` and side effects are never touched.

## Overflow assertions land in `assert`, not `math`

An `a + b` overflow check consumes a spare operand. The engine still removes it (by
reproducing its stack residue — see the [Safety model](../../../README.md#safety-model)),
but it KEEPS the `ADD` (live logic) and cuts only the assertion `SWAP1 DUP2 LT revert
JUMPI`. That removed run has no arithmetic opcode, so it classifies as `assert`. The
`math` category therefore fires only when an arithmetic opcode is inside the removed
run — e.g. Solidity's ABI decoder validating calldata length with `SUB`/`SLT`.

## Example

The `a + b` body compiles the same in both, but what the `math` category strips differs:

Vyper — the overflow assertion is removed under `assert` (the `ADD` is kept), so
`math`-only is a no-op:

```vyper
@external
def foo(a: uint256, b: uint256) -> uint256:
    return a + b   # ADD kept; overflow assertion removed under `assert`, not `math`
```

Solidity — the ABI decoder's calldata-length check (`SUB`/`SLT`) is in the removed
run → **stripped by `math`**:

```solidity
function foo(uint256 a, uint256 b) external pure returns (uint256) {
    return a + b;   // calldata-length (SUB SLT) decode guard — STRIPPED
}
```

(Auth checks like `require(msg.sender == owner)` are never stripped — the
trusted-caller premise, see the [Safety](#safety) section.)

## Safety

The removed run's live-stack residue is reproduced exactly, so the only behavioral
change is "no longer revert on an input that would overflow / fall out of range".
Safe only under a trusted caller.

## Measured (real EVM, revm — `e2e.rs`)

`foo(3, 4)` returns `7` before and after; stripping only `math`:

| Source   | call gas            | creation bytecode | notes |
|----------|---------------------|-------------------|-------|
| Solidity | 23842 → 23811 (−31) | 324 → 311 (−13)   | calldata-length `SUB SLT` check stripped |
| Vyper    | no-op               | no-op             | `a + b` overflow assertion is `assert` category (stripped with `assert`/all) |

Vyper's gas-optimized venom does not expose math-identity guards for arithmetic;
the e2e pins this as a safety property (the tool does not touch consuming checks).

## Files

| File | Purpose |
|---|---|
| `mod.rs` | `META`, `strip()`, pattern unit tests |
| `e2e.rs` | real-EVM proof (Solidity win; Vyper no-op pin), via `features::e2e_harness` |
