//! Frontend: Vyper contract (`.vy`).
//!
//! Requires the `vyper` compiler installed in PATH (checked before compiling).
//! The contract is compiled to assembly (`vyper -f asm`), then the text is
//! normalized and parsed by our parser.
//!
//! Status: EXPERIMENTAL. The exact format of `-f asm` depends on the Vyper
//! version; the `strip` engine targets the symbolic labels `_sym_*revert*` of the
//! venom output (`--experimental-codegen`). On other modes stripping may be
//! incomplete.

use std::process::Command;

use crate::core::asm::{is_symbolic, parse_tokens};

use super::Loaded;

/// Check that `vyper` is installed; return the version string.
pub fn ensure_installed() -> Result<String, String> {
    let out = Command::new("vyper")
        .arg("--version")
        .output()
        .map_err(|e| format!("vyper compiler not found in PATH ({e}); install `pip install vyper`"))?;
    if !out.status.success() {
        return Err("`vyper --version` exited with an error".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Compile the contract to assembly and parse it into instructions.
pub fn load(path: &str, evm_version: Option<&str>) -> Result<Loaded, String> {
    let version = ensure_installed()?;
    eprintln!("vyper: {version}");

    let mut cmd = Command::new("vyper");
    cmd.arg("-f").arg("asm").arg("--experimental-codegen");
    if let Some(ev) = evm_version {
        cmd.arg("--evm-version").arg(ev);
    }
    cmd.arg(path);

    let out = cmd
        .output()
        .map_err(|e| format!("could not run vyper: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "vyper compilation failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let asm_text = String::from_utf8_lossy(&out.stdout);
    let toks = tokenize_vyper_asm(&asm_text);
    let instrs = parse_tokens(&toks);
    if instrs.is_empty() {
        return Err("vyper returned empty assembly".into());
    }
    let symbolic = is_symbolic(&instrs);
    Ok(Loaded { instrs, symbolic, kind: "vyper" })
}

/// Normalize `vyper -f asm` output: drop brackets/commas, keep tokens.
fn tokenize_vyper_asm(text: &str) -> Vec<&str> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '[' | ']' | '{' | '}' | ',' | '(' | ')'))
        .filter(|s| !s.is_empty())
        .collect()
}
