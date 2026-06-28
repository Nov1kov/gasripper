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
use crate::core::{Instr, Span};
use crate::features::{self, optimize_with};
use crate::input::{self, InputKind, Loaded};
use crate::sidecar::{Backend, Lang};

const AFTER_HELP: &str = "\
FEATURES (all enabled by default):
    guards     — strip provably-safe revert guards (overflow/underflow, calldata
                 bounds, range/cast asserts); safe only under a trusted caller
    shuffle    — reschedule DUP/SWAP/POP windows to a cheaper equivalent (always
                 safe; symbolic input only)
    involution — cancel runs of an involutive op (NOT NOT -> nothing; always safe;
                 symbolic input only)
    recompute  — recompute a cheap nullary opcode instead of DUP-ing it (OP DUP1 ->
                 OP OP; always safe; any input)
    foldshift  — precompute a constant PUSH a PUSH b SHL/SHR into one push (always
                 safe; lowers gas, grows bytecode; symbolic input only)
    cmpnorm    — fold a SWAP1 before a comparison into the mirrored comparator
                 (SWAP1 LT -> GT; always safe; symbolic input only)
    inline     — relocate a small internal function into its call sites, removing
                 the call/return indirection (size via --inline-max-body; symbolic
                 input only)";

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

    /// max internal-function body size (instructions) the inline pass relocates
    #[arg(long = "inline-max-body", value_name = "n")]
    inline_max_body: Option<usize>,

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
    for key in cli
        .disable
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        config.disable(key)?;
    }
    for key in cli
        .enable
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        config.enable(key)?;
    }
    if let Some(n) = cli.inline_max_body {
        config.set_inline_max_body(n);
    }

    let input = cli
        .input
        .clone()
        .ok_or("no input given (file path or '-')")?;

    // Creation-bytecode emission is a dedicated Vyper path: it drives the sidecar
    // (compile -> strip RUNTIME -> re-assemble via Vyper) rather than the generic
    // text-assembly frontend, because only the compiler can relink to bytecode.
    if let Some(out) = &cli.emit_creation {
        return emit_creation(&input, out, &cli, &config);
    }

    // For a compiler source the report/asm must use the sidecar dump (as --emit-creation does):
    // the text frontend fragments venom's multi-token internal-function symbols, hiding the inline
    // pass. Fall back to the text frontend when the sidecar is unavailable (inline counts read 0).
    if let Some(backend) = compiler_backend(&input, cli.input_kind) {
        match backend.dump(&input, cli.evm_version.as_deref()) {
            Ok(dump) => return report_compiler(dump, &backend, &input, &cli, &config),
            Err(e) => tracing::warn!(
                "sidecar dump unavailable ({e}); reporting via the text frontend — \
                 inline detection needs GASRIPPER_VYPER_PYTHON (a python with vyper importable)"
            ),
        }
    }

    let loaded = input::load(&input, cli.input_kind, cli.evm_version.as_deref())?;

    let enabled = config.enabled_categories();
    let (optimized, spans) = optimize_with(&loaded.instrs, &enabled, config.inline_max_body());

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

/// The creation-bytecode backend for a compiler source (Vyper/Solidity), or `None` for an
/// assembly/bytecode/stdin input the sidecar cannot drive.
fn compiler_backend(input: &str, kind: InputKind) -> Option<Backend> {
    match kind {
        InputKind::Vyper => Some(Backend::new(Lang::Vyper)),
        InputKind::Solidity => Some(Backend::new(Lang::Solidity)),
        InputKind::Auto => Backend::from_extension(input),
        _ => None,
    }
}

/// Resolve the creation-bytecode backend from the explicit kind or file extension.
fn resolve_backend(input: &str, kind: InputKind) -> Result<Backend, String> {
    compiler_backend(input, kind).ok_or_else(|| {
        "--emit-creation needs a Vyper (.vy) or Solidity (.sol) source; \
         set --input-kind vyper|solidity for other paths"
            .into()
    })
}

/// Emit optimized creation bytecode for a compiler source (Vyper/Solidity) via
/// the shared sidecar backend.
fn emit_creation(input: &str, out: &str, cli: &Cli, config: &FeatureConfig) -> Result<i32, String> {
    let backend = resolve_backend(input, cli.input_kind)?;

    let evm = cli.evm_version.as_deref();
    // 1. Compile and read RUNTIME instructions + reference creation bytecode.
    let dump = backend.dump(input, evm)?;
    // 2. Decide what to strip with the enabled categories.
    let enabled = config.enabled_categories();
    let (_optimized, spans) = optimize_with(&dump.instrs, &enabled, config.inline_max_body());
    // 3. Re-assemble creation bytecode with those guards removed/rewritten.
    let built = backend.build(input, &spans, evm)?;

    fs::write(out, &built.creation_hex).map_err(|e| format!("could not write {out}: {e}"))?;

    println!("source: {} ({input})", backend.label());
    println!("runtime instructions: {}", dump.instrs.len());
    print_enabled(config);
    println!("potential improvements: {}", spans.len());
    print_spans(&spans, &dump.instrs);
    println!(
        "\nwrote creation bytecode: {out}  ({} -> {} bytes, {:+})",
        built.bytes_before,
        built.bytes_after,
        built.bytes_after as i64 - built.bytes_before as i64
    );
    Ok(0)
}

