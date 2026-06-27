//! Creation-bytecode backends via language sidecars.
//!
//! gasripper deliberately does NOT port a compiler into Rust. Producing final
//! creation bytecode (relinking after a strip) is delegated to each language's
//! native toolchain through a thin sidecar that speaks one shared, line-based
//! text protocol — so this Rust side stays dependency-free (pure `std`) and the
//! orchestration is written ONCE for every language.
//!
//! Two backends, one protocol:
//!   * [`Lang::Vyper`]    — `scripts/vyper_sidecar.py`, re-assembles with Vyper's
//!     own `assembly_to_evm`;
//!   * [`Lang::Solidity`] — `scripts/solc_sidecar.py`, round-trips through
//!     `solc --asm-json` ⇄ `--import-asm-json`.
//!
//! Flow (identical for both):
//!   1. [`Backend::dump`]  — compile, return RUNTIME instruction descriptors
//!      (`kind mnem`) + the reference creation bytecode;
//!   2. run the shared strip engine to choose instruction indices to delete;
//!   3. [`Backend::build`] — recompile, delete exactly those RUNTIME indices, and
//!      let the native toolchain emit the final creation bytecode (constructor
//!      untouched; a baseline mismatch is a hard error in the sidecar).
//!
//! Protocol (stdout):
//!   dump  -> `REF 0x<hex>` then one `INSTR <kind> <mnem> [value]` line per instruction.
//!            A concrete literal push carries its `0x..` immediate (the fold pass needs it);
//!            value-less pushes are symbolic / linker-resolved and never folded.
//!   build -> `CREATION 0x<hex>` / `REFERENCE 0x<hex>` / `BYTES_BEFORE n` /
//!            `BYTES_AFTER n`. Delete indices are passed comma-separated; a `#<hex>` edit
//!            op is a folded push literal the sidecar emits as a single push.
//!
//! Resolution via environment (so the tool can be pointed at the right toolchain):
//!   * `GASRIPPER_VYPER_PYTHON` / `GASRIPPER_VYPER_SIDECAR`;
//!   * `GASRIPPER_SOLC_PYTHON`  / `GASRIPPER_SOLC_SIDECAR` (+ `GASRIPPER_SOLC` for
//!     the `solc` binary, read by the sidecar).

use std::process::Command;

use crate::core::asm::{Instr, Kind};

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

    fn interpreter(&self) -> String {
        let var = match self.lang {
            Lang::Vyper => "GASRIPPER_VYPER_PYTHON",
            Lang::Solidity => "GASRIPPER_SOLC_PYTHON",
        };
        std::env::var(var).unwrap_or_else(|_| "python".to_string())
    }

    fn script(&self) -> String {
        match self.lang {
            Lang::Vyper => std::env::var("GASRIPPER_VYPER_SIDECAR").unwrap_or_else(|_| {
                concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/vyper_sidecar.py").to_string()
            }),
            Lang::Solidity => std::env::var("GASRIPPER_SOLC_SIDECAR").unwrap_or_else(|_| {
                concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/solc_sidecar.py").to_string()
            }),
        }
    }

    fn run(&self, subcmd: &str, source: &str, evm_version: Option<&str>, extra: &[&str]) -> Result<String, String> {
        let interp = self.interpreter();
        let mut cmd = Command::new(&interp);
        cmd.arg(self.script()).arg(subcmd).arg(source);
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
        let stdout = self.run("dump", source, evm_version, &[])?;
        let mut instrs = Vec::new();
        let mut reference_hex = None;
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("REF ") {
                reference_hex = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("INSTR ") {
                let mut it = rest.splitn(3, ' ');
                let kind = it.next().unwrap_or("");
                let mnem = it.next().unwrap_or("").trim();
                let value = it.next().map(|v| v.trim()).filter(|v| !v.is_empty());
                instrs.push(descriptor_to_instr(kind, mnem, value)?);
            }
        }
        let reference_hex = reference_hex.ok_or("sidecar dump: missing REF line")?;
        Ok(Dump { instrs, reference_hex })
    }

    /// Recompile `source`, apply the strip edits to the RUNTIME, and assemble.
    pub fn build(&self, source: &str, spans: &[crate::core::Span], evm_version: Option<&str>) -> Result<Build, String> {
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

/// Map a `kind mnem [value]` descriptor to an [`Instr`]. The strip engine reasons over
/// stack arity and mnemonics, so most immediates are irrelevant and carry placeholders.
/// A concrete literal push carries its real `value` (a `0x..` immediate) — the fold pass
/// needs it to precompute a constant shift; a value-less push is symbolic/linker-resolved
/// and stays a non-literal (so it is never folded). Shared by every language backend.
fn descriptor_to_instr(kind: &str, mnem: &str, value: Option<&str>) -> Result<Instr, String> {
    let i = match kind {
        "op" => Instr::new(Kind::Op, vec![mnem.into()]),
        "push" => match value {
            Some(v) => Instr::new(Kind::Push, vec![mnem.into(), v.into()]),
            None => Instr::new(Kind::Push, vec![mnem.into()]),
        },
        "pushsym" => Instr::new(Kind::PushSym, vec![mnem.into()]),
        "pushmem" => Instr::new(Kind::PushMem, vec![mnem.into()]),
        "ofst" => Instr::new(Kind::Ofst, vec![mnem.into(), "0".into(), "0".into()]),
        "label" => {
            if mnem == "JUMPDEST" {
                Instr::new(Kind::Label, vec!["JUMPDEST".into()])
            } else {
                Instr::new(Kind::Label, vec![mnem.into(), "JUMPDEST".into()])
            }
        }
        "raw" => Instr::new(Kind::Raw, vec![mnem.into()]),
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
