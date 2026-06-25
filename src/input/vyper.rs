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

/// Base vyper invocation. When `GASRIPPER_VYPER_PYTHON` is set the compiler runs
/// as `<python> -m vyper`, so a real interpreter (e.g. a venv) is used instead of
/// the bare `vyper` on PATH. This matters on Windows: a PyInstaller-frozen
/// `vyper.exe` ignores `PYTHONUTF8` and reads sources in the locale codec (cp1252),
/// while a real Python honors UTF-8 mode and compiles non-ASCII contracts.
fn vyper_command() -> Command {
    match std::env::var("GASRIPPER_VYPER_PYTHON") {
        Ok(python) => {
            let mut cmd = Command::new(python);
            cmd.arg("-m").arg("vyper");
            cmd
        }
        Err(_) => Command::new("vyper"),
    }
}

/// Check that `vyper` is installed; return the version string.
pub fn ensure_installed() -> Result<String, String> {
    let out = vyper_command()
        .arg("--version")
        .output()
        .map_err(|e| format!("vyper compiler not found in PATH ({e}); install `pip install vyper`"))?;
    if !out.status.success() {
        return Err("`vyper --version` exited with an error".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Build the `vyper -f asm` command. `PYTHONUTF8=1` forces Python's UTF-8 mode so
/// vyper reads the source as UTF-8 instead of the Windows locale codec (cp1252),
/// which rejects non-ASCII bytes (e.g. Cyrillic comments) with a UnicodeDecodeError.
fn compile_command(path: &str, evm_version: Option<&str>) -> Command {
    let mut cmd = vyper_command();
    cmd.arg("-f").arg("asm").arg("--experimental-codegen");
    if let Some(ev) = evm_version {
        cmd.arg("--evm-version").arg(ev);
    }
    cmd.arg(path);
    cmd.env("PYTHONUTF8", "1");
    cmd
}

/// Compile the contract to assembly and parse it into instructions.
pub fn load(path: &str, evm_version: Option<&str>) -> Result<Loaded, String> {
    let version = ensure_installed()?;
    tracing::info!("vyper: {version}");

    let out = compile_command(path, evm_version)
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::compile_command;

    // The vyper command must force Python UTF-8 mode; without it vyper opens the
    // source with the Windows locale codec (cp1252) and a non-ASCII byte such as
    // 0x98 (UTF-8 Cyrillic) aborts compilation with a UnicodeDecodeError.
    #[test]
    fn compile_command_forces_python_utf8() {
        let cmd = compile_command("contract.vy", None);
        let utf8 = cmd
            .get_envs()
            .any(|(k, v)| k == "PYTHONUTF8" && v == Some(OsStr::new("1")));
        assert!(utf8, "vyper command is missing PYTHONUTF8=1: non-ASCII contracts fail on Windows");
    }
}
