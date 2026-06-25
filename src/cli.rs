//! gasripper command-line interface.
//!
//! By default (only an input path given) the tool enables ALL features and prints
//! a report of what would be stripped. The config file and any flags are optional.

use std::collections::HashMap;
use std::fs;

use crate::config::FeatureConfig;
use crate::core::asm::render;
use crate::core::bytecode::{assemble, bytes_to_hex};
use crate::core::{Category, Span, strip_guards};
use crate::features;
use crate::input::{self, InputKind, Loaded};
use crate::sidecar::{Backend, Lang};

const HELP: &str = "\
gasripper — super-aggressive gas optimizer for EVM bytecode/assembly.

USAGE:
    gasripper [OPTIONS] <INPUT>

INPUT:
    path to .vy / .sol / .asm / .hex, or '-' for stdin (with --input-kind).

OPTIONS:
    --input-kind <kind>    vyper|solidity|asm|bytecode (default: auto by extension)
    --config <path>        feature config file (not used by default)
    --disable <f,f,...>    disable features (comma-separated; flag may repeat)
    --enable <f,f,...>     enable features (overrides --config)
    --evm-version <v>      EVM version for compiler frontends (vyper/solc)
    --report               show what would be stripped and exit (this is the default)
    --emit-asm <path>      write the optimized assembly text
    --emit-bytecode <path> write the optimized bytecode (hex; non-symbolic input only)
    --emit-creation <path> write optimized creation bytecode (hex; Vyper source only)
    --list-features        list features and exit
    -h, --help             this help
    -V, --version          version

FEATURES (all enabled by default):
    math    — strip overflow/underflow and arithmetic revert guards
    abi     — strip ABI/calldata bounds checks
    assert  — strip other range/cast assert checks

ALWAYS preserved: authorization (CALLER/ORIGIN), any side effects, and checks that
consume their own input.";

