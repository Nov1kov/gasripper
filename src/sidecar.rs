//! Creation-bytecode backends.
//!
//! gasripper deliberately does NOT port a compiler into Rust. Producing final
//! creation bytecode (relinking after a strip) is delegated to each language's
//! native toolchain — gasripper writes no linker.
//!
//! Two backends behind one [`Backend`]:
//!   * [`Lang::Solidity`] — native Rust ([`crate::solc`]): drives the `solc` binary's
//!     `--asm-json` ⇄ `--import-asm-json` round-trip directly, **no Python**;
//!   * [`Lang::Vyper`]    — a Python sidecar (`scripts/vyper_sidecar.py`, embedded in the
//!     binary with `include_str!` and materialized to a temp cache on first use, so a
//!     `cargo install` ships no loose files): re-assembles with Vyper's own
//!     `assembly_to_evm`, a Python library function with no CLI equivalent, so this
//!     backend still shells out to a Python with the `vyper` package.
//!
//! Flow (identical for both):
//!   1. [`Backend::dump`]  — compile, return RUNTIME instruction descriptors
//!      + the reference creation bytecode;
//!   2. run the shared strip engine to choose instruction indices to delete;
//!   3. [`Backend::build`] — recompile, delete exactly those RUNTIME indices, and
//!      let the native toolchain emit the final creation bytecode (constructor
//!      untouched; a baseline mismatch is a hard error).
//!
//! The Vyper sidecar speaks a line-based stdout protocol (so the Rust side parses
//! plain text):
//!   dump  -> `REF 0x<hex>` then one `INSTR <kind> <mnem> [value]` line per instruction.
//!            A concrete literal push carries its `0x..` immediate (the fold pass needs it);
//!            value-less pushes are symbolic / linker-resolved and never folded.
//!   build -> `CREATION 0x<hex>` / `REFERENCE 0x<hex>` / `BYTES_BEFORE n` /
//!            `BYTES_AFTER n`. Delete edits are passed via `--edit` (see [`serialize_edits`]);
//!            a `#<hex>` edit op is a folded push literal the sidecar emits as a single push.
//!
//! Toolchain resolution via environment:
//!   * Solidity — `GASRIPPER_SOLC` (the `solc` binary, default `solc` on PATH);
//!   * Vyper    — `GASRIPPER_VYPER_PYTHON` (the embedded sidecar is materialized at runtime).

use std::path::Path;
use std::process::Command;

use crate::core::asm::{Instr, Kind};

/// The Vyper sidecar source, compiled into the binary so an installed `gasripper`
/// carries it with no loose files. Materialized to disk on first Vyper use.
const SIDECAR_SRC: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/vyper_sidecar.py"
));

/// Cache file name for the materialized sidecar, versioned so a new gasripper
/// release overwrites a stale copy instead of running an old script.
const SIDECAR_NAME: &str = concat!("vyper_sidecar-", env!("CARGO_PKG_VERSION"), ".py");

/// Which language toolchain produces the creation bytecode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Vyper,
    Solidity,
}

/// A creation-bytecode backend bound to one language.
pub struct Backend {
    lang: Lang,
}

/// Result of `dump`: the runtime instructions and the reference creation bytecode.
pub struct Dump {
    pub instrs: Vec<Instr>,
    /// The toolchain's own creation bytecode (baseline for invariant checks).
    #[allow(dead_code)] // protocol field; consumed by tests / future callers
    pub reference_hex: String,
}

/// Result of `build`: the assembled creation bytecode and size accounting.
pub struct Build {
    pub creation_hex: String,
    /// The toolchain's reference bytecode, echoed for a baseline-equality assertion.
    #[allow(dead_code)] // protocol field; consumed by the e2e baseline invariant
    pub reference_hex: String,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

impl Backend {
    pub fn new(lang: Lang) -> Self {
        Backend { lang }
    }

    /// Pick a backend by file extension (`.vy` -> Vyper, `.sol` -> Solidity).
    pub fn from_extension(path: &str) -> Option<Backend> {
        let p = path.to_lowercase();
        if p.ends_with(".vy") {
            Some(Backend::new(Lang::Vyper))
        } else if p.ends_with(".sol") {
            Some(Backend::new(Lang::Solidity))
        } else {
            None
        }
    }