/// Report what each enabled pass would rewrite, sourcing instructions from the sidecar dump (so a
/// compiler source's report matches what `--emit-creation` produces — the text frontend cannot see
/// the inline pass). Honors `--emit-asm` by rendering the optimized runtime.
fn report_compiler(
    dump: crate::sidecar::Dump,
    backend: &Backend,
    input: &str,
    cli: &Cli,
    config: &FeatureConfig,
) -> Result<i32, String> {
    let enabled = config.enabled_categories();
    let (optimized, spans) = optimize_with(&dump.instrs, &enabled, config.inline_max_body());

    println!("source: {} ({input})", backend.label());
    println!("runtime instructions: {}", dump.instrs.len());
    print_enabled(config);
    println!("potential improvements: {}", spans.len());
    print_spans(&spans, &dump.instrs);

    if let Some(path) = &cli.emit_asm {
        fs::write(path, render(&optimized)).map_err(|e| format!("could not write {path}: {e}"))?;
        println!("\nwrote assembly: {path}");
    } else {
        println!(
            "\nnote: input is symbolic — final bytecode requires linking; \
             use --emit-creation for deployable bytecode (or --emit-asm for the optimized assembly)."
        );
    }
    if cli.emit_bytecode.is_some() {
        return Err(
            "input is symbolic (labels _sym_/_mem_/_OFST): use --emit-creation for deployable \
             bytecode, not --emit-bytecode."
                .into(),
        );
    }
    Ok(0)
}

/// Print the per-feature breakdown and one (length-capped) sample rewritten range per feature that
/// fired. Registry order, so the output is stable; features that found nothing are omitted.
fn print_spans(spans: &[Span], instrs: &[Instr]) {
    if spans.is_empty() {
        return;
    }
    print_category_counts(spans);
    println!("\nsample rewritten ranges (one per feature):");
    for f in features::registry() {
        let Some(s) = spans.iter().find(|s| s.category == f.category) else {
            continue;
        };
        let len = s.end - s.start + 1;
        let mut seq: Vec<String> = instrs[s.start..=s.end]
            .iter()
            .take(8)
            .map(|x| x.mnem().to_string())
            .collect();
        if len > 8 {
            seq.push("…".to_string());
        }
        println!(
            "  [{}..{}] {} -> {}",
            s.start,
            s.end,
            s.category.key(),
            seq.join(" ")
        );
    }
}

/// Print how many potential improvements each enabled feature found (categories
/// that found none are omitted). Registry order, so the breakdown is stable.
fn print_category_counts(spans: &[Span]) {
    for f in features::registry() {
        let n = spans.iter().filter(|s| s.category == f.category).count();
        if n > 0 {
            println!("  {}: {}", f.category.key(), n);
        }
    }
}

/// Print the comma-separated list of enabled features.
fn print_enabled(config: &FeatureConfig) {
    let on: Vec<&str> = features::registry()
        .into_iter()
        .filter(|f| config.is_enabled(f.key))
        .map(|f| f.key)
        .collect();
    println!(
        "enabled features: {}",
        if on.is_empty() {
            "—".to_string()
        } else {
            on.join(", ")
        }
    );
}

fn print_features() {
    println!("Available features (all enabled by default):\n");
    for f in features::registry() {
        println!(
            "  {:8} [{}]  {} — {}",
            f.key,
            f.category.key(),
            f.name,
            f.description
        );
    }
}

fn print_report(loaded: &Loaded, spans: &[Span], config: &FeatureConfig, emitting_asm: bool) {
    println!("source: {}", loaded.kind);
    println!("input instructions: {}", loaded.instrs.len());
    print_enabled(config);
    println!("potential improvements: {}", spans.len());
    print_spans(spans, &loaded.instrs);

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
        assert_eq!(
            c.input.as_deref(),
            Some("in.asm"),
            "positional input was not captured"
        );
        assert_eq!(
            c.disable,
            vec!["guards", "extra"],
            "comma list was not split into features"
        );
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
        let c = Cli::try_parse_from([
            "gasripper",
            "--disable",
            "guards",
            "--disable",
            "extra",
            "in.asm",
        ])
        .unwrap();
        assert_eq!(
            c.disable,
            vec!["guards", "extra"],
            "repeated --disable flags did not accumulate"
        );
    }

    #[test]
    fn unknown_input_kind_errors() {
        assert!(
            Cli::try_parse_from(["gasripper", "--input-kind", "nope", "in.asm"]).is_err(),
            "an unknown --input-kind value was accepted instead of rejected"
        );
    }
}
