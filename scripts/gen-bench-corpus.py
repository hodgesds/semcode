#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Generate a synthetic C corpus for semcode query benchmarks.

The corpus is fully deterministic: the same --seed and size arguments always
produce byte-identical files, so a database built from it can be rebuilt and
compared against earlier measurements.

Shape is chosen to exercise the query paths that the layout controls:

  * The call graph is layered.  Every function sits at a depth, and calls
    mostly into the layer below it, so forward and reverse call chains have
    real depth instead of reaching the whole corpus in a couple of hops.
    Callchain traversal and all-paths search are the expensive recursive
    queries, and a uniformly random graph makes them meaningless: with a flat
    graph everything is reachable within ~7 hops of everything else.
  * Layers are a pyramid — many leaf helpers, few entry points — and a tunable
    share of edges skip a layer or point back up, so the graph is a DAG with
    some genuine cycles and self-recursion for the visited-set path to handle.
  * A few "hot" functions (mutex_lock, kmalloc, ...) are called from a large
    fraction of all bodies, giving reverse-call lookups a realistic fan-in.
  * Function names are built from a small vocabulary, so substring searches
    match a predictable share of the corpus rather than 0 or all of it.
  * Structs are defined and referenced across files, so type lookups and
    type references have something to resolve.

corpus.json records representative symbols at the leaf, middle and entry
layers, so benchmarks can measure chains of different shapes without
hardcoding names.

Usage:

    scripts/gen-bench-corpus.py --out ~/semcode-bench-corpus
    cd ~/semcode-bench-corpus && semcode-index -s .
    SEMCODE_BENCH_DB=~/semcode-bench-corpus cargo bench --bench query

The generator writes a corpus.json manifest describing what it produced, so
benchmark runs can record the corpus they measured.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys

# Functions every generated body may call.  These are defined once in core.c
# and get a large, tunable fan-in; `mutex_lock` is the default symbol used by
# benches/query.rs.
HOT_FUNCTIONS = [
    "mutex_lock",
    "mutex_unlock",
    "kmalloc",
    "kfree",
    "spin_lock_irqsave",
]

# Name fragments.  "alloc" is the default substring used by benches/query.rs;
# with this vocabulary it matches roughly one in eight generated names.
SUBSYSTEMS = ["net", "fs", "mm", "sched", "block", "crypto", "usb", "acpi"]
VERBS = ["alloc", "free", "init", "lock", "read", "write", "flush", "probe"]
NOUNS = ["buffer", "node", "page", "queue", "inode", "device", "entry", "chunk"]


class Rng:
    """Deterministic LCG.

    Python's `random` module is not guaranteed stable across releases, and the
    whole point of this corpus is that it can be regenerated identically years
    apart, so the generator carries its own generator.
    """

    def __init__(self, seed):
        self.state = seed & 0xFFFFFFFFFFFFFFFF

    def next(self):
        # Numerical Recipes LCG constants.
        self.state = (self.state * 6364136223846793005 + 1442695040888963407) & 0xFFFFFFFFFFFFFFFF
        return self.state >> 33

    def below(self, n):
        return self.next() % n

    def pick(self, seq):
        return seq[self.below(len(seq))]


def function_name(rng, index):
    return "{}_{}_{}_{}".format(
        rng.pick(SUBSYSTEMS), rng.pick(VERBS), rng.pick(NOUNS), index
    )


def struct_name(index):
    return "bench_state_{}".format(index)


def emit_header(num_structs):
    out = [
        "/* Auto-generated benchmark corpus.  Do not edit. */",
        "#ifndef BENCH_H",
        "#define BENCH_H",
        "",
        "typedef unsigned long size_t;",
        "",
    ]
    for i in range(num_structs):
        out += [
            "struct {} {{".format(struct_name(i)),
            "\tint refcount;",
            "\tunsigned long flags;",
            "\tvoid *private_data;",
            "\tsize_t length;",
            "};",
            "",
        ]
    for hot in HOT_FUNCTIONS:
        out.append("int {}(void *arg);".format(hot))
    out += ["", "#endif /* BENCH_H */", ""]
    return "\n".join(out)


