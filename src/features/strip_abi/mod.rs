//! Feature `abi` — strip ABI / calldata bounds checks.
//!
//! # What it optimizes
//!
//! Solidity and Vyper insert defensive checks that validate **incoming calldata**
//! before a function body runs: that the calldata is long enough for the declared
//! arguments, that a dynamic offset/length is in range, etc. Each such check reads
//! calldata (`CALLDATASIZE` / `CALLDATALOAD`) and conditionally jumps to a revert.
//!
//! This feature removes those guards. The canonical shape (Vyper venom) is a
//! calldata-size validation at the start of an external function:
//!
//! ```text
//! CALLDATASIZE PUSH1 <min_len> GT  _sym_<revert> JUMPI   ; revert if calldata too short
//! ```
//!
//! It is removed **only** when cutting it is a stack identity (the run reads its
//! inputs via DUP/SWAP without consuming them, so the surrounding code is
//! unaffected — see [`crate::core::stack::simulate_identity`]). Authorization
//! (`CALLER`/`ORIGIN`) and side effects are never touched.
//!
//! # Safety
//!
//! Safe **only** under a trusted caller that always supplies well-formed calldata
//! (e.g. a private MEV executor driven solely by the owner's bot). For a publicly
//! callable contract, removing input validation introduces vulnerabilities.
//!
//! # Measured effect
//!
//! On a reference `foo(uint256, uint256)` guarded by a calldata-size check,
//! stripping just the `abi` category and verifying on a real EVM (revm) — same
//! pipeline for both languages (compile → strip RUNTIME → re-assemble creation
//! bytecode → deploy → call), see [`e2e`]:
//!
//! | source   | call gas         | creation bytecode | behavior              |
//! |----------|------------------|-------------------|-----------------------|
//! | Vyper    | 23631 → 23605 (−26) | 191 → 181 (−10) | `foo(3,4) == 7` kept  |
//! | Solidity | 23842 → 23821 (−21) | 324 → 317 (−7)  | `foo(3,4) == 7` kept  |
//!
//! (Enabling every category shrinks the Vyper contract further to 176 bytes.)
//!
//! On Solidity the solc sidecar normalizes both revert idioms into `_sym_*revert*`,
//! so this shared engine strips them unchanged; here the `abi` guard is the direct
//! calldata-size check (`<cond> PUSH[revert_tag] JUMPI`). Per-argument calldata
//! bounds happen to classify as `math` (they compute offsets via `SUB`).

use std::collections::HashSet;

use super::FeatureMeta;
use crate::core::{Category, Instr, Span, strip_guards};

#[cfg(test)]
mod e2e;

pub const META: FeatureMeta = FeatureMeta {
    key: "abi",
    name: "ABI/calldata bounds",
    description: "strip ABI/calldata bounds checks (length/offset validation)",
    category: Category::Abi,
    default_enabled: true,
};

/// Strip only the abi category (in isolation — for tests/targeted runs).
#[allow(dead_code)] // feature's module API; the CLI strips all enabled categories at once
pub fn strip(instrs: &[Instr]) -> (Vec<Instr>, Vec<Span>) {
    let only: HashSet<Category> = [Category::Abi].into_iter().collect();
    strip_guards(instrs, &only)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{mnemonics, parse_str};

    const REV: &str = "_sym___revert";

    #[test]
    fn abi_bounds_check_removed() {
        // ABI calldata-length validation -> everything is removed and classified as abi.
        let p = parse_str(&format!("DUP1 CALLDATALOAD PUSH1 32 LT {REV} JUMPI"));
        let (out, spans) = strip(&p);
        assert!(mnemonics(&out).is_empty(), "ABI check not stripped entirely: {:?}", mnemonics(&out));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].category, Category::Abi);
    }

    #[test]
    fn live_code_after_check_kept() {
        // Live code after the check stays untouched.
        let p = parse_str(&format!("CALLDATASIZE PUSH1 4 GT {REV} JUMPI PUSH1 0 CALLDATALOAD"));
        let (out, spans) = strip(&p);
        assert_eq!(mnemonics(&out), vec!["PUSH1", "CALLDATALOAD"]);
        assert_eq!(spans.len(), 1, "exactly one check should be stripped");
    }

    #[test]
    fn math_check_not_touched_by_abi_feature() {
        let p = parse_str(&format!("DUP1 PUSH1 1 ADD PUSH1 100 LT {REV} JUMPI"));
        let (_out, spans) = strip(&p);
        assert!(spans.is_empty(), "the abi feature must not strip a pure math check");
    }
}
