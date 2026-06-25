# Feature `abi` — strip ABI / calldata bounds checks

Reference feature for gasripper (the template others follow: module docs + unit
tests + a real-EVM e2e). Project-wide safety model: [main README](../../../README.md).

## What it strips

Compiler-inserted guards that validate **incoming calldata** before a function
body runs — calldata length, dynamic offset/length range, selector presence. Each
reads `CALLDATASIZE`/`CALLDATALOAD` and conditionally reverts. Canonical shape:

```text
CALLDATASIZE PUSH1 <min_len> GT  <revert> JUMPI   ; revert if calldata too short
```

Removed **only** when cutting it is a stack identity (reads inputs via `DUP`/`SWAP`
without consuming them — `core::stack::simulate_identity`). `CALLER`/`ORIGIN` and
side effects are never touched.

## Example: source that pays for unused validation

A contract called only by a **trusted caller** with known-correct calldata. The
ABI/calldata checks the compiler emits are then pure gas overhead on every call.

Vyper:

```vyper
@external
def foo(a: uint256, b: uint256) -> uint256:
    return a + b   # the compiler's calldata-length guard around the args — STRIPPED
```

Solidity:

```solidity
function foo(uint256 a, uint256 b) external pure returns (uint256) {
    return a + b;   // the compiler's `calldatasize >= 4` selector guard — STRIPPED
}
```

(Auth checks like `require(msg.sender == owner)` are never stripped — that is the
trusted-caller premise, covered by the [Safety](#safety) section, not this feature.)

## Safety

The deleted run is a stack identity, so the only behavioral change is "revert on
malformed calldata". Safe **only** under a trusted caller (private MEV executor,
owner's bot). On a publicly callable contract this removes input validation — do
not use it there.

## Measured (real EVM, revm — `e2e.rs`)

`foo(3, 4)` returns `7` before and after; stripping only `abi`:

| Source   | call gas            | creation bytecode |
|----------|---------------------|-------------------|
| Vyper    | 23631 → 23605 (−26) | 191 → 181 (−10)   |
| Solidity | 23842 → 23821 (−21) | 324 → 317 (−7)    |

**Solidity:** the solc sidecar normalizes both revert idioms (direct `<cond>
PUSH[revert_tag] JUMPI` and inverse `<cond> PUSH[continue_tag] JUMPI; <revert>`) to
`_sym_*revert*`, so the shared engine strips them unchanged. Here the `abi` guard is
the direct calldata-size check; per-argument bounds classify as `math` (offset
math via `SUB`).

## Files

| File | Purpose |
|---|---|
| `mod.rs` | `META`, `strip()`, pattern unit tests |
| `e2e.rs` | real-EVM gas + behavior proof (Vyper + Solidity), via `features::e2e_harness` |
