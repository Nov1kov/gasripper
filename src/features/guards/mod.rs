//! Feature `guards` — strip provably-safe revert guards.
//!
//! # What it optimizes
//!
//! Every redundant revert guard a compiler inserts before a function body runs:
//! overflow/underflow assertions (`a + b`, `a - b`, `a * b`), division-by-zero
//! checks (`a // b`), ABI/calldata bounds (length/offset validation), and range/cast
//! asserts (`convert(x, uintN)`, `require(x < N)`). Each is a `<cond> _sym_*revert*
//! JUMPI` whose fall-through path is provably unchanged when the revert is removed.
//!
//! # One feature, not three
//!
//! Earlier versions split this into `abi` / `math` / `assert` by sniffing which
//! opcodes sat in the removed run. That label was fragile: the SAME calldata bounds
//! check landed in `abi` on one compiler and `math` on another (e.g. solc keeps
//! `CALLDATASIZE` live and the removed run carries only `SUB`/`SLT`), so `--disable
//! abi` never reliably "kept calldata checks". The classes were one mechanism, so
//! they are now one feature.
//!
//! # Safety
//!
//! Safe **only** under a trusted caller that always supplies well-formed calldata and
//! in-range inputs. The engine removes a guard only when its fall-through stack is
//! reproduced exactly (a stack identity, or a minimal `POP`/`SWAP` residue), and
//! never touches authorization (`CALLER`/`ORIGIN`) or side effects. See the
//! [project README](../../../README.md) safety model.

use std::collections::HashSet;

use super::FeatureMeta;
use crate::core::{Category, Instr, Span, strip_guards};

#[cfg(test)]
mod e2e;

pub const META: FeatureMeta = FeatureMeta {
    key: "guards",
    name: "Revert guards",
    description: "strip provably-safe revert guards (overflow, calldata bounds, range/cast)",
    category: Category::Guard,
    default_enabled: true,
};

/// Strip the guard category (the only one — for tests/targeted runs).
#[allow(dead_code)] // feature's module API; the CLI strips via the enabled config
pub fn strip(instrs: &[Instr]) -> (Vec<Instr>, Vec<Span>) {
    let only: HashSet<Category> = [Category::Guard].into_iter().collect();
    strip_guards(instrs, &only)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{mnemonics, parse_str};

    const REV: &str = "_sym___revert";

    #[test]
    fn overflow_check_removed() {
        // A bound read via DUP (x+1<100), reverting; x survives -> pure identity strip.
        let p = parse_str(&format!("DUP1 PUSH1 1 ADD PUSH1 100 LT {REV} JUMPI"));
        let (out, spans) = strip(&p);
        assert!(mnemonics(&out).is_empty(), "overflow check not stripped: {:?}", mnemonics(&out));
        assert_eq!(spans.len(), 1, "exactly one guard should be stripped");
    }

    #[test]
    fn abi_bounds_check_removed() {
        // A calldata-length validation read via DUP is removed entirely.
        let p = parse_str(&format!("DUP1 CALLDATALOAD PUSH1 32 LT {REV} JUMPI"));
        let (out, spans) = strip(&p);
        assert!(mnemonics(&out).is_empty(), "ABI check not stripped: {:?}", mnemonics(&out));
        assert_eq!(spans.len(), 1, "exactly one guard should be stripped");
    }

    #[test]
    fn range_assert_removed() {
        // A pure range/cast check via DUP (value >> 128 == 0) is removed.
        let p = parse_str(&format!("DUP1 PUSH1 128 SHR ISZERO {REV} JUMPI"));
        let (out, spans) = strip(&p);
        assert!(mnemonics(&out).is_empty(), "range assert not stripped: {:?}", mnemonics(&out));
        assert_eq!(spans.len(), 1, "exactly one guard should be stripped");
    }

    #[test]
    fn consuming_check_stripped_via_residue() {
        // A check that consumes its input is removed by reproducing its residue (a POP).
        let p = parse_str(&format!("PUSH1 5 GT {REV} JUMPI"));
        let (out, spans) = strip(&p);
        assert_eq!(spans.len(), 1, "the consuming check should be stripped");
        assert_eq!(mnemonics(&out), vec!["POP"], "the residue strip must leave the equivalent POP");
    }

    #[test]
    fn live_code_after_check_kept() {
        // Live code after the guard stays untouched.
        let p = parse_str(&format!("CALLDATASIZE PUSH1 4 GT {REV} JUMPI PUSH1 0 CALLDATALOAD"));
        let (out, spans) = strip(&p);
        assert_eq!(mnemonics(&out), vec!["PUSH1", "CALLDATALOAD"], "live code after the guard was altered");
        assert_eq!(spans.len(), 1, "exactly one guard should be stripped");
    }

    #[test]
    fn auth_check_preserved() {
        // CALLER (msg.sender == owner) is auth — never stripped.
        let p = parse_str(&format!("CALLER PUSH20 0xABCD XOR {REV} JUMPI"));
        let (_out, spans) = strip(&p);
        assert!(spans.is_empty(), "auth (CALLER) check wrongly stripped");
    }
}
