#!/usr/bin/env python3
"""Vyper assembly sidecar for gasripper.

gasripper does not (and must not) port the Vyper compiler into Rust: Python is
Vyper's native compiler and re-implementing its assembler would be dangerous for
a gas-critical tool. Instead, this thin sidecar exposes exactly two operations
that the Rust side drives over a process boundary:

  * ``dump``  — compile a ``.vy`` source and emit the RUNTIME assembly as a flat
                list of per-instruction descriptors ``kind mnem`` (plus the
                reference creation bytecode). Rust parses these descriptors, runs
                its strip engine and decides which instruction indices to delete.

  * ``build`` — recompile the same source, delete the instruction indices chosen
                by Rust from the RUNTIME assembly, and re-assemble the whole
                program (constructor + runtime) back to creation bytecode using
                Vyper's OWN assembler (``assembly_to_evm``). The constructor is
                never touched. With an empty delete-set the output MUST equal the
                reference bytecode (baseline invariant — a Vyper version drift is
                a hard error).

The instruction segmentation (``to_instr``) is identical in ``dump`` and
``build`` and is a 1:1 match of the Rust parser, so the indices Rust returns line
up with the list ``build`` deletes from. Communication is a simple line-based
text protocol on stdout (so the Rust side stays dependency-free / pure ``std``):

  dump  -> ``REF 0x<hex>`` then one ``INSTR <kind> <mnem>`` line per instruction.
  build -> ``CREATION 0x<hex>`` / ``REFERENCE 0x<hex>`` / ``BYTES_BEFORE n`` /
           ``BYTES_AFTER n``. Delete indices are passed as a comma-separated list.

Requires an importable ``vyper`` package (tested on 0.4.3). The interpreter is
chosen by the Rust caller; this script only needs ``import vyper`` to work.
"""
import argparse
import sys

from vyper.compiler.phases import CompilerData
from vyper.compiler.settings import Settings, OptimizationLevel
from vyper.ir import compile_ir
from vyper.ir.compile_ir import RuntimeHeader

PUSH_OPS = {f"PUSH{n}" for n in range(1, 33)}


def to_instr(asm):
    """Flat Vyper-assembly tokens -> list of (kind, tokens).

    kind: op | push | pushsym | pushmem | ofst | label | raw.
    1:1 port of the Rust parser so instruction indices match across the boundary.
    """
    out, i, n = [], 0, len(asm)
    while i < n:
        t = asm[i]
        if isinstance(t, (list, int)):
            out.append(("raw", [t])); i += 1; continue
        if isinstance(t, str) and t.startswith("_sym_"):
            if i + 1 < n and asm[i + 1] == "JUMPDEST":
                out.append(("label", [t, "JUMPDEST"])); i += 2; continue
            out.append(("pushsym", [t])); i += 1; continue
        if t == "_OFST":
            out.append(("ofst", list(asm[i:i + 3]))); i += 3; continue
        if isinstance(t, str) and t.startswith("_mem_"):
            out.append(("pushmem", [t])); i += 1; continue
        if t in PUSH_OPS and i + 1 < n and isinstance(asm[i + 1], int):
            out.append(("push", [t, asm[i + 1]])); i += 2; continue
        if t == "JUMPDEST":
            out.append(("label", ["JUMPDEST"])); i += 1; continue
        out.append(("op", [t])); i += 1
    return out


def flatten(instr):
    out = []
    for _, toks in instr:
        out.extend(toks)
    return out


def compile_data(source_code, evm_version=None):
    return CompilerData(source_code, settings=Settings(
        experimental_codegen=True, optimize=OptimizationLevel.GAS, evm_version=evm_version))


def _runtime_index(asm):
    for i, item in enumerate(asm):
        if isinstance(item, list) and item and isinstance(item[0], RuntimeHeader):
            return i
    raise ValueError("runtime sublist not found in assembly")


def _runtime_instr(data):
    asm = list(data.assembly)
    ri = _runtime_index(asm)
    return asm, ri, to_instr(asm[ri][1:])


def _parse_edits(s):
    """`start:end:op1,op2;...` -> [(start, end, [ops])]. Empty string -> []."""
    edits = []
    if not s.strip():
        return edits
    for part in s.split(";"):
        a, b, ops = part.split(":")
        edits.append((int(a), int(b), [o for o in ops.split(",") if o]))
    return edits


def _assemble(data, edits):
    """Re-assemble creation bytecode after applying RUNTIME edits (replace [start,end]
    with the given POP/SWAP ops; an empty op list is a plain deletion)."""
    asm, ri, instr = _runtime_instr(data)
    header = asm[ri][0]
    out = list(instr)
    for start, end, ops in sorted(edits, key=lambda e: e[0], reverse=True):
        out[start:end + 1] = [("op", [op]) for op in ops]
    asm[ri] = [header] + flatten(out)
    metadata = bytes.fromhex(data.integrity_sum)
    return bytes(compile_ir.assembly_to_evm(asm, compiler_metadata=metadata)[0])


def cmd_dump(args):
    src = _read(args.source)
    data = compile_data(src, args.evm_version)
    _, _, instr = _runtime_instr(data)
    out = ["REF 0x" + bytes(data.bytecode).hex()]
    for kind, toks in instr:
        # mnem must be a single whitespace-free token (it always is for EVM asm).
        out.append("INSTR %s %s" % (kind, str(toks[0])))
    sys.stdout.write("\n".join(out) + "\n")


def cmd_build(args):
    src = _read(args.source)
    data = compile_data(src, args.evm_version)
    reference = bytes(data.bytecode)
    # Baseline invariant: assembling with no edits must reproduce vyper's bytecode.
    baseline = _assemble(data, [])
    if baseline != reference:
        sys.stderr.write("baseline assembly != vyper bytecode (vyper version/settings drift)\n")
        sys.exit(3)
    optimized = _assemble(data, _parse_edits(args.edit))
    sys.stdout.write(
        "CREATION 0x%s\nREFERENCE 0x%s\nBYTES_BEFORE %d\nBYTES_AFTER %d\n"
        % (optimized.hex(), reference.hex(), len(reference), len(optimized))
    )


def _read(path):
    if path == "-":
        return sys.stdin.read()
    with open(path, "r", encoding="utf-8") as f:
        return f.read()


def main():
    p = argparse.ArgumentParser(description="Vyper assembly sidecar for gasripper")
    sub = p.add_subparsers(dest="cmd", required=True)

    d = sub.add_parser("dump", help="emit runtime instruction descriptors + reference bytecode")
    d.add_argument("source")
    d.add_argument("--evm-version", default=None)
    d.set_defaults(func=cmd_dump)

    b = sub.add_parser("build", help="apply runtime strip edits and assemble creation bytecode")
    b.add_argument("source")
    b.add_argument("--edit", default="", help="strip edits: start:end:op1,op2;... (empty = baseline)")
    b.add_argument("--evm-version", default=None)
    b.set_defaults(func=cmd_build)

    args = p.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
