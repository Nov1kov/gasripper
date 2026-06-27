//! Feature `foldshift` — precompute a constant `PUSH a PUSH b SHL/SHR` into a single push.
//!
//! # What it optimizes
//!
//! A `PUSH a PUSH b SHL` (or `SHR`) where both operands are concrete literals computes
//! the compile-time constant `a << b` (`a >> b`) at runtime: two `PUSH` (3 gas each) plus
//! the shift (3 gas) = 9 gas, every execution. Replacing the window with a single push of
//! the precomputed value costs one `PUSH` = 3 gas — a flat **6 gas per occurrence** saved.
//! Like [`crate::features::shuffle`] this is **always safe**: a `PUSH a PUSH b SHL` window
//! has no stack input and one output (the constant), so a single push of that same constant
//! is value- and stack-identical regardless of surrounding code.
//!
//! # Why it fires at all (and the size trade-off)
//!
//! solc deliberately materializes large constants with this idiom to keep **bytecode
//! small**: the address-cleaning mask `1 << 160` is `PUSH1 0x01 PUSH1 0xa0 SHL` (5 bytes)
//! rather than `PUSH21 0x0100…00` (22 bytes). gasripper is an aggressive *gas* optimizer,
//! so it makes the opposite trade — it spends bytecode to lower the per-call gas. The fold
//! therefore **grows the creation bytecode** while lowering runtime gas; it is the first
//! pass that trades size for gas (see `e2e.rs` / README for measured numbers). The idiom is
//! solc-specific — Vyper's venom does not emit it.
//!
//! # Length-changing — symbolic (relinkable) input only
//!
//! Folding three instructions (≥5 bytes) into one push (up to 33 bytes) shifts every later
//! `JUMPDEST` offset, so — like `shuffle`/`involution` — it runs only on symbolic programs,
//! where the compiler's own assembler relinks via the sidecar. The folded literal is carried
//! to the sidecar as a `#<hex>` edit token (see [`crate::core::asm::replacement_instr`]); the
//! sidecars stay dumb and just emit the push.
//!
//! Only `SHL`/`SHR` are folded — the shift family is exactly the constant-materialization
//! idiom compilers leave in their output. General `PUSH a PUSH b <arith>` folding is not
//! done: compilers already fold it, and 256-bit-wrapping arithmetic folds are the easiest
//! place to introduce a correctness bug (see `todo-ebso-features/04`).

use super::FeatureMeta;
use crate::core::asm::push_literal_value;
use crate::core::{Category, Instr, Span, apply_spans};

#[cfg(test)]
mod e2e;

pub const META: FeatureMeta = FeatureMeta {
    key: "foldshift",
    name: "Fold-shift",
    description: "precompute a constant PUSH a PUSH b SHL/SHR into one push (gas down, bytecode up; symbolic input only)",
    category: Category::FoldShift,
    default_enabled: true,
};

