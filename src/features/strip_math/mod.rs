//! Feature `math` — strip overflow / underflow / arithmetic revert guards.
//!
//! # What it optimizes
//!
//! Provably-safe checks whose run contains arithmetic — `ADD`/`SUB`/`MUL`/`DIV`/
//! `MOD`/`EXP`/`SHL` — plus a conditional revert. Canonical shape (a bound checked
//! on a value read via `DUP`, so the value survives):
//!
//! ```text
//! DUP1 PUSH1 1 ADD PUSH1 100 LT  <revert> JUMPI    ; revert if x+1 >= 100
//! ```
//!
//! Removed **only** when cutting it is a stack identity
//! ([`crate::core::stack::simulate_identity`]). Authorization (`CALLER`/`ORIGIN`)
//! and side effects are never touched.
//!
//! # Overflow assertions are category `assert`, not `math`
//!
//! A real `a + b` overflow check consumes a spare operand — the engine now removes
//! it by reproducing its stack residue (see `core::stack::strip_residue`), but it
//! KEEPS the `ADD` (live logic) and cuts only the assertion `SWAP1 DUP2 LT revert
//! JUMPI`. That removed run carries no arithmetic opcode, so it classifies as
//! `assert`. This `math` feature therefore fires only when an arithmetic opcode is
//! itself inside the removed run — e.g. Solidity's ABI decoder validating calldata
//! length with a `SUB`/`SLT` comparison.
//!
//! # Safety
//!
//! Same model as every strip feature: safe only under a trusted caller. The removed
//! run is a stack identity, so the only behavioral change is "revert on an input
//! that would overflow / fall out of range".
//!
//! # Measured effect (real EVM, revm — see [`e2e`])
//!
//! `foo(3, 4)` returns `7` before and after; stripping just the `math` category:
//!
//! | source   | call gas            | creation bytecode | notes |
//! |----------|---------------------|-------------------|-------|
//! | Solidity | 23842 → 23811 (−31) | 324 → 311 (−13)   | calldata-length (`SUB SLT`) check stripped |
//! | Vyper    | no-op               | no-op             | `a + b` overflow assertion is `assert` category, not `math` |
//!
//! The Vyper `a + b` overflow check IS removed by the engine, but under the `assert`
//! category (the kept `ADD` keeps it out of the removed run); `math`-only is a no-op
//! here. With every category enabled it is stripped.

use std::collections::HashSet;

use super::FeatureMeta;
use crate::core::{Category, Instr, Span, strip_guards};

#[cfg(test)]
mod e2e;

pub const META: FeatureMeta = FeatureMeta {
    key: "math",
    name: "Math/overflow guards",
    description: "strip overflow/underflow and arithmetic revert checks",
    category: Category::Math,
    default_enabled: true,
};

/// Strip only the math category (in isolation — for tests/targeted runs).
#[allow(dead_code)] // feature's module API; the CLI strips all enabled categories at once
pub fn strip(instrs: &[Instr]) -> (Vec<Instr>, Vec<Span>) {
    let only: HashSet<Category> = [Category::Math].into_iter().collect();
    strip_guards(instrs, &only)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{mnemonics, parse_str};

    const REV: &str = "_sym___revert";

    #[test]
    fn overflow_check_removed() {
        // math guard: reads x via DUP, computes x+1<100, reverts; x remains -> stripped.
        let p = parse_str(&format!("DUP1 PUSH1 1 ADD PUSH1 100 LT {REV} JUMPI"));
        let (out, spans) = strip(&p);
        assert!(mnemonics(&out).is_empty(), "overflow check not stripped: {:?}", mnemonics(&out));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].category, Category::Math);
    }

    #[test]
    fn consuming_math_check_stripped_via_residue() {
        // A consuming arithmetic check is removed by reproducing its stack residue
        // (here a POP) instead of deleting it outright. Aggressive but stack-safe.
        let p = parse_str(&format!("PUSH1 1 ADD PUSH1 100 LT {REV} JUMPI"));
        let (out, spans) = strip(&p);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].category, Category::Math);
        assert_eq!(mnemonics(&out), vec!["POP"]);
    }

    #[test]
    fn auth_check_preserved() {
        // Arithmetic around CALLER is auth — never stripped.
        let p = parse_str(&format!("CALLER PUSH1 1 ADD ISZERO {REV} JUMPI"));
        let (_out, spans) = strip(&p);
        assert!(spans.is_empty(), "auth (CALLER) check wrongly stripped by math feature");
    }

    #[test]
    fn abi_check_not_touched_by_math_feature() {
        // An ABI check (CALLDATALOAD) is not math -> the feature leaves it alone.
        let p = parse_str(&format!("DUP1 CALLDATALOAD PUSH1 32 LT {REV} JUMPI"));
        let (out, spans) = strip(&p);
        assert!(spans.is_empty(), "the math feature must not strip an abi check");
        assert_eq!(out.len(), p.len());
    }

    #[test]
    fn assert_check_not_touched_by_math_feature() {
        // A pure range check (SHR, no arithmetic from the math set) classifies as assert.
        let p = parse_str(&format!("DUP1 PUSH1 128 SHR ISZERO {REV} JUMPI"));
        let (_out, spans) = strip(&p);
        assert!(spans.is_empty(), "the math feature must not strip an assert check");
    }
}
