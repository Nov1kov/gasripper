//! Assembly model and parser of flat tokens into instructions.
//!
//! Port of `to_instr` / `flatten` / `mnem` from the Python project
//! `evm_asm_optimizer`, but operating on text tokens (rather than the mixed
//! str/int Python list).
//!
//! Supports "symbolic" assembly in the Vyper-venom style:
//!   * `_sym_NAME`  — a symbolic label (a push-label or, if followed by JUMPDEST, a label definition);
//!   * `_OFST sym n` — an offset from a label;
//!   * `_mem_NAME`  — a pseudo memory address;
//! plus concrete opcodes and `PUSHn <int>`.

use super::opcodes::push_immediate_len;

/// Instruction kind (matches `kind` in the Python port).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Opcode without an immediate (including DUPn/SWAPn, arithmetic, JUMP/JUMPI, ...).
    Op,
    /// `PUSHn value` — a concrete push with an immediate.
    Push,
    /// `_sym_*` — push of a symbolic label.
    PushSym,
    /// `_mem_*` — push of a pseudo memory address.
    PushMem,
    /// `_OFST sym n` — push of an offset from a label.
    Ofst,
    /// Label definition: `JUMPDEST` or `_sym_x JUMPDEST`.
    Label,
    /// Raw data (a lone int/byte that is not an immediate).
    Raw,
}

/// An instruction: kind + its tokens. `tokens[0]` is the mnemonic.
#[derive(Clone, Debug)]
pub struct Instr {
    pub kind: Kind,
    pub tokens: Vec<String>,
}

impl Instr {
    pub fn new(kind: Kind, tokens: Vec<String>) -> Self {
        Instr { kind, tokens }
    }

    /// The instruction's mnemonic (first token).
    pub fn mnem(&self) -> &str {
        self.tokens.first().map(|s| s.as_str()).unwrap_or("")
    }
}

/// Whether a token looks like an integer literal (decimal or 0x-hex).
fn is_int_literal(t: &str) -> bool {
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit());
    }
    !t.is_empty() && t.chars().all(|c| c.is_ascii_digit())
}

/// Parse assembly text (tokens separated by spaces/newlines) into instructions.
pub fn parse_str(src: &str) -> Vec<Instr> {
    let toks: Vec<&str> = src.split_whitespace().collect();
    parse_tokens(&toks)
}

/// Parse already-split tokens into instructions (= `to_instr`).
pub fn parse_tokens(toks: &[&str]) -> Vec<Instr> {
    let mut out = Vec::new();
    let n = toks.len();
    let mut i = 0;
    while i < n {
        let t = toks[i];
        if let Some(sym) = strip_sym(t) {
            let _ = sym;
            if i + 1 < n && toks[i + 1] == "JUMPDEST" {
                out.push(Instr::new(Kind::Label, vec![t.into(), "JUMPDEST".into()]));
                i += 2;
            } else {
                out.push(Instr::new(Kind::PushSym, vec![t.into()]));
                i += 1;
            }
            continue;
        }
        if t == "_OFST" {
            let end = (i + 3).min(n);
            out.push(Instr::new(Kind::Ofst, toks[i..end].iter().map(|s| s.to_string()).collect()));
            i = end;
            continue;
        }
        if t.starts_with("_mem_") {
            out.push(Instr::new(Kind::PushMem, vec![t.into()]));
            i += 1;
            continue;
        }
        if push_immediate_len(t).is_some() && i + 1 < n && is_int_literal(toks[i + 1]) {
            out.push(Instr::new(Kind::Push, vec![t.into(), toks[i + 1].into()]));
            i += 2;
            continue;
        }
        if t == "JUMPDEST" {
            out.push(Instr::new(Kind::Label, vec!["JUMPDEST".into()]));
            i += 1;
            continue;
        }
        if is_int_literal(t) {
            out.push(Instr::new(Kind::Raw, vec![t.into()]));
            i += 1;
            continue;
        }
        out.push(Instr::new(Kind::Op, vec![t.into()]));
        i += 1;
    }
    out
}

fn strip_sym(t: &str) -> Option<&str> {
    t.strip_prefix("_sym_")
}

/// Instructions -> flat tokens (inverse of `parse_tokens`).
#[allow(dead_code)] // part of the assembler API; used in tests and round-trips
pub fn flatten(instrs: &[Instr]) -> Vec<String> {
    let mut out = Vec::new();
    for ins in instrs {
        out.extend(ins.tokens.iter().cloned());
    }
    out
}

/// Text representation of the assembly (one instruction per line).
pub fn render(instrs: &[Instr]) -> String {
    instrs
        .iter()
        .map(|ins| ins.tokens.join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// List of mnemonics (the first token of each instruction) — handy for tests.
#[allow(dead_code)] // used in feature and strip-engine tests
pub fn mnemonics(instrs: &[Instr]) -> Vec<String> {
    instrs.iter().map(|ins| ins.mnem().to_string()).collect()
}

/// Whether the program contains symbolic elements that require linking
/// (and are therefore not assemblable by our simple assembler without label resolution).
pub fn is_symbolic(instrs: &[Instr]) -> bool {
    instrs.iter().any(|ins| {
        matches!(ins.kind, Kind::PushSym | Kind::PushMem | Kind::Ofst)
            || (ins.kind == Kind::Label && ins.tokens.len() > 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_with_immediate_parsed_as_single_instr() {
        // "PUSH1 32 ADD" -> [Push(PUSH1,32), Op(ADD)] = 2 instructions.
        let p = parse_str("PUSH1 32 ADD");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].kind, Kind::Push);
        assert_eq!(p[0].tokens, vec!["PUSH1", "32"]);
        assert_eq!(p[1].kind, Kind::Op);
        assert_eq!(p[1].mnem(), "ADD");
    }

    #[test]
    fn revert_symbol_is_pushsym() {
        let p = parse_str("_sym___revert JUMPI");
        assert_eq!(p[0].kind, Kind::PushSym);
        assert!(p[0].mnem().to_lowercase().contains("revert"));
        assert_eq!(p[1].kind, Kind::Op);
    }

    #[test]
    fn sym_followed_by_jumpdest_is_label() {
        let p = parse_str("_sym_block JUMPDEST PUSH0");
        assert_eq!(p[0].kind, Kind::Label);
        assert_eq!(p[0].tokens, vec!["_sym_block", "JUMPDEST"]);
        assert!(is_symbolic(&p));
    }

    #[test]
    fn flatten_roundtrips_tokens() {
        let src = "DUP1 PUSH1 32 LT _sym___revert JUMPI";
        let p = parse_str(src);
        assert_eq!(flatten(&p).join(" "), src);
    }
}
