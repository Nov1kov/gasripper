//! gasripper command-line interface.
//!
//! By default (only an input path given) the tool enables ALL features and prints
//! a report of what would be stripped. The config file and any flags are optional.

use std::fs;

use clap::{CommandFactory, Parser};
use tracing_subscriber::EnvFilter;

use crate::config::FeatureConfig;
use crate::core::asm::render;
use crate::core::bytecode::{assemble, bytes_to_hex};
use crate::core::Span;
use crate::features::{self, optimize};
use crate::input::{self, InputKind, Loaded};
use crate::sidecar::{Backend, Lang};

const AFTER_HELP: &str = "\
FEATURES (all enabled by default):
    guards   — strip provably-safe revert guards (overflow/underflow, calldata
               bounds, range/cast asserts); safe only under a trusted caller
    shuffle  — reschedule DUP/SWAP/POP windows to a cheaper equivalent (always
               safe; symbolic input only)";

/// Super-aggressive gas optimizer for EVM bytecode/assembly.
#[derive(Parser, Debug)]
#[command(name = "gasripper", version, about, after_help = AFTER_HELP)]
struct Cli {
    /// path to .vy / .sol / .asm / .hex, or '-' for stdin (with --input-kind)
    input: Option<String>,

    /// input frontend: vyper|solidity|asm|bytecode (default: auto by extension)
    #[arg(long = "input-kind", value_name = "kind", default_value = "auto",
          value_parser = parse_input_kind)]
    input_kind: InputKind,

    /// feature config file (not used by default)
    #[arg(long, value_name = "path")]
    config: Option<String>,

    /// disable features (comma-separated; flag may repeat)
    #[arg(long, value_name = "f,f,...", value_delimiter = ',')]
    disable: Vec<String>,

    /// enable features (overrides --config)
    #[arg(long, value_name = "f,f,...", value_delimiter = ',')]
    enable: Vec<String>,

    /// EVM version for compiler frontends (vyper/solc)
    #[arg(long = "evm-version", value_name = "v")]
    evm_version: Option<String>,

    /// show what would be stripped (this is the default)
    #[arg(long)]
    report: bool,

    /// write the optimized assembly text
    #[arg(long = "emit-asm", value_name = "path")]
    emit_asm: Option<String>,

    /// write the optimized bytecode (hex; non-symbolic input only)
    #[arg(long = "emit-bytecode", value_name = "path")]
    emit_bytecode: Option<String>,

    /// write optimized creation bytecode hex
    #[arg(long = "emit-creation", value_name = "path")]
    emit_creation: Option<String>,

    /// list features and exit
    #[arg(long = "list-features")]
    list_features: bool,
}

#[inline]
fn parse_input_kind(s: &str) -> Result<InputKind, String> {
    InputKind::parse(s).ok_or_else(|| format!("unknown --input-kind: {s}"))
}

/// Install a diagnostic-log subscriber writing to stderr; the level comes from
/// `RUST_LOG` (default `info`). The CLI report itself stays on stdout.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// CLI entry point. Returns the process exit code.
pub fn run() -> i32 {
    init_tracing();
    if std::env::args_os().len() <= 1 {
        let _ = Cli::command().print_help();
        println!();
        return 0;
    }
    run_inner(Cli::parse()).unwrap_or_else(|e| {
        tracing::error!("{e}");
        1
    })
}

fn run_inner(cli: Cli) -> Result<i32, String> {
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
    for key in cli.disable.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        config.disable(key)?;
    }
    for key in cli.enable.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
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
    let (optimized, spans) = optimize(&loaded.instrs, &enabled);

    print_report(&loaded, &spans, &config, cli.emit_asm.is_some());

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
    let (_optimized, spans) = optimize(&dump.instrs, &enabled);
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

/// Print the strip count and a few sample stripped ranges.
fn print_span_summary(spans: &[Span], instrs: &[crate::core::Instr]) {
    println!("checks to strip: {}", spans.len());
    if spans.is_empty() {
        return;
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

fn print_report(loaded: &Loaded, spans: &[Span], config: &FeatureConfig, emitting_asm: bool) {
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

    // Show the first few stripped ranges as mnemonics.
    println!("\nsample stripped ranges:");
    for s in spans.iter().take(5) {
        let seq: Vec<String> = loaded.instrs[s.start..=s.end]
            .iter()
            .map(|x| x.mnem().to_string())
            .collect();
        println!("  [{}..{}] {} -> {}", s.start, s.end, s.category.key(), seq.join(" "));
    }

    if loaded.symbolic && !emitting_asm {
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
        let c = Cli::try_parse_from(["gasripper", "--disable", "guards,extra", "in.asm"]).unwrap();
        assert_eq!(c.input.as_deref(), Some("in.asm"), "positional input was not captured");
        assert_eq!(c.disable, vec!["guards", "extra"], "comma list was not split into features");
    }

    #[test]
    fn unknown_option_errors() {
        assert!(
            Cli::try_parse_from(["gasripper", "--nope"]).is_err(),
            "an unknown option was accepted instead of rejected"
        );
    }

    #[test]
    fn repeated_disable_accumulates() {
        let c = Cli::try_parse_from(["gasripper", "--disable", "guards", "--disable", "extra", "in.asm"])
            .unwrap();
        assert_eq!(c.disable, vec!["guards", "extra"], "repeated --disable flags did not accumulate");
    }

    #[test]
    fn unknown_input_kind_errors() {
        assert!(
            Cli::try_parse_from(["gasripper", "--input-kind", "nope", "in.asm"]).is_err(),
            "an unknown --input-kind value was accepted instead of rejected"
        );
    }
}