/// A [`Span`] for every `PUSH a PUSH b {SHL,SHR}` of two concrete literals, replacing the
/// three-instruction window with one push of the precomputed 256-bit result. A window whose
/// result is `0` is left alone (a `PUSH0` rewrite is a separate concern and never arises
/// from the materialization idiom).
pub fn scan(instrs: &[Instr]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < instrs.len() {
        if let (Some(a), Some(b), Some(op)) = (
            push_literal_value(&instrs[i]),
            push_literal_value(&instrs[i + 1]),
            shift_op(&instrs[i + 2]),
        ) {
            if let (Some(value), Some(shift)) = (lit_to_be32(a), shift_amount(b)) {
                let result = match op {
                    Shift::Left => shl(value, shift),
                    Shift::Right => shr(value, shift),
                };
                if let Some(hex) = nonzero_hex(&result) {
                    out.push(Span {
                        start: i,
                        end: i + 2,
                        category: Category::FoldShift,
                        replacement: vec![format!("#{hex}")],
                    });
                    i += 3;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Apply every fold (for tests/targeted runs); the CLI rewrites via the enabled config
/// through [`crate::features::optimize`].
#[allow(dead_code)] // feature's module API; the CLI rewrites via the orchestrator
pub fn fold(instrs: &[Instr]) -> (Vec<Instr>, Vec<Span>) {
    let spans = scan(instrs);
    (apply_spans(instrs, &spans), spans)
}

enum Shift {
    Left,
    Right,
}

/// `ins` is a `SHL`/`SHR` opcode.
fn shift_op(ins: &Instr) -> Option<Shift> {
    if ins.kind != crate::core::asm::Kind::Op {
        return None;
    }
    match ins.mnem() {
        "SHL" => Some(Shift::Left),
        "SHR" => Some(Shift::Right),
        _ => None,
    }
}

/// A literal token (`0x..` hex or a small decimal) as a big-endian 256-bit value, or `None`
/// if it does not fit in 32 bytes.
fn lit_to_be32(tok: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    if let Some(h) = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X")) {
        let h = if h.len() % 2 == 1 {
            format!("0{h}")
        } else {
            h.to_string()
        };
        let bytes: Vec<u8> = (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16))
            .collect::<Result<_, _>>()
            .ok()?;
        if bytes.len() > 32 {
            return None;
        }
        out[32 - bytes.len()..].copy_from_slice(&bytes);
    } else {
        let v: u128 = tok.parse().ok()?;
        out[16..].copy_from_slice(&v.to_be_bytes());
    }
    Some(out)
}

/// A shift-amount literal as a bit count, saturated to 256 (any shift `>= 256` zeroes the
/// result under EVM `SHL`/`SHR` semantics).
fn shift_amount(tok: &str) -> Option<u32> {
    let b = lit_to_be32(tok)?;
    if b[..30].iter().any(|&x| x != 0) {
        return Some(256);
    }
    Some(((b[30] as u32) << 8) | b[31] as u32)
}

/// `value << shift` over 256 bits (wrapping), big-endian.
fn shl(mut value: [u8; 32], shift: u32) -> [u8; 32] {
    if shift >= 256 {
        return [0u8; 32];
    }
    for _ in 0..shift {
        let mut carry = 0u8;
        for i in (0..32).rev() {
            let next = value[i] >> 7;
            value[i] = (value[i] << 1) | carry;
            carry = next;
        }
    }
    value
}

/// `value >> shift` over 256 bits, big-endian.
fn shr(mut value: [u8; 32], shift: u32) -> [u8; 32] {
    if shift >= 256 {
        return [0u8; 32];
    }
    for _ in 0..shift {
        let mut carry = 0u8;
        for i in 0..32 {
            let next = value[i] & 1;
            value[i] = (value[i] >> 1) | (carry << 7);
            carry = next;
        }
    }
    value
}

/// Minimal upper-case hex of a big-endian value, or `None` if it is zero.
fn nonzero_hex(value: &[u8; 32]) -> Option<String> {
    let start = value.iter().position(|&b| b != 0)?;
    Some(value[start..].iter().map(|b| format!("{b:02X}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::asm::{mnemonics, parse_str, render};

    #[test]
    fn folds_shl_address_mask() {
        // solc's address mask: PUSH1 1 PUSH1 0xa0 SHL == 1 << 160 == 0x01 then 20 zero bytes.
        let p = parse_str("PUSH1 1 PUSH1 0xa0 SHL");
        let (out, spans) = fold(&p);
        assert_eq!(
            spans.len(),
            1,
            "the constant 1<<160 materialization was not folded"
        );
        assert_eq!(
            spans[0].category,
            Category::FoldShift,
            "the span must carry the FoldShift category"
        );
        assert_eq!(
            render(&out),
            format!("PUSH21 0x01{}", "00".repeat(20)),
            "the fold did not produce the precomputed 21-byte push"
        );
    }

    #[test]
    fn folds_shr_constant() {
        // 0x0100 >> 8 == 0x01.
        let p = parse_str("PUSH2 0x0100 PUSH1 8 SHR");
        let (out, spans) = fold(&p);
        assert_eq!(spans.len(), 1, "a constant SHR was not folded");
        assert_eq!(
            render(&out),
            "PUSH1 0x01",
            "the SHR fold produced the wrong literal"
        );
    }

    #[test]
    fn ignores_general_arithmetic() {
        // Only the shift family is folded; PUSH a PUSH b ADD is left to the compiler.
        let p = parse_str("PUSH1 1 PUSH1 2 ADD");
        assert!(
            scan(&p).is_empty(),
            "a non-shift binary op was wrongly folded"
        );
    }

    #[test]
    fn ignores_single_operand() {
        // A shift needs two literal operands; one push is not a constant shift.
        let p = parse_str("PUSH1 8 SHL");
        assert!(
            scan(&p).is_empty(),
            "a lone push before SHL was wrongly treated as a constant fold"
        );
    }

    #[test]
    fn zero_result_is_not_folded() {
        // 1 >> 256 == 0 — folding to an empty/zero push is a separate concern, skip it.
        let p = parse_str("PUSH1 1 PUSH2 0x0100 SHR");
        assert!(
            scan(&p).is_empty(),
            "a shift whose result is zero was wrongly folded"
        );
    }

    #[test]
    fn folded_mnemonic_size_is_minimal() {
        // 0xff << 8 == 0xff00 — a two-byte literal -> PUSH2, not a wider push.
        let p = parse_str("PUSH1 0xff PUSH1 8 SHL");
        let (out, _spans) = fold(&p);
        assert_eq!(
            mnemonics(&out),
            vec!["PUSH2"],
            "the folded push was not sized to its minimal byte length"
        );
    }
}