/// CLI entry point. Returns the process exit code.
pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run_inner(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

struct Cli {
    input: Option<String>,
    input_kind: InputKind,
    config: Option<String>,
    disable: Vec<String>,
    enable: Vec<String>,
    evm_version: Option<String>,
    report: bool,
    emit_asm: Option<String>,
    emit_bytecode: Option<String>,
    emit_creation: Option<String>,
    list_features: bool,
    help: bool,
    version: bool,
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut c = Cli {
        input: None,
        input_kind: InputKind::Auto,
        config: None,
        disable: Vec::new(),
        enable: Vec::new(),
        evm_version: None,
        report: false,
        emit_asm: None,
        emit_bytecode: None,
        emit_creation: None,
        list_features: false,
        help: false,
        version: false,
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let take = |name: &str| -> Result<String, String> {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{name} requires an argument"))
        };
        match a {
            "-h" | "--help" => c.help = true,
            "-V" | "--version" => c.version = true,
            "--report" => c.report = true,
            "--list-features" => c.list_features = true,
            "--input-kind" => {
                let v = take("--input-kind")?;
                c.input_kind = InputKind::parse(&v)
                    .ok_or_else(|| format!("unknown --input-kind: {v}"))?;
                i += 1;
            }
            "--config" => {
                c.config = Some(take("--config")?);
                i += 1;
            }
            "--disable" => {
                c.disable.extend(split_list(&take("--disable")?));
                i += 1;
            }
            "--enable" => {
                c.enable.extend(split_list(&take("--enable")?));
                i += 1;
            }
            "--evm-version" => {
                c.evm_version = Some(take("--evm-version")?);
                i += 1;
            }
            "--emit-asm" => {
                c.emit_asm = Some(take("--emit-asm")?);
                i += 1;
            }
            "--emit-bytecode" => {
                c.emit_bytecode = Some(take("--emit-bytecode")?);
                i += 1;
            }
            "--emit-creation" => {
                c.emit_creation = Some(take("--emit-creation")?);
                i += 1;
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option: {other}"));
            }
            _ => {
                if c.input.is_some() {
                    return Err(format!("extra positional argument: {a}"));
                }
                c.input = Some(a.to_string());
            }
        }
        i += 1;
    }
    Ok(c)
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn run_inner(args: &[String]) -> Result<i32, String> {
    let cli = parse_args(args)?;

    if cli.help || args.is_empty() {
        println!("{HELP}");
        return Ok(0);
    }
    if cli.version {
        println!("gasripper {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if cli.list_features {
        print_features();
        return Ok(0);
    }

    // Collect feature config: defaults -> file -> CLI.
    let mut config = FeatureConfig::defaults();
    if let Some(path) = &cli.config {
        let content =
            fs::read_to_string(path).map_err(|e| format!("could not read config {path}: {e}"))?;
        config.apply_file(&content)?;
    }
    for key in &cli.disable {
        config.disable(key)?;
    }
    for key in &cli.enable {
        config.enable(key)?;
    }

    let input = cli.input.clone().ok_or("no input given (file path or '-')")?;

    // Creation-bytecode emission is a dedicated Vyper path: it drives the sidecar
    // (compile -> strip RUNTIME -> re-assemble via Vyper) rather than the generic
    // text-assembly frontend, because only the compiler can relink to bytecode.
    if let Some(out) = &cli.emit_creation {
        return emit_creation(&input, out, &cli, &config);
    }

    let loaded = input::load(&input, cli.input_kind, cli.evm_version.as_deref())?;

    let enabled = config.enabled_categories();
    let (optimized, spans) = strip_guards(&loaded.instrs, &enabled);

    print_report(&loaded, &spans, &config);

    // Emission (if requested). --report does not block writing, but without emit
    // flags we just print the report.
    if let Some(path) = &cli.emit_asm {
        let text = render(&optimized);
        fs::write(path, text).map_err(|e| format!("could not write {path}: {e}"))?;
        println!("wrote assembly: {path}");
    }
    if let Some(path) = &cli.emit_bytecode {
        if loaded.symbolic {
            return Err(
                "input is symbolic (labels _sym_/_mem_/_OFST): final bytecode requires linking, \
                 which is not implemented yet. Use --emit-asm."
                    .into(),
            );
        }
        let before = assemble(&loaded.instrs)?;
        let after = assemble(&optimized)?;
        fs::write(path, bytes_to_hex(&after))
            .map_err(|e| format!("could not write {path}: {e}"))?;
        println!(
            "wrote bytecode: {path}  ({} -> {} bytes, {:+} )",
            before.len(),
            after.len(),
            after.len() as i64 - before.len() as i64
        );
    }

    Ok(0)
}

/// Resolve the creation-bytecode backend from the explicit kind or file extension.
fn resolve_backend(input: &str, kind: InputKind) -> Result<Backend, String> {
    match kind {
        InputKind::Vyper => Ok(Backend::new(Lang::Vyper)),
        InputKind::Solidity => Ok(Backend::new(Lang::Solidity)),
        InputKind::Auto => Backend::from_extension(input).ok_or_else(|| {
            "--emit-creation needs a Vyper (.vy) or Solidity (.sol) source; \
             set --input-kind vyper|solidity for other paths"
                .into()
        }),
        _ => Err("--emit-creation supports only Vyper/Solidity sources".into()),
    }
}

/// Emit optimized creation bytecode for a compiler source (Vyper/Solidity) via
/// the shared sidecar backend.
fn emit_creation(
    input: &str,
    out: &str,
    cli: &Cli,
    config: &FeatureConfig,
) -> Result<i32, String> {
    let backend = resolve_backend(input, cli.input_kind)?;

    let evm = cli.evm_version.as_deref();
    // 1. Compile and read RUNTIME instructions + reference creation bytecode.
    let dump = backend.dump(input, evm)?;
    // 2. Decide what to strip with the enabled categories.
    let enabled = config.enabled_categories();
    let (_optimized, spans) = strip_guards(&dump.instrs, &enabled);
    // 3. Re-assemble creation bytecode with those guards removed/rewritten.
    let built = backend.build(input, &spans, evm)?;

    fs::write(out, &built.creation_hex).map_err(|e| format!("could not write {out}: {e}"))?;

    println!("source: {} ({input})", backend.label());
    println!("runtime instructions: {}", dump.instrs.len());
    print_span_summary(&spans, &dump.instrs);
    println!(
        "\nwrote creation bytecode: {out}  ({} -> {} bytes, {:+})",
        built.bytes_before,
        built.bytes_after,
        built.bytes_after as i64 - built.bytes_before as i64
    );
    Ok(0)
}

/// Print a per-category summary and a few sample stripped ranges.
fn print_span_summary(spans: &[Span], instrs: &[crate::core::Instr]) {
    println!("checks to strip: {}", spans.len());
    if spans.is_empty() {
        return;
    }
    let mut by_cat: HashMap<Category, usize> = HashMap::new();
    for s in spans {
        *by_cat.entry(s.category).or_insert(0) += 1;
    }
    for cat in [Category::Abi, Category::Math, Category::Assert] {
        if let Some(n) = by_cat.get(&cat) {
            println!("  {:8}: {n}", cat.key());
        }
    }
    println!("\nsample stripped ranges:");
    for s in spans.iter().take(5) {
        let seq: Vec<String> = instrs[s.start..=s.end].iter().map(|x| x.mnem().to_string()).collect();
        println!("  [{}..{}] {} -> {}", s.start, s.end, s.category.key(), seq.join(" "));
    }
}

fn print_features() {
    println!("Available features (all enabled by default):\n");
    for f in features::registry() {
        println!("  {:8} [{}]  {} — {}", f.key, f.category.key(), f.name, f.description);
    }
}

fn print_report(loaded: &Loaded, spans: &[Span], config: &FeatureConfig) {
    println!("source: {}", loaded.kind);
    println!("input instructions: {}", loaded.instrs.len());

    let on: Vec<&str> = features::registry()
        .into_iter()
        .filter(|f| config.is_enabled(f.key))
        .map(|f| f.key)
        .collect();
    println!("enabled features: {}", if on.is_empty() { "—".to_string() } else { on.join(", ") });

    println!("checks to strip: {}", spans.len());
    if spans.is_empty() {
        return;
    }
    let mut by_cat: HashMap<Category, usize> = HashMap::new();
    for s in spans {
        *by_cat.entry(s.category).or_insert(0) += 1;
    }
    for cat in [Category::Abi, Category::Math, Category::Assert] {
        if let Some(n) = by_cat.get(&cat) {
            println!("  {:8}: {n}", cat.key());
        }
    }

    // Show the first few stripped ranges as mnemonics.
    println!("\nsample stripped ranges:");
    for s in spans.iter().take(5) {
        let seq: Vec<String> = loaded.instrs[s.start..=s.end]
            .iter()
            .map(|x| x.mnem().to_string())
            .collect();
        println!("  [{}..{}] {} -> {}", s.start, s.end, s.category.key(), seq.join(" "));
    }

    if loaded.symbolic {
        println!(
            "\nnote: input is symbolic — final bytecode requires linking; \
             use --emit-asm for the optimized assembly."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_args() {
        let args: Vec<String> = ["--disable", "math,abi", "in.asm"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let c = parse_args(&args).unwrap();
        assert_eq!(c.input.as_deref(), Some("in.asm"));
        assert_eq!(c.disable, vec!["math", "abi"]);
    }

    #[test]
    fn unknown_option_errors() {
        let args: Vec<String> = ["--nope".to_string()].to_vec();
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn repeated_disable_accumulates() {
        let args: Vec<String> = ["--disable", "math", "--disable", "assert", "in.asm"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let c = parse_args(&args).unwrap();
        assert_eq!(c.disable, vec!["math", "assert"]);
    }
}
