//! Core module: everything every feature needs.
//!
//! * [`opcodes`] — EVM opcode table (mnemonic <-> byte, arity);
//! * [`asm`]     — instruction model and assembly-text parser;
//! * [`stack`]   — stack simulation: the safe-removal identity criterion + shuffle rescheduler;
//! * [`strip`]   — the revert-guard strip engine (gated by category);
//! * [`bytecode`]— raw-bytecode disassembler and concrete-assembly assembler.

pub mod asm;
pub mod bytecode;
pub mod opcodes;
pub mod stack;
pub mod strip;

pub use asm::Instr;
pub(crate) use strip::apply_spans;
pub use strip::{Category, Span, strip_guards};
