#!/usr/bin/env python3
"""loadmem_holders.py FOLDED... [--top N] [--group holder|kind|category|driver] [--stacks KIND NEEDLE [DEPTH]]

The holders of the heap profiles `RA_JIT_MEMORY_EVERY` had a worker write as `at-<live MB>.prof`,
side by side. Each FOLDED is what `heapfold.py` (macOS) or `jeprof --collapsed` (Linux) makes of
one profile. Holders are those of `heap.py`: the innermost salsa ingredient on the stack, as its
`value` (what the memo keeps), its `record` (edges and revisions), an `interned` value, or a
tracked struct; what no ingredient made goes to ra-jit's own structures by the function that
allocated it. Prints markdown, MB sampled by jemalloc (about 84% of the live bytes at 2^18 B
sampling), one column per profile:

    --group holder    the N holders largest in the last profile, and what each gained (default)
    --group kind      value, record, interned, tracked_struct, own, other
    --group category  the holders gathered into the stages of a build (macro expansion, MIR, ...)
    --group driver    by the outermost driver on the stack (compile_ahead, resolve, symbol table, ...)

`--stacks KIND NEEDLE [DEPTH]` instead lists, for the last profile, the innermost DEPTH (6)
frames of the largest stacks of holders of that kind whose name has NEEDLE, to name the code
that allocated them. Python 3 standard library only.
"""
import collections, os, re, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import heap  # noqa: E402

# The stages of a build, by what a holder's name has in it; the first that matches.
CATEGORIES = [
    ("macro expansion (expansions, arguments, hygiene)", ["parse_macro_expansion", "macro_arg", "expand_proc_macro", "expand_speculative", "macro_def", "span::hygiene", "SyntaxContext", "MacroCallId"]),
    ("syntax (file parses, ast id maps)", ["EditionedFileId>::parse", "ast_id_map", "AstIdMap", "rowan"]),
    ("bodies, signatures, item trees", ["Body>::", "body_syntax", "with_source_map", "item_tree", "Signature", "AttrFlags", "attrs::", "expr_store"]),
    ("name resolution (def maps, imports)", ["nameres", "ModuleScope", "crate_local_def_map", "block_def_map", "DefMap", "ImportMap"]),
    ("type inference and traits", ["InferenceResult", "infer::", "method_resolution", "lookup_impl", "TraitImpls", "next_solver", "traits::", "specialization", "variance", "generics"]),
    ("MIR (built, runtime, monomorphized)", ["mir::lower", "mir::transform", "mir_body", "runtime_mir", "monomorphized", "borrowck", "mir::"]),
    ("layouts", ["layout"]),
    ("codegen products (compiled_instance, inlinable, roots)", ["compiled_instance", "compiled_root", "inlinable", "Produced", "fragment::Inlinable", "told_type", "row_place", "source_place", "instance_serial", "hir_ty_codegen::"]),
    ("symbol table (rustc's symbols)", ["symbol table"]),
    ("vfs and file loading", ["vfs / file loading"]),
]

# The outermost driver on a stack, by a frame it has; the first that matches.
DRIVERS = [
    ("compile_ahead (codegen, every core)", "ObjectCodegen>::compile_ahead"),
    ("find_stored_roots", "find_stored_roots"),
    ("lower_until / compile_unnamed", "compile_unnamed"),
    ("lower_until", "lower_until"),
    ("resolve_crates (def maps)", "resolve_crates"),
    ("proc-macro expansion", "expand_proc_macro"),
    ("symbol table load", "SymbolTable"),
    ("load workspace (project model, vfs)", "load_cargo"),
    ("load workspace (project model, vfs)", "project_model"),
    ("frozen / store", "frozen::"),
    ("follow_cargo", "follow_cargo"),
]


def category(key):
    kind, name = key
    if kind == "own" and "codegen driver" in name:
        return "codegen driver outside queries (cached::Entries, StableNames, the object)"
    for label, needles in CATEGORIES:
        if any(n in name for n in needles):
            return label
    return "ra-jit other" if kind == "own" else f"other salsa ingredients ({kind})"


def driver(frames):
    for label, needle in DRIVERS:
        if any(needle in f for f in frames):
            return label
    return "other (front end run from rayon and salsa threads)"


def load(path, group):
    totals = collections.Counter()
    for line in open(path):
        stack, _, size = line.rstrip().rpartition(" ")
        frames = stack.split(";")
        key = heap.bucket(frames)
        totals[{"kind": key[0], "category": category(key), "driver": driver(frames)}.get(group, key)] += int(size)
    return totals


def stacks(path, kind, needle, depth):
    totals = collections.Counter()
    for line in open(path):
        stack, _, size = line.rstrip().rpartition(" ")
        frames = stack.split(";")
        key = heap.bucket(frames)
        if key[0] != kind or needle not in key[1]:
            continue
        inner = [f for f in frames if "prof_" not in f and "jemalloc" not in f and "malloc" not in f.lower()[:12]]
        inner = [re.sub(r"\{closure#\d+\}", "{c}", f)[:130] for f in inner[-depth:]]
        totals[" < ".join(reversed(inner))] += int(size)
    print(f"{kind} {needle}: {sum(totals.values()) >> 20} MB sampled in {os.path.basename(path)}")
    for where, size in totals.most_common(14):
        print(f"{size >> 20:7d} MB  {where}")


def main():
    args = sys.argv[1:]
    if "--stacks" in args:
        at = args.index("--stacks")
        files = [a for a in args[:at] if os.path.exists(a)]
        kind, needle = args[at + 1], args[at + 2]
        stacks(files[-1], kind, needle, int(args[at + 3]) if len(args) > at + 3 else 6)
        return
    files = [a for a in args if not a.startswith("--") and os.path.exists(a)]
    top = int(args[args.index("--top") + 1]) if "--top" in args else 30
    group = args[args.index("--group") + 1] if "--group" in args else "holder"
    profiles = [load(f, group) for f in files]
    names = [os.path.basename(f).replace(".folded", "") for f in files]
    print("| " + group + " | " + " | ".join(names) + " | gained |")
    print("|---|" + "---|" * (len(names) + 1))
    for key, _ in profiles[-1].most_common(top):
        row = [p.get(key, 0) >> 20 for p in profiles]
        label = (" ".join(key) if isinstance(key, tuple) else key)[:110].replace("|", "\\|")
        print(f"| {label} | " + " | ".join(map(str, row)) + f" | {row[-1] - row[0]} |")
    whole = [sum(p.values()) >> 20 for p in profiles]
    print("| **sampled live, all holders** | " + " | ".join(map(str, whole)) + f" | {whole[-1] - whole[0]} |")


if __name__ == "__main__":
    main()
