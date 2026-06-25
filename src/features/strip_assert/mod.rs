//! Feature `assert` — strip range / cast / other assert guards.
//!
//! # What it optimizes
//!
//! The **fallback** category: provably-safe revert guards that are neither `abi`
//! (no `CALLDATALOAD`/`CALLDATASIZE`) nor `math` (no `ADD`/`SUB`/`MUL`/`DIV`/`MOD`/
//! `EXP`/`SHL`). Typically range / cast validations — e.g. a downcast
//! `convert(x, uint128)` that checks `x >> 128 == 0` via `SHR`/`ISZERO`, reading
//! `x` through `DUP` without consuming it.
//!
//! Canonical shape (Vyper venom):
//!
//! ```text
//! DUP1 PUSH1 128 SHR ISZERO  <revert> JUMPI    ; revert if the value doesn't fit
//! ```
//!
//! Removed **only** when cutting it is a stack identity ([`crate::core::stack::simulate_identity`]).
//! Checks that CONSUME their input (not an identity) are preserved — they may be
//! meaningful profit/state guards. Authorization (`CALLER`/`ORIGIN`) and side
//! effects are never touched.
//!
//! # Safety
//!
//! Same model as every strip feature: safe only under a trusted caller. The
//! removed run is a stack identity, so the only behavioral change is "revert on an
//! out-of-range input that the trusted caller never sends".
//!
//! # Measured effect (real EVM, revm — see [`e2e`])
//!
//! On a `foo(uint256) -> uint128` whose `convert`/cast range-check is dead weight,
//! stripping just the `assert` category; `foo(3)` returns `3` before and after:
//!
//! | source   | call gas            | creation bytecode | notes |
//! |----------|---------------------|-------------------|-------|
//! | Vyper    | 23479 → 23445 (−34) | 187 → 168 (−19)   | `convert` range-check + consuming guards stripped |
//! | Solidity | 23617 → 23576 (−41) | 283 → 264 (−19)   | `require(a < 256)` (inverse idiom) stripped |
//!
//! Solidity's range/cast guards use the *inverse* revert idiom (`<cond>
//! PUSH[continue_tag] JUMPI; <inline revert>`); the solc sidecar normalizes it
//! (`_sym_revert_inv_*`) so this shared engine strips it unchanged.

use std::collections::HashSet;

use super::FeatureMeta;
use crate::core::{Category, Instr, Span, strip_guards};

#[cfg(test)]
mod e2e;

pub const META: FeatureMeta = FeatureMeta {
    key: "assert",
    name: "Range/cast asserts",
    description: "strip other range/cast assert checks (neither abi nor math)",
    category: Category::Assert,
    default_enabled: true,
};

/// Strip only the assert category (in isolation — for tests/targeted runs).
#[allow(dead_code)] // feature's module API; the CLI strips all enabled categories at once
pub fn strip(instrs: &[Instr]) -> (Vec<Instr>, Vec<Span>) {
    let only: HashSet<Category> = [Category::Assert].into_iter().collect();
    strip_guards(instrs, &only)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{mnemonics, parse_str};

    const REV: &str = "_sym___revert";

    #[test]
    fn range_assert_removed() {
        // A pure check via DUP, no calldata/arithmetic -> assert category, stripped.
        let p = parse_str(&format!("DUP1 PUSH1 128 SHR ISZERO {REV} JUMPI"));
        let (out, spans) = strip(&p);
        assert!(mnemonics(&out).is_empty(), "assert check not stripped: {:?}", mnemonics(&out));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].category, Category::Assert);
    }

    #[test]
    fn consuming_assert_stripped_via_residue() {
        // A consuming check (drops its input) is removed by replacing it with the
        // equivalent stack residue — here a single POP. Aggressive but stack-safe.
        let p = parse_str(&format!("PUSH1 5 GT {REV} JUMPI"));
        let (out, spans) = strip(&p);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].category, Category::Assert);
        assert_eq!(mnemonics(&out), vec!["POP"]);
    }

    #[test]
    fn auth_check_preserved() {
        // CALLER is auth — never stripped, even though its run would classify as assert.
        let p = parse_str(&format!("CALLER PUSH20 0xABCD XOR {REV} JUMPI"));
        let (_out, spans) = strip(&p);
        assert!(spans.is_empty(), "auth (CALLER) check wrongly stripped by assert feature");
    }

    #[test]
    fn math_check_not_touched_by_assert_feature() {
        let p = parse_str(&format!("DUP1 PUSH1 1 ADD PUSH1 100 LT {REV} JUMPI"));
        let (_out, spans) = strip(&p);
        assert!(spans.is_empty(), "the assert feature must not strip a math check");
    }

    #[test]
    fn abi_check_not_touched_by_assert_feature() {
        let p = parse_str(&format!("DUP1 CALLDATALOAD PUSH1 32 LT {REV} JUMPI"));
        let (_out, spans) = strip(&p);
        assert!(spans.is_empty(), "the assert feature must not strip an abi check");
    }
}
