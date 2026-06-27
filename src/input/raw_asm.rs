//! Frontend: raw EVM assembly (text).
//!
//! Accepts assembly text (mnemonics separated by spaces/newlines), including the
//! Vyper-venom-style symbolic form (`_sym_*`, `_OFST`, `_mem_`). This is the most
//! direct path: the `strip` engine detects revert guards precisely by the
//! symbolic labels `_sym_*revert*`. For the tool to strip anything on raw asm,
//! the target label of the conditional revert must contain `revert` in its name.

use crate::core::asm::{is_symbolic, parse_str};

use super::Loaded;

pub fn load(content: &str) -> Result<Loaded, String> {
    let instrs = parse_str(content);
    if instrs.is_empty() {
        return Err("empty assembly".into());
    }
    let symbolic = is_symbolic(&instrs);
    Ok(Loaded {
        instrs,
        symbolic,
        kind: "asm",
    })
}
