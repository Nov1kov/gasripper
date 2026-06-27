//! Input frontends: how to obtain instructions from different sources.
//!
//! Supported:
//!   * `asm`      — raw EVM assembly (text), including symbolic venom;
//!   * `bytecode` — raw bytecode (hex), disassembled;
//!   * `vyper`    — a `.vy` contract (needs `vyper` in PATH), EXPERIMENTAL;
//!   * `solidity` — a `.sol` contract (needs `solc` in PATH), EXPERIMENTAL.

use std::fs;

use crate::core::Instr;
use crate::core::bytecode::{disassemble, hex_to_bytes};

pub mod raw_asm;
pub mod solidity;
pub mod vyper;

/// The input type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    Vyper,
    Solidity,
    Asm,
    Bytecode,
    /// Detect by file extension.
    Auto,
}

impl InputKind {
    pub fn parse(s: &str) -> Option<InputKind> {
        Some(match s {
            "vyper" | "vy" => InputKind::Vyper,
            "solidity" | "sol" => InputKind::Solidity,
            "asm" | "evm" => InputKind::Asm,
            "bytecode" | "hex" | "bin" => InputKind::Bytecode,
            "auto" => InputKind::Auto,
            _ => return None,
        })
    }
}

/// The result of loading an input.
pub struct Loaded {
    pub instrs: Vec<Instr>,
    /// True if the program contains symbolic elements (not assemblable to bytecode
    /// without linking) — then final emission is to assembly text only.
    pub symbolic: bool,
    /// Source label for the report.
    pub kind: &'static str,
}

/// Detect the type from the path extension.
fn detect_by_extension(path: &str) -> InputKind {
    let lower = path.to_lowercase();
    if lower.ends_with(".vy") {
        InputKind::Vyper
    } else if lower.ends_with(".sol") {
        InputKind::Solidity
    } else if lower.ends_with(".hex") || lower.ends_with(".bin") {
        InputKind::Bytecode
    } else {
        InputKind::Asm
    }
}

/// Load an input: a file path or `-` for stdin.
pub fn load(path: &str, kind: InputKind, evm_version: Option<&str>) -> Result<Loaded, String> {
    let resolved = match kind {
        InputKind::Auto => {
            if path == "-" {
                return Err(
                    "for stdin specify the type explicitly: --input-kind <asm|bytecode>".into(),
                );
            }
            detect_by_extension(path)
        }
        k => k,
    };

    // Compiler frontends work directly off the file path.
    match resolved {
        InputKind::Vyper => return vyper::load(path, evm_version),
        InputKind::Solidity => return solidity::load(path, evm_version),
        _ => {}
    }

    // asm / bytecode are read as text (file or stdin).
    let content = read_text(path)?;
    match resolved {
        InputKind::Asm => raw_asm::load(&content),
        InputKind::Bytecode => {
            let code = hex_to_bytes(&content)?;
            let instrs = disassemble(&code);
            if instrs.is_empty() {
                return Err("empty bytecode".into());
            }
            Ok(Loaded {
                instrs,
                symbolic: false,
                kind: "bytecode",
            })
        }
        InputKind::Vyper | InputKind::Solidity | InputKind::Auto => unreachable!(),
    }
}

fn read_text(path: &str) -> Result<String, String> {
    if path == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("error reading stdin: {e}"))?;
        Ok(buf)
    } else {
        fs::read_to_string(path).map_err(|e| format!("could not read {path}: {e}"))
    }
}