def emit_core():
    out = [
        "/* Auto-generated benchmark corpus.  Do not edit. */",
        '#include "bench.h"',
        "",
    ]
    for hot in HOT_FUNCTIONS:
        out += [
            "int {}(void *arg)".format(hot),
            "{",
            "\tif (!arg)",
            "\t\treturn -1;",
            "\treturn 0;",
            "}",
            "",
        ]
    return "\n".join(out)


def layer_sizes(total, layers):
    """Pyramid: layer 0 (leaves) is widest, the top layer is narrowest."""
    weights = [layers - i for i in range(layers)]
    total_weight = sum(weights)
    sizes = [max(1, total * w // total_weight) for w in weights]
    # Hand any rounding remainder to the leaf layer.
    sizes[0] += total - sum(sizes)
    return sizes


def choose_callee(rng, name, layer, names_by_layer, hot_ratio, cycle_ratio):
    """Pick one call target for a function at `layer`.

    Returns (callee_name, is_hot).  Hot functions take a single argument, so
    the caller needs to know which form to emit.
    """
    roll = rng.below(100)

    if roll < hot_ratio or layer == 0:
        return rng.pick(HOT_FUNCTIONS), True

    if roll < hot_ratio + cycle_ratio:
        # Back edge: call into this layer or above, creating cycles and (when
        # the pick lands on the caller itself) direct recursion.
        upper = [l for l in range(layer, len(names_by_layer)) if names_by_layer[l]]
        if upper:
            return rng.pick(names_by_layer[rng.pick(upper)]), False
        return name, False

    # Normal edge: mostly one layer down, sometimes skipping a layer, which is
    # what keeps chain depth from being perfectly uniform.
    target = layer - 1
    if target > 0 and rng.below(100) < 20:
        target -= 1
    return rng.pick(names_by_layer[target]), False


def emit_file(rng, names, layer_of, names_by_layer, num_structs, calls_per_fn,
              hot_ratio, cycle_ratio):
    """Emit one .c file defining `names`."""
    out = [
        "/* Auto-generated benchmark corpus.  Do not edit. */",
        '#include "bench.h"',
        "",
    ]

    for name in names:
        st = struct_name(rng.below(num_structs))
        layer = layer_of[name]
        out += [
            "/* layer {} */".format(layer),
            "int {}(struct {} *state, size_t length)".format(name, st),
            "{",
            "\tint ret = 0;",
            "",
            "\tif (!state)",
            "\t\treturn -1;",
        ]
        # Distinct callees per body.  Without this, a 35% hot ratio routinely
        # spends four of six call slots on the same lock, which throws away
        # graph edges and flattens chain depth.
        chosen = []
        seen = set()
        for _ in range(calls_per_fn * 4):
            if len(chosen) == calls_per_fn:
                break
            callee, is_hot = choose_callee(
                rng, name, layer, names_by_layer, hot_ratio, cycle_ratio
            )
            if callee in seen:
                continue
            seen.add(callee)
            chosen.append((callee, is_hot))

        for callee, is_hot in chosen:
            if is_hot:
                out.append("\tret |= {}(state);".format(callee))
            else:
                out.append("\tret |= {}(state, length);".format(callee))
        out += [
            "\tstate->refcount++;",
            "\treturn ret;",
            "}",
            "",
        ]
    return "\n".join(out)


def run_git(repo, *args, when=0):
    env = dict(os.environ)
    # Fixed timestamps keep the git SHAs identical across regenerations.
    # `when` advances by whole days so each commit gets a distinct SHA.
    stamp = "2020-01-01T00:00:00+0000" if when == 0 else \
        "2020-01-{:02d}T00:00:00+0000".format(1 + (when % 28))
    env.update(
        {
            "GIT_AUTHOR_NAME": "bench",
            "GIT_AUTHOR_EMAIL": "bench@example.com",
            "GIT_COMMITTER_NAME": "bench",
            "GIT_COMMITTER_EMAIL": "bench@example.com",
            "GIT_AUTHOR_DATE": stamp,
            "GIT_COMMITTER_DATE": stamp,
        }
    )
    subprocess.run(["git", "-C", repo] + list(args), check=True, env=env,
                   stdout=subprocess.DEVNULL)


def write_history(out, file_names, commits, churn_pct):
    """Rewrite a slice of files per commit so functions accumulate versions.

    Without this the corpus has one commit, every function has exactly one
    version, and any measurement of history depth -- which is most of what a
    kernel-sized database contains -- is invisible.
    """
    if commits <= 1:
        return 0

    touched_total = 0
    per_commit = max(1, len(file_names) * churn_pct // 100)

    for n in range(1, commits):
        # Deterministic rotating slice, so a given seed reproduces exactly.
        start = (n * per_commit) % len(file_names)
        chosen = [file_names[(start + i) % len(file_names)]
                  for i in range(per_commit)]

        for path in chosen:
            full = os.path.join(out, path)
            with open(full) as f:
                text = f.read()
            # Touch the body of every function in the file: a new local whose
            # name changes per commit.  Cheap to generate, and it changes the
            # blob hash so the indexer stores a new version.
            text = text.replace(
                "\tint ret = 0;",
                "\tint ret = 0;\n\tint rev_{} = {};".format(n, n))
            with open(full, "w") as f:
                f.write(text)
            touched_total += 1

        run_git(out, "add", "-A")
        run_git(out, "commit", "-q", "-m", "Synthetic revision {}".format(n),
                when=n)

    return touched_total


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--out", required=True, help="output directory (created; must not exist unless --force)")
    p.add_argument("--files", type=int, default=1000, help="number of .c files (default 1000)")
    p.add_argument("--funcs-per-file", type=int, default=100, help="functions per file (default 100)")
    p.add_argument("--structs", type=int, default=512, help="number of struct definitions (default 512)")
    p.add_argument("--calls-per-fn", type=int, default=6, help="call sites per function (default 6)")
    p.add_argument("--max-depth", type=int, default=12,
                   help="call graph layers, i.e. the nominal deepest call chain (default 12). "
                        "Back edges from --cycle-ratio can produce longer simple paths.")
    p.add_argument("--hot-ratio", type=int, default=35,
                   help="percent of call sites aimed at hot functions (default 35)")
    p.add_argument("--cycle-ratio", type=int, default=3,
                   help="percent of call sites that point back up a layer, creating cycles "
                        "and self-recursion (default 3)")
    p.add_argument("--commits", type=int, default=1,
                   help="number of commits (default 1). Commits after the first rewrite a "
                        "slice of files so functions accumulate versions, which is what "
                        "makes history depth measurable.")
    p.add_argument("--churn-pct", type=int, default=5,
                   help="percent of files rewritten per commit (default 5)")
    p.add_argument("--seed", type=int, default=20260731, help="PRNG seed (default 20260731)")
    p.add_argument("--force", action="store_true", help="overwrite an existing output directory")
    args = p.parse_args()

    out = os.path.abspath(os.path.expanduser(args.out))
    if os.path.exists(out):
        if not args.force:
            sys.exit("error: {} already exists (use --force to overwrite)".format(out))
        shutil.rmtree(out)
    os.makedirs(out)

    total_funcs = args.files * args.funcs_per_file

    if args.max_depth < 2:
        sys.exit("error: --max-depth must be at least 2")

    # Names are generated first so bodies can call anything in the corpus.
    rng = Rng(args.seed)
    all_names = [function_name(rng, i) for i in range(total_funcs)]
    # Duplicate names would make exact-lookup benchmarks ambiguous.
    assert len(set(all_names)) == len(all_names), "generated duplicate function names"

    # Assign layers: leaves first, so a file (a contiguous slice of the name
    # list) holds functions of similar depth, the way a real source file does.
    sizes = layer_sizes(total_funcs, args.max_depth)
    names_by_layer = []
    layer_of = {}
    cursor = 0
    for layer, size in enumerate(sizes):
        chunk = all_names[cursor:cursor + size]
        names_by_layer.append(chunk)
        for n in chunk:
            layer_of[n] = layer
        cursor += size

    with open(os.path.join(out, "bench.h"), "w") as f:
        f.write(emit_header(args.structs))
    with open(os.path.join(out, "core.c"), "w") as f:
        f.write(emit_core())

    body_rng = Rng(args.seed ^ 0x5DEECE66D)
    total_bytes = 0
    file_names = []
    for i in range(args.files):
        chunk = all_names[i * args.funcs_per_file:(i + 1) * args.funcs_per_file]
        text = emit_file(body_rng, chunk, layer_of, names_by_layer, args.structs,
                         args.calls_per_fn, args.hot_ratio, args.cycle_ratio)
        rel = "bench_{:05d}.c".format(i)
        file_names.append(rel)
        path = os.path.join(out, rel)
        with open(path, "w") as f:
            f.write(text)
        total_bytes += len(text)

    substring_matches = sum(1 for n in all_names if "alloc" in n)

    # Representative symbols for chain benchmarks.  A leaf has enormous
    # fan-in and a shallow forward chain; an entry point is the mirror image;
    # the middle is expensive in both directions.
    def representative(layer):
        candidates = names_by_layer[layer]
        return candidates[len(candidates) // 2] if candidates else None

    manifest = {
        "seed": args.seed,
        "files": args.files,
        "funcs_per_file": args.funcs_per_file,
        "total_functions": total_funcs + len(HOT_FUNCTIONS),
        "structs": args.structs,
        "calls_per_fn": args.calls_per_fn,
        "max_depth": args.max_depth,
        "hot_ratio": args.hot_ratio,
        "cycle_ratio": args.cycle_ratio,
        "layer_sizes": sizes,
        "source_bytes": total_bytes,
        "hot_functions": HOT_FUNCTIONS,
        "bench_symbol": "mutex_lock",
        "bench_substring": "alloc",
        "substring_matches": substring_matches,
        "commits": args.commits,
        "leaf_symbol": representative(0),
        "mid_symbol": representative(args.max_depth // 2),
        "entry_symbol": representative(args.max_depth - 1),
    }
    with open(os.path.join(out, "corpus.json"), "w") as f:
        json.dump(manifest, f, indent=2, sort_keys=True)
        f.write("\n")

    run_git(out, "init", "-q", "-b", "main")
    run_git(out, "add", "-A")
    run_git(out, "commit", "-q", "-m", "Synthetic semcode benchmark corpus")

    touched = write_history(out, file_names, args.commits, args.churn_pct)
    if touched:
        print("  {} commits, {} file revisions".format(args.commits, touched))

    print("corpus written to {}".format(out))
    print("  {} files, {} functions, {:.1f} MiB of C".format(
        args.files, manifest["total_functions"], total_bytes / (1024 * 1024)))
    print("  {} layers, sizes {}{}".format(
        args.max_depth,
        sizes[:4],
        "..." if len(sizes) > 4 else ""))
    print("  {} names contain 'alloc' ({:.1f}%)".format(
        substring_matches, 100.0 * substring_matches / total_funcs))
    print("  chain symbols: leaf={} mid={} entry={}".format(
        manifest["leaf_symbol"], manifest["mid_symbol"], manifest["entry_symbol"]))
    print("")
    print("next:")
    print("  semcode-index -s {}".format(out))
    print("  SEMCODE_BENCH_DB={} cargo bench --bench query".format(out))


if __name__ == "__main__":
    main()
