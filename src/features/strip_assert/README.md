# Feature `assert` — strip range / cast / other assert guards

Concise reference. Project-wide safety model: [main README](../../../README.md).
Feature template: [`strip_abi`](../strip_abi/README.md).

## What it strips

The **fallback** category: provably-safe revert guards that are neither `abi`
(no `CALLDATALOAD`/`CALLDATASIZE`) nor `math` (no `ADD`/`SUB`/`MUL`/`DIV`/`MOD`/
`EXP`/`SHL`). Typically range / cast validations. Canonical shape:

```text
DUP1 PUSH1 128 SHR ISZERO  <revert> JUMPI    ; revert if value doesn't fit a downcast
```

Removed **only** when cutting it is a stack identity (reads its input via
`DUP`/`SWAP` without consuming it — `core::stack::simulate_identity`). Checks that
CONSUME their input are preserved (possible profit/state guards), as are
`CALLER`/`ORIGIN` and side effects.

## Example: source that pays for an unused range check

A contract called only by a **trusted caller** with in-range inputs. The downcast
bound check the compiler emits is then dead weight on every call.

Vyper:

```vyper
@external
def foo(a: uint256) -> uint128:
    return convert(a, uint128)   # range check (a >> 128 == 0) — STRIPPED
```

Solidity:

```solidity
function foo(uint256 a) external pure returns (uint256) {
    require(a < 256);   // range check (inverse idiom) — STRIPPED
    return a;
}
```

(Auth checks like `require(msg.sender == owner)` are never stripped — the
trusted-caller premise, see the [Safety](#safety) section.)

## Safety

The deleted run is a stack identity, so the only behavioral change is "revert on an
out-of-range input the trusted caller never sends". Safe only under a trusted
caller; do not use on a publicly callable contract.

## Measured (real EVM, revm — `e2e.rs`)

`foo(3)` returns `3` before and after; stripping only `assert`:

| Source   | call gas            | creation bytecode | notes |
|----------|---------------------|-------------------|-------|
| Vyper    | 23479 → 23445 (−34) | 187 → 168 (−19)   | `convert` range-check + consuming guards stripped |
| Solidity | 23617 → 23576 (−41) | 283 → 264 (−19)   | `require(a < 256)` (inverse idiom) stripped |

Solidity's range/cast guards compile to the *inverse* revert idiom (`<cond>
PUSH[continue_tag] JUMPI; <inline revert>`); the solc sidecar normalizes it
(`_sym_revert_inv_*`) so the shared engine strips it with no engine change.

## Files

| File | Purpose |
|---|---|
| `mod.rs` | `META`, `strip()`, pattern unit tests |
| `e2e.rs` | real-EVM proof (Vyper win; Solidity no-op pin), via `features::e2e_harness` |
