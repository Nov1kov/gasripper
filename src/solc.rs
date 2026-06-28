//! Native Solidity creation-bytecode backend (no Python).
//!
//! Drives the `solc` binary's asm-json round-trip directly from Rust, so a user
//! with only `solc` on PATH can run `--emit-creation` without setting up any Python
//! environment.
//!
//! There is still **no hand-written linker** — `solc` re-assembles its OWN EVM
//! assembly (`--asm-json` out, `--import-asm-json` in), reproducing its bytecode
//! byte-for-byte. This module only orchestrates that round-trip and edits the
//! runtime sub-assembly JSON:
//!   * [`dump`]  — `solc --asm-json --optimize`, return the RUNTIME instructions
//!     (`.data["0"].code`) as [`Instr`] descriptors + the reference bytecode;
//!   * [`build`] — apply the strip edits to the runtime code array, re-import via
//!     `solc --import-asm-json --bin`, return the creation bytecode. The
//!     constructor (top-level `.code`) is never touched; assembling with no edits
//!     must reproduce `solc --bin` (baseline invariant — a hard error on drift).
//!
//! Revert-idiom normalization (so the shared strip engine, which detects
//! `<identity> _sym_*revert* JUMPI`, works unchanged): solc reverts via tags, so a
//! guarding `JUMPI` is made to follow a synthetic `pushsym _sym_*revert*`:
//!   * **direct** (`<cond> PUSH[revert_tag] JUMPI`, jump TO a pure-revert block) —
//!     the `PUSH[tag]` becomes `_sym_revert_<n>`; the strip deletes it 1:1.
//!   * **inverse** (`<cond> PUSH[continue_tag] JUMPI; <inline revert>; tag:`, jump
//!     OVER the revert — the `require` form) — the `PUSH[tag]` becomes
//!     `_sym_revert_inv_<n>`. Cutting this guard must ALSO drop the inline revert
//!     block, so [`apply_edits`] expands the delete set. Detection is
//!     deterministic, so `dump` and `build` agree on indices.
//!
//! `solc` is resolved from `GASRIPPER_SOLC` (default `solc` on PATH).

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use crate::core::Span;
use crate::sidecar::{Build, Dump, descriptor_to_instr};

const SOLC_ENV: &str = "GASRIPPER_SOLC";

#[inline]
fn solc_binary() -> String {
    std::env::var(SOLC_ENV).unwrap_or_else(|_| "solc".to_string())
}

