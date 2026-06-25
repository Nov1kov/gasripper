//! gasripper — super-aggressive gas optimizer for EVM bytecode and assembly.
//!
//! See README: the first line is a disclaimer about the unsafety of aggressive
//! gas optimization. Structure:
//!   * [`core`]     — core (opcodes, assembler, stack simulation, strip engine);
//!   * [`features`] — optimization features, each removing its own class of guards;
//!   * [`input`]    — frontends (asm/bytecode/vyper/solidity);
//!   * [`config`]   — the set of enabled features (defaults -> file -> CLI);
//!   * [`cli`]      — command-line interface.

mod cli;
mod config;
mod core;
mod features;
mod input;
mod sidecar;

fn main() {
    std::process::exit(cli::run());
}