    /// Human label for reports.
    pub fn label(&self) -> &'static str {
        match self.lang {
            Lang::Vyper => "vyper",
            Lang::Solidity => "solidity",
        }
    }

    #[inline]
    fn interpreter(&self) -> String {
        std::env::var("GASRIPPER_VYPER_PYTHON").unwrap_or_else(|_| "python".to_string())
    }

    fn run(
        &self,
        subcmd: &str,
        source: &str,
        evm_version: Option<&str>,
        extra: &[&str],
    ) -> Result<String, String> {
        let interp = self.interpreter();
        let mut cmd = Command::new(&interp);
        cmd.arg(materialize_sidecar(
            &std::env::temp_dir().join("gasripper"),
        )?)
        .arg(subcmd)
        .arg(source);
        if let Some(ev) = evm_version {
            cmd.arg("--evm-version").arg(ev);
        }
        cmd.args(extra);
        cmd.env("PYTHONUTF8", "1");
        let out = cmd
            .output()
            .map_err(|e| format!("could not run {} sidecar via '{interp}': {e}", self.label()))?;
        if !out.status.success() {
            return Err(format!(
                "{} sidecar `{subcmd}` failed:\n{}",
                self.label(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Compile `source` and return its RUNTIME instructions + reference bytecode.
    pub fn dump(&self, source: &str, evm_version: Option<&str>) -> Result<Dump, String> {
        match self.lang {
            Lang::Solidity => crate::solc::dump(source, evm_version),
            Lang::Vyper => self.dump_vyper(source, evm_version),
        }
    }

    fn dump_vyper(&self, source: &str, evm_version: Option<&str>) -> Result<Dump, String> {
        let stdout = self.run("dump", source, evm_version, &[])?;
        let mut instrs = Vec::new();
        let mut reference_hex = None;
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("REF ") {
                reference_hex = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("INSTR ") {
                let (kind, payload) = rest.split_once(' ').unwrap_or((rest, ""));
                instrs.push(descriptor_to_instr(kind, payload.trim())?);
            }
        }
        let reference_hex = reference_hex.ok_or("sidecar dump: missing REF line")?;
        Ok(Dump {
            instrs,
            reference_hex,
        })
    }

    /// Recompile `source`, apply the strip edits to the RUNTIME, and assemble.
    pub fn build(
        &self,
        source: &str,
        spans: &[crate::core::Span],
        evm_version: Option<&str>,
    ) -> Result<Build, String> {
        match self.lang {
            Lang::Solidity => crate::solc::build(source, spans, evm_version),
            Lang::Vyper => self.build_vyper(source, spans, evm_version),
        }
    }

    fn build_vyper(
        &self,
        source: &str,
        spans: &[crate::core::Span],
        evm_version: Option<&str>,
    ) -> Result<Build, String> {
        let edits = serialize_edits(spans);
        let stdout = self.run("build", source, evm_version, &["--edit", &edits])?;
        let mut creation_hex = None;
        let mut reference_hex = None;
        let mut bytes_before = 0usize;
        let mut bytes_after = 0usize;
        for line in stdout.lines() {
            if let Some(r) = line.strip_prefix("CREATION ") {
                creation_hex = Some(r.trim().to_string());
            } else if let Some(r) = line.strip_prefix("REFERENCE ") {
                reference_hex = Some(r.trim().to_string());
            } else if let Some(r) = line.strip_prefix("BYTES_BEFORE ") {
                bytes_before = r.trim().parse().map_err(|_| "bad BYTES_BEFORE")?;
            } else if let Some(r) = line.strip_prefix("BYTES_AFTER ") {
                bytes_after = r.trim().parse().map_err(|_| "bad BYTES_AFTER")?;
            }
        }
        Ok(Build {
            creation_hex: creation_hex.ok_or("sidecar build: missing CREATION line")?,
            reference_hex: reference_hex.ok_or("sidecar build: missing REFERENCE line")?,
            bytes_before,
            bytes_after,
        })
    }
}

/// Map a `kind payload` descriptor line to an [`Instr`]. `payload` is everything after the
/// kind: for `push` it is `PUSHn [0xVALUE]`; for every other kind it is the full mnemonic or
/// symbol. A Vyper internal-function label is a single asm token that nonetheless spans several
/// space-separated words (`_sym_internal 0 name_runtime`), so the payload is kept WHOLE rather
/// than truncated at the first space — the inline pass matches a call site to its function by
/// this full symbol. The strip engine reasons over arity and mnemonics, so a value-less push
/// stays a non-literal (never folded); a concrete literal push carries its `0x..` immediate so
/// the fold pass can precompute a constant shift. Shared by every language backend.
pub(crate) fn descriptor_to_instr(kind: &str, payload: &str) -> Result<Instr, String> {
    let i = match kind {
        "op" => Instr::new(Kind::Op, vec![payload.into()]),
        "push" => match payload.split_once(' ') {
            Some((mnem, value)) => Instr::new(Kind::Push, vec![mnem.into(), value.trim().into()]),
            None => Instr::new(Kind::Push, vec![payload.into()]),
        },
        "pushsym" => Instr::new(Kind::PushSym, vec![payload.into()]),
        "pushmem" => Instr::new(Kind::PushMem, vec![payload.into()]),
        "ofst" => Instr::new(Kind::Ofst, vec![payload.into(), "0".into(), "0".into()]),
        "label" => {
            if payload == "JUMPDEST" {
                Instr::new(Kind::Label, vec!["JUMPDEST".into()])
            } else {
                Instr::new(Kind::Label, vec![payload.into(), "JUMPDEST".into()])
            }
        }
        "raw" => Instr::new(Kind::Raw, vec![payload.into()]),
        other => return Err(format!("unknown instruction descriptor kind: {other}")),
    };
    Ok(i)
}

/// Serialize strip spans for the sidecar `--edit` argument: `start:end:op1,op2;...`.
/// Each edit replaces RUNTIME instructions `[start, end]` with its (possibly empty)
/// `POP`/`SWAP` shuffle. Sorted by start so the sidecar can splice deterministically.
pub fn serialize_edits(spans: &[crate::core::Span]) -> String {
    let mut spans: Vec<&crate::core::Span> = spans.iter().collect();
    spans.sort_by_key(|s| s.start);
    spans
        .iter()
        .map(|s| format!("{}:{}:{}", s.start, s.end, s.replacement.join(",")))
        .collect::<Vec<_>>()
        .join(";")
}

/// Write the embedded sidecar into `dir` (creating it) and return its path. Skips the
/// write when an up-to-date copy is already there; writes via a pid-tagged temp file and
/// an atomic rename so concurrent gasripper processes never read a half-written script.
fn materialize_sidecar(dir: &Path) -> Result<String, String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("could not create sidecar cache dir {}: {e}", dir.display()))?;
    let path = dir.join(SIDECAR_NAME);
    let fresh = std::fs::read_to_string(&path)
        .map(|s| s == SIDECAR_SRC)
        .unwrap_or(false);
    if !fresh {
        let tmp = dir.join(format!("{SIDECAR_NAME}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, SIDECAR_SRC)
            .map_err(|e| format!("could not write vyper sidecar {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| format!("could not install vyper sidecar {}: {e}", path.display()))?;
    }
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialize_writes_embedded_sidecar() {
        // Materializing must drop a .py whose bytes equal the compiled-in source, and a
        // second call over the existing file must not fail (installed-binary path).
        let dir = std::env::temp_dir().join(format!("gasripper-mat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = materialize_sidecar(&dir).expect("embedded vyper sidecar did not materialize");
        let written = std::fs::read_to_string(&path).expect("materialized sidecar is unreadable");
        assert_eq!(
            written, SIDECAR_SRC,
            "materialized sidecar diverges from the embedded source"
        );
        let again =
            materialize_sidecar(&dir).expect("re-materializing over an existing sidecar failed");
        assert_eq!(
            again, path,
            "idempotent materialization returned a different path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
