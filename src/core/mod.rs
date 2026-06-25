//! Core module: everything every feature needs.
//!
//! * [`opcodes`] — EVM opcode table (mnemonic <-> byte, arity);
//! * [`asm`]     — instruction model and assembly-text parser;
//! * [`stack`]   — stack simulation (the identity criterion for safe removal);
//! * [`strip`]   — the revert-guard strip engine (gated by category);
//! * [`bytecode`]— raw-bytecode disassembler and concrete-assembly assembler.

pub mod asm;
pub mod bytecode;
pub mod opcodes;
pub mod stack;
pub mod strip;

pub use asm::Instr;
pub use strip::{Category, Span, strip_guards};
