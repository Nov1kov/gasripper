//! Frontend: Solidity contract (`.sol`).
//!
//! Requires the `solc` compiler installed in PATH. The contract is compiled to
//! runtime bytecode (`solc --bin-runtime`), which is then disassembled.
//!
//! Status: EXPERIMENTAL. The disassembled bytecode does NOT contain symbolic
//! revert labels, so the current `strip` engine (which detects by `_sym_*revert*`)
//! will strip practically nothing on it. The frontend is left as an extension
//! point: detecting revert guards by resolved jumps into revert blocks is a
//! separate task (see README, "Limitations" section).

use std::process::Command;

use crate::core::bytecode::{disassemble, hex_to_bytes};

use super::Loaded;

/// Check that `solc` is installed; return the version string.
pub fn ensure_installed() -> Result<String, String> {
    let out = Command::new("solc")
        .arg("--version")
        .output()
        .map_err(|e| format!("solc compiler not found in PATH ({e})"))?;
    if !out.status.success() {
        return Err("`solc --version` exited with an error".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Compile the contract to runtime bytecode and disassemble it.
pub fn load(path: &str, evm_version: Option<&str>) -> Result<Loaded, String> {
    let version = ensure_installed()?;
    eprintln!("solc: {version}");

    let mut cmd = Command::new("solc");
    cmd.arg("--bin-runtime").arg("--optimize");
    if let Some(ev) = evm_version {
        cmd.arg("--evm-version").arg(ev);
    }
    cmd.arg(path);

    let out = cmd
        .output()
        .map_err(|e| format!("could not run solc: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "solc compilation failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let hex = extract_bin_runtime(&String::from_utf8_lossy(&out.stdout))
        .ok_or("no 'Binary of the runtime part' found in solc output")?;
    let code = hex_to_bytes(&hex)?;
    let instrs = disassemble(&code);
    if instrs.is_empty() {
        return Err("solc returned empty bytecode".into());
    }
    // symbolic=false: this is raw bytecode, assembled back as-is.
    Ok(Loaded { instrs, symbolic: false, kind: "solidity" })
}

/// Extract the hex after the header line `Binary of the runtime part:`.
fn extract_bin_runtime(stdout: &str) -> Option<String> {
    let mut lines = stdout.lines();
    while let Some(line) = lines.next() {
        if line.contains("Binary of the runtime part") {
            // The hex is on the next non-empty line.
            for l in lines.by_ref() {
                let t = l.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}
