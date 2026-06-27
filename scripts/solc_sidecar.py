#!/usr/bin/env python3
"""Solidity assembly sidecar for gasripper.

Mirrors `vyper_sidecar.py` and speaks the **same** line protocol, so the Rust side
drives both languages with one code path. Solidity needs no hand-written linker:
`solc` round-trips EVM assembly via `--asm-json` (out) and `--import-asm-json` (in),
reproducing its own bytecode byte-for-byte. So the flow is identical to Vyper:
strip the RUNTIME, let solc re-assemble.

  * ``dump``  — `solc --asm-json` the source, emit the RUNTIME sub-assembly
                (`.data["0"].code`) as `kind mnem` descriptors + reference bytecode.
  * ``build`` — delete the chosen RUNTIME instruction indices, write the modified
                assembly JSON, and `solc --import-asm-json --bin` it back. The
                constructor (top-level `.code`) is never touched. With no deletions
                the result must equal `solc --bin` (baseline invariant).

Revert-idiom normalization: Solidity reverts via tags, not symbolic labels. To let
the shared Rust strip engine (which detects `<identity> _sym_*revert* JUMPI`) work
unchanged, both idioms are normalized so a guarding `JUMPI` is preceded by a
`pushsym _sym_*revert*`:

  * **direct** (`<cond> PUSH[revert_tag] JUMPI`, jump TO a pure-revert block) — the
    `PUSH [tag]` becomes `pushsym _sym_revert_<n>`; delete 1:1.
  * **inverse** (`<cond> PUSH[continue_tag] JUMPI; <inline revert>; tag: JUMPDEST`,
    jump OVER the revert, the require form) — the `PUSH [tag]` becomes `pushsym
    _sym_revert_inv_<n>`. Indices stay 1:1, but cutting this guard must ALSO drop
    the inline revert block (else execution would fall straight into it). So `build`
    expands the delete set: if a guard's `JUMPI` is deleted, its inline revert block
    is deleted too. Detection is deterministic, so `dump` and `build` agree.

The shared Rust engine is untouched; its stack-identity criterion still decides what
is safe to cut (auth/side-effecting guards are preserved automatically).

`solc` is resolved from `GASRIPPER_SOLC` (default `solc` on PATH). Communication is
the shared line protocol on stdout.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile

SOLC = os.environ.get("GASRIPPER_SOLC", "solc")


def _solc(args):
    out = subprocess.run([SOLC] + args, capture_output=True, text=True)
    if out.returncode != 0:
        sys.stderr.write(out.stderr)
        sys.exit(4)
    return out.stdout


def _asm_json(source, evm_version):
    args = ["--asm-json", "--optimize"]
    if evm_version:
        args += ["--evm-version", evm_version]
    args.append(source)
    text = _solc(args)
    obj, _ = json.JSONDecoder().raw_decode(text, text.index("{"))
    return obj


def _bin_reference(source, evm_version):
    args = ["--bin", "--optimize"]
    if evm_version:
        args += ["--evm-version", evm_version]
    args.append(source)
    return _parse_binary(_solc(args))


def _import_bin(asm_obj):
    """Assemble an asm-json object to creation bytecode via solc, return hex."""
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False, encoding="utf-8") as f:
        json.dump(asm_obj, f)
        tmp = f.name
    try:
        return _parse_binary(_solc(["--import-asm-json", "--bin", tmp]))
    finally:
        os.unlink(tmp)


def _parse_binary(stdout):
    lines = stdout.splitlines()
    for i, line in enumerate(lines):
        if line.strip() == "Binary:":
            for l in lines[i + 1:]:
                if l.strip():
                    return l.strip()
    raise ValueError("no 'Binary:' section in solc output")


def _runtime_code(asm):
    return asm[".data"]["0"][".code"]


def _revert_tags(code):
    """Tags whose block is a pure revert: `tag N; JUMPDEST; PUSH; PUSH; REVERT`."""
    tags = set()
    for i in range(len(code) - 4):
        if (code[i]["name"] == "tag" and code[i + 1]["name"] == "JUMPDEST"
                and code[i + 2]["name"] == "PUSH" and code[i + 3]["name"] == "PUSH"
                and code[i + 4]["name"] == "REVERT"):
            tags.add(str(code[i]["value"]))
    return tags


def _inverse_guards(code, revert_tags):
    """Find inverse-idiom guards `<cond> PUSH[contN] JUMPI; <revert>; tag N: JUMPDEST`.

    Returns (norm, blocks):
      * norm[push_tag_index]  = N  — relabel that PUSH[tag] as `_sym_revert_inv_N`.
      * blocks[jumpi_index]   = [inline revert block indices] — delete these too when
        the guard's JUMPI is deleted.
    """
    norm, blocks = {}, {}
    n = len(code)
    for p in range(n - 1):
        if code[p]["name"] != "PUSH [tag]" or code[p + 1]["name"] != "JUMPI":
            continue
        target = str(code[p].get("value"))
        if target in revert_tags:
            continue  # direct idiom (jump TO revert) — handled by _descriptor
        q = p + 2
        while q < n and code[q]["name"] != "tag":
            q += 1
        block = list(range(p + 2, q))
        # inverse guard iff the fall-through block ends in REVERT and the JUMPI's
        # target tag is exactly the block's continuation (jump OVER the revert).
        if block and code[block[-1]]["name"] == "REVERT" and q < n and str(code[q].get("value")) == target:
            norm[p] = target
            blocks[p + 1] = block
    return norm, blocks


def _descriptor(item, revert_tags):
    """Map a solc asm-json code item to a shared `(kind, mnem, value)` descriptor.

    `value` is the concrete immediate (`0x..`) only for a plain literal `PUSH`; every
    other push (jump target, data/size/lib/immutable) is symbolic and carries `None`,
    so the fold pass never treats it as a constant.
    """
    name = item["name"]
    if name == "PUSH [tag]":
        val = str(item.get("value"))
        if val in revert_tags:
            return "pushsym", "_sym_revert_%s" % val, None  # normalized revert target
        return "push", "PUSH", None                         # ordinary jump target
    if name == "tag":
        return "label", "_sym_tag_%s" % item.get("value"), None
    if name == "JUMPDEST":
        return "label", "JUMPDEST", None
    if name == "PUSH":
        return "push", "PUSH", "0x" + str(item.get("value"))  # plain literal push
    if name.startswith("PUSH"):
        # PUSH data, PUSH #[$], PUSH [$], PUSHSIZE, PUSHLIB, PUSHIMMUTABLE, ...
        return "push", "PUSH", None
    return "op", name, None                                  # a plain opcode mnemonic


def cmd_dump(args):
    asm = _asm_json(args.source, args.evm_version)
    code = _runtime_code(asm)
    rev = _revert_tags(code)
    norm, _ = _inverse_guards(code, rev)
    out = ["REF 0x" + _bin_reference(args.source, args.evm_version)]
    for i, item in enumerate(code):
        if i in norm:
            kind, mnem, value = "pushsym", "_sym_revert_inv_%s" % norm[i], None
        else:
            kind, mnem, value = _descriptor(item, rev)
        out.append("INSTR %s %s%s" % (kind, mnem, "" if value is None else " " + value))
    sys.stdout.write("\n".join(out) + "\n")


def _parse_edits(s):
    """`start:end:op1,op2;...` -> [(start, end, [ops])]. Empty string -> []."""
    edits = []
    if not s.strip():
        return edits
    for part in s.split(";"):
        a, b, ops = part.split(":")
        edits.append((int(a), int(b), [o for o in ops.split(",") if o]))
    return edits


def _edit_item(op):
    """A replacement op token as an asm-json code item: `#<hex>` is a folded push
    literal (the fold pass precomputed a constant shift); anything else is a bare
    opcode."""
    if op.startswith("#"):
        return {"name": "PUSH", "value": op[1:]}
    return {"name": op}


def _apply_edits(code, edits, blocks):
    """Replace each `[start, end]` with its ops; also drop an inverse guard's inline
    revert block when that guard's JUMPI is removed."""
    repl = {start: (end, ops) for start, end, ops in edits}
    drop = set()
    for _start, end, _ops in edits:
        if end in blocks:
            drop.update(blocks[end])
    out, i, n = [], 0, len(code)
    while i < n:
        if i in repl:
            end, ops = repl[i]
            out.extend(_edit_item(op) for op in ops)
            i = end + 1
            continue
        if i in drop:
            i += 1
            continue
        out.append(code[i])
        i += 1
    return out


def cmd_build(args):
    asm = _asm_json(args.source, args.evm_version)
    reference = _bin_reference(args.source, args.evm_version)
    # Baseline invariant: re-importing with no edits must reproduce solc's bytecode.
    baseline = _import_bin(asm)
    if baseline != reference:
        sys.stderr.write("baseline import != solc bytecode (solc version/settings drift)\n")
        sys.exit(3)
    code = _runtime_code(asm)
    _, blocks = _inverse_guards(code, _revert_tags(code))
    asm[".data"]["0"][".code"] = _apply_edits(code, _parse_edits(args.edit), blocks)
    optimized = _import_bin(asm)
    sys.stdout.write(
        "CREATION 0x%s\nREFERENCE 0x%s\nBYTES_BEFORE %d\nBYTES_AFTER %d\n"
        % (optimized, reference, len(reference) // 2, len(optimized) // 2)
    )


def main():
    p = argparse.ArgumentParser(description="Solidity assembly sidecar for gasripper")
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
