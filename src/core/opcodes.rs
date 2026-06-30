//! EVM opcode table: mnemonic <-> byte, and arity (pops, pushes).
//!
//! Used in three ways:
//!   * `arity()`  — for stack simulation (the identity criterion in `stack.rs`);
//!   * `op_byte()` — for assembling assembly back into bytecode (`bytecode.rs`);
//!   * `name_for_byte()` — for disassembling raw bytecode.
//!
//! DUP/SWAP/PUSH are generated programmatically. DUP/SWAP are handled separately
//! in `stack.rs` (they reorder/read the stack rather than pop/push by arity), so
//! their (pops, pushes) here are nominal and used only for round-tripping.

use std::collections::HashMap;
use std::sync::OnceLock;

/// (mnemonic, byte, pops, pushes) — base opcodes without PUSH/DUP/SWAP.
const BASE: &[(&str, u8, usize, usize)] = &[
    ("STOP", 0x00, 0, 0),
    ("ADD", 0x01, 2, 1),
    ("MUL", 0x02, 2, 1),
    ("SUB", 0x03, 2, 1),
    ("DIV", 0x04, 2, 1),
    ("SDIV", 0x05, 2, 1),
    ("MOD", 0x06, 2, 1),
    ("SMOD", 0x07, 2, 1),
    ("ADDMOD", 0x08, 3, 1),
    ("MULMOD", 0x09, 3, 1),
    ("EXP", 0x0a, 2, 1),
    ("SIGNEXTEND", 0x0b, 2, 1),
    ("LT", 0x10, 2, 1),
    ("GT", 0x11, 2, 1),
    ("SLT", 0x12, 2, 1),
    ("SGT", 0x13, 2, 1),
    ("EQ", 0x14, 2, 1),
    ("ISZERO", 0x15, 1, 1),
    ("AND", 0x16, 2, 1),
    ("OR", 0x17, 2, 1),
    ("XOR", 0x18, 2, 1),
    ("NOT", 0x19, 1, 1),
    ("BYTE", 0x1a, 2, 1),
    ("SHL", 0x1b, 2, 1),
    ("SHR", 0x1c, 2, 1),
    ("SAR", 0x1d, 2, 1),
    ("KECCAK256", 0x20, 2, 1),
    ("ADDRESS", 0x30, 0, 1),
    ("BALANCE", 0x31, 1, 1),
    ("ORIGIN", 0x32, 0, 1),
    ("CALLER", 0x33, 0, 1),
    ("CALLVALUE", 0x34, 0, 1),
    ("CALLDATALOAD", 0x35, 1, 1),
    ("CALLDATASIZE", 0x36, 0, 1),
    ("CALLDATACOPY", 0x37, 3, 0),
    ("CODESIZE", 0x38, 0, 1),
    ("CODECOPY", 0x39, 3, 0),
    ("GASPRICE", 0x3a, 0, 1),
    ("EXTCODESIZE", 0x3b, 1, 1),
    ("EXTCODECOPY", 0x3c, 4, 0),
    ("RETURNDATASIZE", 0x3d, 0, 1),
    ("RETURNDATACOPY", 0x3e, 3, 0),
    ("EXTCODEHASH", 0x3f, 1, 1),
    ("BLOCKHASH", 0x40, 1, 1),
    ("COINBASE", 0x41, 0, 1),
    ("TIMESTAMP", 0x42, 0, 1),
    ("NUMBER", 0x43, 0, 1),
    ("PREVRANDAO", 0x44, 0, 1),
    ("GASLIMIT", 0x45, 0, 1),
    ("CHAINID", 0x46, 0, 1),
    ("SELFBALANCE", 0x47, 0, 1),
    ("BASEFEE", 0x48, 0, 1),
    ("BLOBHASH", 0x49, 1, 1),
    ("BLOBBASEFEE", 0x4a, 0, 1),
    ("POP", 0x50, 1, 0),
    ("MLOAD", 0x51, 1, 1),
    ("MSTORE", 0x52, 2, 0),
    ("MSTORE8", 0x53, 2, 0),
    ("SLOAD", 0x54, 1, 1),
    ("SSTORE", 0x55, 2, 0),
    ("JUMP", 0x56, 1, 0),
    ("JUMPI", 0x57, 2, 0),
    ("PC", 0x58, 0, 1),
    ("MSIZE", 0x59, 0, 1),
    ("GAS", 0x5a, 0, 1),
    ("JUMPDEST", 0x5b, 0, 0),
    ("TLOAD", 0x5c, 1, 1),
    ("TSTORE", 0x5d, 2, 0),
    ("MCOPY", 0x5e, 3, 0),
    ("PUSH0", 0x5f, 0, 1),
    ("LOG0", 0xa0, 2, 0),
    ("LOG1", 0xa1, 3, 0),
    ("LOG2", 0xa2, 4, 0),
    ("LOG3", 0xa3, 5, 0),
    ("LOG4", 0xa4, 6, 0),
    ("CREATE", 0xf0, 3, 1),
    ("CALL", 0xf1, 7, 1),
    ("CALLCODE", 0xf2, 7, 1),
    ("RETURN", 0xf3, 2, 0),
    ("DELEGATECALL", 0xf4, 6, 1),
    ("CREATE2", 0xf5, 4, 1),
    ("STATICCALL", 0xfa, 6, 1),
    ("REVERT", 0xfd, 2, 0),
    ("INVALID", 0xfe, 0, 0),
    ("SELFDESTRUCT", 0xff, 1, 0),
];

