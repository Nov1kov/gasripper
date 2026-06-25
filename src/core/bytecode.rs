//! Disassembler for raw EVM bytecode and assembler back to concrete bytecode.
//!
//! Limitation: assembling (`assemble`) works ONLY for concrete programs (without
//! symbolic labels `_sym_`/`_mem_`/`_OFST`). Linking symbolic assembly requires
//! label resolution with a fixpoint over PUSH sizes — that is a separate task,
//! and for a gas-critical tool it is safer NOT to guess it. So for symbolic input
//! we emit the optimized assembly text rather than final bytecode.

use super::asm::{Instr, Kind};
use super::opcodes::{name_for_byte, op_byte, push_immediate_len};

/// Parse a hex string (with or without `0x`) into bytes.
pub fn hex_to_bytes(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() % 2 != 0 {
        return Err("odd number of hex characters".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("bad hex: {e}")))
        .collect()
}

/// Bytes -> hex string with a `0x` prefix.
pub fn bytes_to_hex(b: &[u8]) -> String {
    let mut out = String::with_capacity(2 + b.len() * 2);
    out.push_str("0x");
    for byte in b {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Disassemble raw bytecode into instructions.
///
/// A PUSHn immediate is kept as a hex token (`0x..`), which gives an exact
/// round-trip through `assemble`. Unknown bytes become `Raw`.
pub fn disassemble(code: &[u8]) -> Vec<Instr> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < code.len() {
        let b = code[i];
        match name_for_byte(b) {
            Some(name) => {
                if let Some(n) = push_immediate_len(name) {
                    let start = i + 1;
                    let end = (start + n).min(code.len());
                    let imm = &code[start..end];
                    out.push(Instr::new(
                        Kind::Push,
                        vec![name.to_string(), bytes_to_hex(imm)],
                    ));
                    i = end;
                } else if name == "JUMPDEST" {
                    out.push(Instr::new(Kind::Label, vec!["JUMPDEST".into()]));
                    i += 1;
                } else {
                    out.push(Instr::new(Kind::Op, vec![name.to_string()]));
                    i += 1;
                }
            }
            None => {
                out.push(Instr::new(Kind::Raw, vec![format!("0x{b:02x}")]));
                i += 1;
            }
        }
    }
    out
}

/// Assemble a CONCRETE program back into bytecode.
///
/// Errors if symbolic elements are encountered (`_sym_`/`_mem_`/`_OFST` or a
/// named label) — their linking is not supported.
pub fn assemble(instrs: &[Instr]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for ins in instrs {
        match ins.kind {
            Kind::Op => {
                let b = op_byte(ins.mnem())
                    .ok_or_else(|| format!("unknown opcode: {}", ins.mnem()))?;
                out.push(b);
            }
            Kind::Push => {
                let name = ins.mnem();
                let n = push_immediate_len(name)
                    .ok_or_else(|| format!("not PUSH1..32: {name}"))?;
                let b = op_byte(name).ok_or_else(|| format!("unknown PUSH: {name}"))?;
                let val = ins.tokens.get(1).ok_or("PUSH without an immediate")?;
                let bytes = push_value_bytes(val, n)?;
                out.push(b);
                out.extend_from_slice(&bytes);
            }
            Kind::Label => {
                if ins.tokens.len() > 1 {
                    return Err("a symbolic label cannot be assembled without linking".into());
                }
                out.push(op_byte("JUMPDEST").unwrap());
            }
            Kind::Raw => {
                let val = ins.mnem();
                let bytes = hex_or_dec_bytes(val)?;
                out.extend_from_slice(&bytes);
            }
            Kind::PushSym | Kind::PushMem | Kind::Ofst => {
                return Err(format!(
                    "symbolic element '{}' cannot be assembled without linking (use --emit-asm)",
                    ins.mnem()
                ));
            }
        }
    }
    Ok(out)
}

/// The immediate value for PUSHn -> exactly `n` bytes (big-endian, left-pad/check).
fn push_value_bytes(val: &str, n: usize) -> Result<Vec<u8>, String> {
    let raw = hex_or_dec_bytes(val)?;
    if raw.len() > n {
        // Allow leading zeros; otherwise the value does not fit into PUSHn.
        let lead = raw.len() - n;
        if raw[..lead].iter().any(|&b| b != 0) {
            return Err(format!("value {val} does not fit into PUSH{n}"));
        }
        return Ok(raw[lead..].to_vec());
    }
    let mut padded = vec![0u8; n - raw.len()];
    padded.extend_from_slice(&raw);
    Ok(padded)
}

/// Parse a value token (`0x..` or decimal) into minimal big-endian bytes.
fn hex_or_dec_bytes(val: &str) -> Result<Vec<u8>, String> {
    if let Some(h) = val.strip_prefix("0x").or_else(|| val.strip_prefix("0X")) {
        let h = if h.len() % 2 != 0 { format!("0{h}") } else { h.to_string() };
        return hex_to_bytes(&h);
    }
    // Decimal — via u128 (enough for hand-written assembly).
    let v: u128 = val.parse().map_err(|_| format!("bad value: {val}"))?;
    if v == 0 {
        return Ok(vec![0u8]);
    }
    let be = v.to_be_bytes();
    let first = be.iter().position(|&b| b != 0).unwrap();
    Ok(be[first..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{flatten, parse_str};

    #[test]
    fn disassemble_basic() {
        // PUSH1 0x01 PUSH1 0x02 ADD STOP
        let code = vec![0x60, 0x01, 0x60, 0x02, 0x01, 0x00];
        let instrs = disassemble(&code);
        assert_eq!(
            flatten(&instrs).join(" "),
            "PUSH1 0x01 PUSH1 0x02 ADD STOP"
        );
    }

    #[test]
    fn assemble_roundtrip() {
        let code = vec![0x60, 0x20, 0x5b, 0x01, 0xfd];
        let instrs = disassemble(&code);
        let back = assemble(&instrs).unwrap();
        assert_eq!(back, code);
    }

    #[test]
    fn assemble_decimal_push() {
        let instrs = parse_str("PUSH1 32 PUSH0 ADD");
        let back = assemble(&instrs).unwrap();
        assert_eq!(back, vec![0x60, 0x20, 0x5f, 0x01]);
    }

    #[test]
    fn assemble_symbolic_fails() {
        let instrs = parse_str("_sym___revert JUMPI");
        assert!(assemble(&instrs).is_err());
    }

    #[test]
    fn push_truncates_leading_zeros() {
        // 0x0020 into PUSH1 -> 0x20 (a leading zero is allowed).
        assert_eq!(push_value_bytes("0x0020", 1).unwrap(), vec![0x20]);
        // does not fit -> error
        assert!(push_value_bytes("0x0120", 1).is_err());
    }
}