/// Run `solc` with `args`, returning its stdout (a non-zero exit is an `Err`).
fn run_solc(args: &[&str]) -> Result<String, String> {
    let bin = solc_binary();
    let out = Command::new(&bin)
        .args(args)
        .output()
        .map_err(|e| format!("could not run solc via '{bin}': {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "solc failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `--optimize` flags plus an optional `--evm-version`, shared by every invocation.
fn optimize_args<'a>(evm: Option<&'a str>, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["--optimize"];
    if let Some(ev) = evm {
        args.push("--evm-version");
        args.push(ev);
    }
    args.extend_from_slice(extra);
    args
}

/// `solc --asm-json` the source and parse the first emitted assembly object. solc
/// prints a header before the JSON, so parsing starts at the first `{` and reads a
/// single value (trailing output is ignored, as the former sidecar's `raw_decode`).
fn asm_json(source: &str, evm: Option<&str>) -> Result<Value, String> {
    let mut args = optimize_args(evm, &["--asm-json"]);
    args.push(source);
    let text = run_solc(&args)?;
    let start = text
        .find('{')
        .ok_or("solc --asm-json: no JSON object in output")?;
    let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
    match stream.next() {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(format!("solc asm-json parse error: {e}")),
        None => Err("solc --asm-json: empty JSON output".into()),
    }
}

/// The compiler's own creation bytecode (`solc --bin`), the baseline for the
/// invariant check.
fn bin_reference(source: &str, evm: Option<&str>) -> Result<String, String> {
    let mut args = optimize_args(evm, &["--bin"]);
    args.push(source);
    parse_binary(&run_solc(&args)?)
}

/// Extract the hex from solc's `--bin` output (the line after `Binary:`).
fn parse_binary(stdout: &str) -> Result<String, String> {
    let lines: Vec<&str> = stdout.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "Binary:" {
            for l in &lines[i + 1..] {
                let t = l.trim();
                if !t.is_empty() {
                    return Ok(t.to_string());
                }
            }
        }
    }
    Err("no 'Binary:' section in solc output".into())
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Assemble an asm-json object to creation bytecode via `solc --import-asm-json`,
/// returning the hex. The object is written to a unique temp file (solc reads a
/// path, not stdin); the file is removed even on failure.
fn import_bin(asm: &Value) -> Result<String, String> {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("gasripper-solc-{}-{n}.json", std::process::id()));
    let text = serde_json::to_string(asm).map_err(|e| format!("serialize asm-json: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("write temp asm-json: {e}"))?;
    let path_str = path.to_string_lossy().into_owned();
    let result = run_solc(&["--import-asm-json", "--bin", &path_str]);
    let _ = std::fs::remove_file(&path);
    parse_binary(&result?)
}

/// The runtime sub-assembly code array (`.data["0"].code`) — the only part edited.
fn runtime_code(asm: &Value) -> Result<&Vec<Value>, String> {
    asm.get(".data")
        .and_then(|d| d.get("0"))
        .and_then(|z| z.get(".code"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| "solc asm-json: missing .data[\"0\"][\".code\"]".to_string())
}

fn set_runtime_code(asm: &mut Value, code: Vec<Value>) -> Result<(), String> {
    let slot = asm
        .get_mut(".data")
        .and_then(|d| d.get_mut("0"))
        .and_then(|z| z.get_mut(".code"))
        .ok_or("solc asm-json: missing .data[\"0\"][\".code\"]")?;
    *slot = Value::Array(code);
    Ok(())
}

#[inline]
fn item_name(item: &Value) -> &str {
    item.get("name").and_then(|n| n.as_str()).unwrap_or("")
}

/// A code item's `value` field as a string: a tag id is a JSON number, a literal
/// push is a hex string — both normalized to one type for comparison/labelling.
fn item_value(item: &Value) -> Option<String> {
    match item.get("value") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Tags whose block is a pure revert: `tag N; JUMPDEST; PUSH; PUSH; REVERT`.
fn revert_tags(code: &[Value]) -> HashSet<String> {
    let mut tags = HashSet::new();
    if code.len() < 5 {
        return tags;
    }
    for i in 0..code.len() - 4 {
        let is_revert = item_name(&code[i]) == "tag"
            && item_name(&code[i + 1]) == "JUMPDEST"
            && item_name(&code[i + 2]) == "PUSH"
            && item_name(&code[i + 3]) == "PUSH"
            && item_name(&code[i + 4]) == "REVERT";
        if let Some(v) = item_value(&code[i]).filter(|_| is_revert) {
            tags.insert(v);
        }
    }
    tags
}

/// Find inverse-idiom guards `<cond> PUSH[contN] JUMPI; <revert>; tag N: JUMPDEST`.
///
/// Returns `(norm, blocks)`:
///   * `norm[push_tag_index]  = N`  — relabel that `PUSH[tag]` as `_sym_revert_inv_N`.
///   * `blocks[jumpi_index]   = [inline revert block indices]` — delete these too
///     when the guard's `JUMPI` is deleted (else execution falls into the revert).
fn inverse_guards(
    code: &[Value],
    revert: &HashSet<String>,
) -> (HashMap<usize, String>, HashMap<usize, Vec<usize>>) {
    let mut norm = HashMap::new();
    let mut blocks = HashMap::new();
    let n = code.len();
    if n == 0 {
        return (norm, blocks);
    }
    for p in 0..n - 1 {
        if item_name(&code[p]) != "PUSH [tag]" || item_name(&code[p + 1]) != "JUMPI" {
            continue;
        }
        let target = match item_value(&code[p]) {
            Some(t) => t,
            None => continue,
        };
        if revert.contains(&target) {
            continue; // direct idiom (jump TO revert) — handled by `descriptor`
        }
        let mut q = p + 2;
        while q < n && item_name(&code[q]) != "tag" {
            q += 1;
        }
        let block: Vec<usize> = (p + 2..q).collect();
        // inverse guard iff the fall-through block ends in REVERT and the JUMPI's
        // target tag is exactly the block's continuation (jump OVER the revert).
        let ends_revert = block
            .last()
            .is_some_and(|&l| item_name(&code[l]) == "REVERT");
        if ends_revert && q < n && item_value(&code[q]).as_deref() == Some(target.as_str()) {
            norm.insert(p, target.clone());
            blocks.insert(p + 1, block);
        }
    }
    (norm, blocks)
}

/// Map a solc asm-json code item to a shared `(kind, mnem, value)` descriptor.
/// `value` is the concrete immediate (`0x..`) only for a plain literal `PUSH`;
/// every other push (jump target, data/size/lib/immutable) is symbolic and carries
/// `None`, so the fold pass never treats it as a constant.
fn descriptor(item: &Value, revert: &HashSet<String>) -> (&'static str, String, Option<String>) {
    let name = item_name(item);
    if name == "PUSH [tag]" {
        let val = item_value(item).unwrap_or_default();
        if revert.contains(&val) {
            return ("pushsym", format!("_sym_revert_{val}"), None);
        }
        return ("push", "PUSH".to_string(), None);
    }
    if name == "tag" {
        return (
            "label",
            format!("_sym_tag_{}", item_value(item).unwrap_or_default()),
            None,
        );
    }
    if name == "JUMPDEST" {
        return ("label", "JUMPDEST".to_string(), None);
    }
    if name == "PUSH" {
        return (
            "push",
            "PUSH".to_string(),
            Some(format!("0x{}", item_value(item).unwrap_or_default())),
        );
    }
    if name.starts_with("PUSH") {
        return ("push", "PUSH".to_string(), None);
    }
    ("op", name.to_string(), None)
}

/// Compile `source` and return its RUNTIME instructions + reference bytecode.
pub fn dump(source: &str, evm: Option<&str>) -> Result<Dump, String> {
    let asm = asm_json(source, evm)?;
    let code = runtime_code(&asm)?;
    let revert = revert_tags(code);
    let (norm, _) = inverse_guards(code, &revert);
    let reference_hex = format!("0x{}", bin_reference(source, evm)?);
    let mut instrs = Vec::with_capacity(code.len());
    for (i, item) in code.iter().enumerate() {
        let (kind, mnem, value) = match norm.get(&i) {
            Some(tag) => ("pushsym", format!("_sym_revert_inv_{tag}"), None),
            None => descriptor(item, &revert),
        };
        let payload = match value {
            Some(v) => format!("{mnem} {v}"),
            None => mnem,
        };
        instrs.push(descriptor_to_instr(kind, &payload)?);
    }
    Ok(Dump {
        instrs,
        reference_hex,
    })
}

/// A replacement op token as an asm-json code item: `#<hex>` is a folded push
/// literal (the fold pass precomputed a constant shift); anything else is a bare
/// opcode.
fn edit_item(op: &str) -> Value {
    match op.strip_prefix('#') {
        Some(hex) => json!({ "name": "PUSH", "value": hex }),
        None => json!({ "name": op }),
    }
}

/// Replace each span's `[start, end]` with its ops; also drop an inverse guard's
/// inline revert block when that guard's `JUMPI` is removed. A drop wins over a
/// replacement another pass placed inside the dropped block (e.g. foldshift folding
/// the Panic-selector inside an inverse guard's inline revert), so no folded push is
/// stranded in deleted code (else `InvalidJump`).
fn apply_edits(code: &[Value], spans: &[Span], blocks: &HashMap<usize, Vec<usize>>) -> Vec<Value> {
    let mut repl: HashMap<usize, (usize, &[String])> = HashMap::new();
    let mut drop: HashSet<usize> = HashSet::new();
    for s in spans {
        repl.insert(s.start, (s.end, &s.replacement));
        if let Some(block) = blocks.get(&s.end) {
            drop.extend(block.iter().copied());
        }
    }
    let mut out = Vec::with_capacity(code.len());
    let n = code.len();
    let mut i = 0;
    while i < n {
        if drop.contains(&i) {
            i = repl.get(&i).map_or(i + 1, |(end, _)| end + 1);
            continue;
        }
        if let Some((end, ops)) = repl.get(&i) {
            out.extend(ops.iter().map(|op| edit_item(op)));
            i = end + 1;
            continue;
        }
        out.push(code[i].clone());
        i += 1;
    }
    out
}

/// Recompile `source`, apply the strip edits to the RUNTIME, and assemble.
pub fn build(source: &str, spans: &[Span], evm: Option<&str>) -> Result<Build, String> {
    let mut asm = asm_json(source, evm)?;
    let reference = bin_reference(source, evm)?;
    // Baseline invariant: re-importing with no edits must reproduce solc's bytecode.
    let baseline = import_bin(&asm)?;
    if baseline != reference {
        return Err("baseline import != solc bytecode (solc version/settings drift)".into());
    }
    let code = runtime_code(&asm)?.clone();
    let (_norm, blocks) = inverse_guards(&code, &revert_tags(&code));
    set_runtime_code(&mut asm, apply_edits(&code, spans, &blocks))?;
    let optimized = import_bin(&asm)?;
    Ok(Build {
        bytes_before: reference.len() / 2,
        bytes_after: optimized.len() / 2,
        creation_hex: format!("0x{optimized}"),
        reference_hex: format!("0x{reference}"),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::{Value, json};

    use crate::core::strip::{Category, Span};

    use super::{apply_edits, inverse_guards, item_name, item_value, revert_tags};

    fn names(items: &[Value]) -> Vec<&str> {
        items.iter().map(item_name).collect()
    }

    // An inverse-idiom guard (`PUSH[tag] JUMPI; <inline revert>; tag: JUMPDEST`) must be
    // detected: its PUSH[tag] index normalized to its continuation tag, and the inline
    // revert block recorded under the JUMPI index so a strip can drop it too.
    #[test]
    fn inverse_guard_detected_with_its_revert_block() {
        let code = vec![
            json!({"name":"PUSH [tag]","value":7}),
            json!({"name":"JUMPI"}),
            json!({"name":"PUSH","value":"00"}),
            json!({"name":"REVERT"}),
            json!({"name":"tag","value":7}),
            json!({"name":"JUMPDEST"}),
        ];
        let (norm, blocks) = inverse_guards(&code, &revert_tags(&code));
        assert_eq!(
            norm.get(&0).map(String::as_str),
            Some("7"),
            "the inverse guard's PUSH[tag] was not normalized to its continuation tag"
        );
        assert_eq!(
            blocks.get(&1),
            Some(&vec![2usize, 3usize]),
            "the inline revert block was not recorded under the guard's JUMPI index"
        );
    }

    // A guard jumping straight TO a pure-revert block (the direct idiom) is NOT an inverse
    // guard: it carries no inline revert block to expand the delete set with.
    #[test]
    fn direct_revert_is_not_an_inverse_guard() {
        let code = vec![
            json!({"name":"PUSH [tag]","value":3}),
            json!({"name":"JUMPI"}),
            json!({"name":"tag","value":3}),
            json!({"name":"JUMPDEST"}),
            json!({"name":"PUSH","value":"00"}),
            json!({"name":"PUSH","value":"00"}),
            json!({"name":"REVERT"}),
        ];
        let (norm, blocks) = inverse_guards(&code, &revert_tags(&code));
        assert!(
            norm.is_empty() && blocks.is_empty(),
            "a direct revert guard was wrongly treated as an inverse guard"
        );
    }

    // When a guard strip deletes an inverse guard's JUMPI, the guard's inline revert block
    // must be dropped even if another pass placed a replacement inside it — else the folded
    // push is stranded in dead code (an InvalidJump at runtime).
    #[test]
    fn drop_of_revert_block_wins_over_a_replacement_inside_it() {
        let code = vec![
            json!({"name":"PUSH [tag]","value":7}),
            json!({"name":"JUMPI"}),
            json!({"name":"PUSH","value":"00"}),
            json!({"name":"REVERT"}),
            json!({"name":"tag","value":7}),
            json!({"name":"JUMPDEST"}),
        ];
        let (_norm, blocks) = inverse_guards(&code, &revert_tags(&code));
        let spans = vec![
            Span {
                start: 0,
                end: 1,
                category: Category::Guard,
                replacement: vec![],
            },
            Span {
                start: 2,
                end: 2,
                category: Category::FoldShift,
                replacement: vec!["#abcd".into()],
            },
        ];
        let out = apply_edits(&code, &spans, &blocks);
        assert_eq!(
            names(&out),
            vec!["tag", "JUMPDEST"],
            "the inverse guard's body survived the strip instead of being fully dropped"
        );
        let stranded = out.iter().any(|i| item_value(i).as_deref() == Some("abcd"));
        assert!(
            !stranded,
            "a folded push was stranded inside the deleted revert block (would be an InvalidJump)"
        );
    }

    // A plain replacement span with no inverse-guard block must swap exactly its range for
    // the replacement opcodes, leaving the surrounding code untouched.
    #[test]
    fn replacement_swaps_only_its_range() {
        let code = vec![
            json!({"name":"CALLVALUE"}),
            json!({"name":"DUP1"}),
            json!({"name":"ISZERO"}),
        ];
        let spans = vec![Span {
            start: 1,
            end: 1,
            category: Category::Recompute,
            replacement: vec!["CALLVALUE".into()],
        }];
        let out = apply_edits(&code, &spans, &HashMap::new());
        assert_eq!(
            names(&out),
            vec!["CALLVALUE", "CALLVALUE", "ISZERO"],
            "the replacement did not swap exactly the targeted instruction"
        );
    }
}