struct Tables {
    /// mnemonic -> (byte, pops, pushes)
    by_name: HashMap<String, (u8, usize, usize)>,
    /// byte -> mnemonic
    by_byte: HashMap<u8, String>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut by_name = HashMap::new();
        let mut by_byte = HashMap::new();
        let put = |name: String,
                   b: u8,
                   p: usize,
                   s: usize,
                   by_name: &mut HashMap<_, _>,
                   by_byte: &mut HashMap<_, _>| {
            by_name.insert(name.clone(), (b, p, s));
            by_byte.insert(b, name);
        };
        for &(name, b, p, s) in BASE {
            put(name.to_string(), b, p, s, &mut by_name, &mut by_byte);
        }
        // PUSH1..PUSH32 (0x60..0x7f): pops 0, pushes 1, n-byte immediate.
        for n in 1..=32u8 {
            put(
                format!("PUSH{n}"),
                0x5f + n,
                0,
                1,
                &mut by_name,
                &mut by_byte,
            );
        }
        // DUP1..DUP16 (0x80..0x8f) — nominal arity, see stack.rs.
        for n in 1..=16u8 {
            put(
                format!("DUP{n}"),
                0x7f + n,
                0,
                1,
                &mut by_name,
                &mut by_byte,
            );
        }
        // SWAP1..SWAP16 (0x90..0x9f) — nominal arity, see stack.rs.
        for n in 1..=16u8 {
            put(
                format!("SWAP{n}"),
                0x8f + n,
                0,
                0,
                &mut by_name,
                &mut by_byte,
            );
        }
        Tables { by_name, by_byte }
    })
}

/// (pops, pushes) for a mnemonic, or None for an unknown opcode.
pub fn arity(name: &str) -> Option<(usize, usize)> {
    tables().by_name.get(name).map(|&(_, p, s)| (p, s))
}

/// The opcode byte for a mnemonic.
pub fn op_byte(name: &str) -> Option<u8> {
    tables().by_name.get(name).map(|&(b, _, _)| b)
}

/// The mnemonic for an opcode byte.
pub fn name_for_byte(b: u8) -> Option<&'static str> {
    tables().by_byte.get(&b).map(|s| s.as_str())
}

/// If `name` is PUSH1..PUSH32, return the immediate size (1..=32).
/// PUSH0 does not count (it has no immediate).
pub fn push_immediate_len(name: &str) -> Option<usize> {
    let n: usize = name.strip_prefix("PUSH")?.parse().ok()?;
    if (1..=32).contains(&n) { Some(n) } else { None }
}

/// Static gas cost of the result-invariant arithmetic, comparison and stack opcodes the
/// block superoptimizer reasons about (`G_base` = 2, `G_verylow` = 3, `G_low` = 5). `None`
/// for any opcode outside that pure set — a block containing one is not a superopt input,
/// so its cost is never queried. PUSH1..32/DUP*/SWAP* are generated like in [`tables`].
#[cfg(feature = "smt")]
pub fn gas(name: &str) -> Option<u32> {
    if push_immediate_len(name).is_some() {
        return Some(3); // G_verylow
    }
    if let Some(rest) = name
        .strip_prefix("DUP")
        .or_else(|| name.strip_prefix("SWAP"))
    {
        if rest
            .parse::<u8>()
            .map(|n| (1..=16).contains(&n))
            .unwrap_or(false)
        {
            return Some(3); // G_verylow
        }
    }
    let g = match name {
        "PUSH0" | "POP" => 2, // G_base
        "ADD" | "SUB" | "NOT" | "LT" | "GT" | "SLT" | "SGT" | "EQ" | "ISZERO" | "AND" | "OR"
        | "XOR" | "BYTE" | "SHL" | "SHR" | "SAR" => 3, // G_verylow
        "MUL" | "DIV" | "SDIV" | "MOD" | "SMOD" | "SIGNEXTEND" => 5, // G_low
        _ => return None,
    };
    Some(g)
}
