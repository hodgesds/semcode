# Query Performance: Baselines and Layout Plan

Working notes for the `lancedb-query-perf` effort. Records what was measured,
what the target layout is, and what is left to do.

## Measurement setup

Two corpora:

* **Synthetic** — `scripts/gen-bench-corpus.py`, deterministic, layered call
  graph. Default 1200 files x 120 functions = 144 k functions, 12 layers,
  53.9 MiB of C. Regenerates byte-identically from `--seed`; indexes in 11.6 s,
  so it is cheap to rebuild after a schema change.
* **Kernel** — `~/linux/.semcode.db`, 44 GB, `functions.lance` 11.5 GB in 22
  data files. Note: its `lore` table is missing a data file, so the query CLI
  cannot open it; the benchmark harness can, because it never calls
  `create_tables`.

`benches/query.rs` runs in 34.75 s on the synthetic corpus. Scan-bound queries
are gated behind `SEMCODE_BENCH_SCANS=1`.

## The scalar indices covered zero rows

On the synthetic corpus, every index was built over an empty table and never
refreshed, so the planner fell back to a full scan for every query — including
all the synthetic baselines originally recorded here.

```
functions: 144010 rows
  name_idx  on ["name"]  -> BTree      indexed=0 unindexed=144010
  calls_idx on ["calls"] -> LabelList  indexed=0 unindexed=144010
  ...
```

`create_scalar_indices()` is called from `create_tables()`, which runs at the
*start* of indexing when the tables are empty. On every later run it hits this
guard and returns:

```rust
if count > 100 {
    // "Skipping index creation - likely already indexed"
    return Ok(());
}
```

The indices are therefore created exactly once, over zero rows, and never
rebuilt. `OptimizeAction::Index` would have fixed it, but it only ran inside
`optimize_single_table`, which is gated behind `check_optimization_health()` —
a fragment-count check that reports healthy on a normal database.

An index that covers no rows is invisible from the outside: `list_indices()`
reports it, the files exist on disk, and queries return correct results. Only
`index_stats()` shows the coverage, which is why `examples/list-indices.rs`
prints it.

**This bites small and medium databases specifically.** The kernel database was
measured too, and its indices are 99.7% covered (11 948 772 of 11 989 396 rows)
— it has enough fragments that `check_optimization_health()` fires and the
index step runs as a side effect of compaction. So the trap is exactly
backwards from how you would want it: benchmarks on a modest corpus measure
scans, while the large database measures index probes. That is why the earlier
synthetic numbers and the kernel strace numbers disagreed about what was
expensive.

The kernel table also has no `calls_idx` at all — it predates the LabelList
change, and the `count > 100` guard means it would never acquire one.

The fix is `SchemaManager::optimize_scalar_indices()`, called unconditionally at
the end of indexing and decoupled from compaction and pruning. It creates any
missing indices first (so an existing database picks up new ones), then
refreshes coverage. It costs 0.7 s for 144 k functions.

Confirming with `explain_plan` — before, the filter is evaluated by the reader:

```
LanceRead: ... full_filter=array_has_any(calls, List([mm_free_inode_110772]))
```

after, it is an index probe:

```
ScalarIndexQuery: query=[array_has_any(calls, List([...]))]@calls_idx
```

Isolated filter timings, warm:

| Filter | scan | index probe |
| --- | ---: | ---: |
| `name = 'x'` | 7.2 ms | 1.8 ms |
| `array_has(calls, 'x')` | 48.5 ms | 2.3 ms |

## Baselines

Synthetic corpus, 144 k functions. "Before" is JSON relationship columns with
uncovered indices; "after" is `List<Utf8>` with covered indices.

| Benchmark | before | after | |
| --- | ---: | ---: | ---: |
| `lookup/find_type` | 1.72 ms | 1.16 ms | −33% |
| `lookup/find_function` | 11.2 ms | 4.67 ms | −58% |
| `lookup/get_function_callees` | 11.3 ms | 4.52 ms | −60% |
| `chain/callees_git_aware` (leaf/mid/entry) | 16.4 / 16.7 / 16.4 ms | 11.1 / 11.1 / 10.9 ms | −33% |
| `chain/callers_git_aware` (leaf/mid/entry) | 33.9 / 35.7 / 33.6 ms | 11.4 / 11.5 / 11.3 ms | −66% |
| probe `get_function_callers` (66 882 results) | 128 ms | 138 ms | flat |
| probe `search_functions_fuzzy` | 249 ms | 186 ms | −25% |

The caller count is identical before and after (66 882), which is the main
correctness check on the `array_has_any` rewrite.

**The list migration alone was a regression.** Measured with the lists in place
but the indices still uncovered, `callers_git_aware` went from 33.9 ms to
56 ms — a LabelList evaluated by the reader is more expensive than a substring
match on a contiguous JSON string. Lists only pay off once an index uses them,
which is worth remembering before porting the same change to other columns.

Kernel, under strace, same probe set:

| | baseline | + column projection |
| --- | ---: | ---: |
| `pread64` | 534 914 | 278 167 |
| syscall time | 28.16 s | 15.21 s |

Unbounded queries, kernel: `search_functions_regex` 375 s / 240 405 results;
`find_all_paths` does not terminate usefully. Both are opt-in for that reason.

## Storage

Synthetic corpus, whole database:

| | JSON | `List<Utf8>` |
| --- | ---: | ---: |
| database, indices uncovered | 37 MB | 25 MB |
| `functions.lance/data` | — | 13 MB |

Native lists with dictionary encoding cut stored data ~32%.

Populated indices then cost more than the data they index: `functions.lance` is
13 MB of data against **37 MB of `_indices`**, taking the database to 84 MB.
That matters for the object-storage plan, where index size is the query-node
cache budget. Some of it is waste — there are BTree indices on `line_start` and
`line_end`, and nothing queries a function by line number.

## Changes evaluated

* **Column projection on `get_function_callers`** — kept. 48% fewer bytes read.
  It reads `name` and `calls` but was materializing all ten columns.
* **Bulk content hydration + shard handle caching** — rejected. A/B with
  projection held constant in both arms: opens 13 128 -> 14 344, reads
  identical, wall time 1.2 s -> 1.3 s. `get_content` is already an index-served
  point lookup; replacing 100 of those with 16 IN-list queries per shard loses.
  Parked in a stash.

## Target layout

Uber's schema-agnostic log platform on ClickHouse went through three schemas:
raw JSON unmarshalled at query time (too slow), one dedicated column per field
(50x faster, but column count grew with the data and the file count killed it),
then grouped `(names, values)` array pairs per value type.

Semcode is at their first schema today: `calls` and `types` are JSON text.
Their second schema's failure mode does not apply here, because semcode's
relationship schema is fixed and small rather than schema-agnostic — so we can
take dedicated typed columns and never hit unbounded column growth. The
`(names, values)` indirection exists to survive arbitrary keys we do not have.

**Forward edges** stay in-row as `List<Utf8>` with a LabelList index, one
column per relationship kind. Already cheap: the data is colocated with a row
the caller has usually fetched anyway.

**Reverse edges** are the query the in-row model cannot serve. "Which rows
contain X in their list" is an index probe across the whole table and grows
with every commit indexed. A dedicated edge table — one row per edge,
`(caller, callee, git_file_hash)`, clustered and indexed on `callee` — makes
reverse lookup a point lookup that stays flat as history grows.

Keep relationship columns to a small fixed set. Every column multiplies
per-fragment metadata across all fragments, which is the object-count surface
that matters on object storage.

## Do lists improve the layout for object storage?

Measured with `strace -e pread64,openat` on the synthetic corpus, one query per
run, `examples/explain-filter.rs`. Requests and bytes are the object-storage
proxies; the full scan reading 13.72 MB against 13 MB on disk validates the
accounting.

| Query | requests | bytes | rows |
| --- | ---: | ---: | ---: |
| fixed overhead (no-op) | 6 | 0.01 MB | 0 |
| BTree probe, `name = 'x'` | 15 | 0.09 MB | 1 |
| LabelList probe, absent symbol | 80 | 1.42 MB | 0 |
| LabelList probe, `array_has_any(calls, [...])` | 101 | 4.29 MB | 4 |
| scan of the `calls` column | 21 | 5.19 MB | 0 |
| full scan, all columns | 264 | 13.72 MB | 144 010 |

Three conclusions, in increasing order of importance.

**Sharding: unchanged.** Fragment count and size come from write batching and
compaction, not column encoding. The list migration does not touch sharding at
all.

**Encoding: a modest win.** 32% less data to egress on any scan, because
dictionary-encoded list values compress better than JSON text. Slightly against
that, a `List<Utf8>` is physically two or three buffers (list offsets, value
offsets, value bytes) where a string column is one, so reading the column is a
few more range GETs.

**The LabelList index is not object-storage friendly.** This is the part that
inverts the earlier plan. A LabelList probe costs 1.42 MB in 80 requests just to
report that a symbol is absent, against 0.09 MB in 15 requests for a BTree probe
— roughly 16x the bytes and 5x the requests for the same "look up one key"
operation. Against simply scanning the `calls` column (5.19 MB in 21 requests),
the LabelList probe reads fewer bytes but issues 5x more requests. Locally that
is a large win, because 4 MB of page-cache hits is free and the probe avoids
decoding 144 k lists: 48 ms of CPU becomes 2.3 ms. On S3, where requests are
what you are billed for and round trips are what you wait on, it is close to a
wash at this scale.

**Therefore the reverse edge table should carry a BTree on `callee`, not a
LabelList on an in-row list.** The two measurements above make the case
directly: 0.09 MB / 15 requests versus 1.42 MB / 80 requests. An edge table
sorted by `callee` also turns the scatter-gather over `functions` into one
contiguous range read, which is the single best access shape for object storage.
The in-row list stays as the forward-edge representation, where it is already
colocated with a row the caller has usually fetched anyway.

## How these scale: LabelList is linear, BTree is not

Repeating the suite on a 10x corpus (12 000 files x 120 functions = 1.44 M
functions, 163 MB of data) settles the scaling question. Generate with
`scripts/gen-bench-corpus.py --files 12000 --funcs-per-file 120 --structs 4096`.

| Query | 1x: 144 k | 10x: 1.44 M | growth |
| --- | ---: | ---: | ---: |
| no-op (absent name) | 6 req / 0.01 MB | 13 / 0.03 | 2.2x / 3x |
| BTree probe `name =` | 15 / 0.09 MB | 35 / 0.19 | 2.3x / **2.1x** |
| LabelList probe, absent | 80 / 1.42 MB | 692 / 14.47 | 8.7x / **10.2x** |
| LabelList probe, present | 101 / 4.29 MB | 774 / 21.56 | 7.7x / 5.0x |
| scan of `calls` column | 21 / 5.19 MB | 88 / 87.93 | 4.2x / 16.9x |
| full scan, all columns | 272 / 13.73 MB | 2553 / 179.27 | 9.4x / 13.1x |

**The LabelList probe grows linearly with table size** — 10x the rows costs
10.2x the bytes. It is not an index probe in the asymptotic sense at all; it is
a scan with a good constant factor. Against scanning the `calls` column
(87.93 MB) it reads 21.56 MB, so it buys roughly 4x, and that ratio holds as the
table grows.

The BTree probe over the same 10x jump costs 2.1x the bytes. That is the shape
an index is supposed to have.

This refutes the earlier guess in this document that the crossover would move
toward LabelList at kernel scale. It does not. Extrapolating to the kernel's
12 M rows — another ~8x — a BTree probe stays well under a megabyte while a
LabelList probe approaches 180 MB per reverse lookup.

LabelList is also expensive to store. Of 448 MB of indices on 163 MB of data at
10x, the two LabelList indices are 111 MB each: the bitmap indices on `calls`
and `types` together are 1.4x the size of all the data they index. (BTree
indices have `page_data.lance` + `page_lookup.lance`; LabelList has
`bitmap_page_lookup.lance`, which is how to tell them apart on disk.)

So the conclusion is stronger than "prefer a BTree on the edge table for object
storage". A reverse edge table with a BTree on `callee` is the only structure
measured here that does not degrade linearly, which makes it the right fix for
plain upstream semcode on local disk as well.

## The reverse edge table

`call_edges` holds one row per reverse edge — `(callee, caller,
caller_file_path, caller_git_file_hash, kind)` — with a BTree on `callee`, and
is rewritten in `callee` order at the end of indexing.

Selective reverse lookup, which is the interactive case:

| | 1x: 144 k fn / 841 k edges | 10x: 1.44 M fn / 8.4 M edges | growth |
| --- | ---: | ---: | ---: |
| LabelList on `functions.calls` | 4.29 MB / 101 req | 22.23 MB / 774 req | 5.2x |
| sorted `call_edges` + BTree | 1.98 MB / 38 req | 2.18 MB / 97 req | **1.1x** |
| LabelList, absent symbol | 1.42 MB / 80 req | 14.47 MB / 692 req | 10.2x |
| sorted edges, absent symbol | — | 0.05 MB / 12 req | flat |

Bytes read are effectively flat across a 10x jump — 1.98 to 2.18 MB — against
5.2x growth for the bitmap index. Answering "not found" costs 0.05 MB instead of
14.47 MB.

**Sorting is the whole trick, and the index is nearly useless without it.** A
BTree yields row ids; if the rows are scattered the reader still fetches a page
each. Measured at 1x with the index built but the table unsorted, a high fan-in
lookup read 132 MB out of a 38 MB table — worse than scanning it. Three details
mattered, each found by measurement:

* Sort the table, not just index it.
* Do the copy-back with ordered range reads. A plain scan of the staging table
  reads fragments in parallel and returns batches in arbitrary order, silently
  undoing the sort — this measured as a 13x regression.
* Do not compact afterwards. Compaction merges across key ranges and cost 2x
  (22 MB to 41 MB on the same query).

**Where it loses.** Returning a large fraction of the table is worse than the
in-row list, because every edge row repeats `caller_file_path` and
`caller_git_file_hash` where `functions` stores one row per caller:

| high fan-in (670 393 rows at 10x) | 1x | 10x |
| --- | ---: | ---: |
| LabelList on `functions.calls` | 8.63 MB | 117.15 MB |
| sorted `call_edges` | 20.11 MB | 363.48 MB |

Both grow with the result set, which is unavoidable when returning 670 k rows,
but the edge table pays a ~3x constant factor for the repeated columns. Two
fixes, neither done: `caller_file_path` can be dropped entirely — the manifest
filter only needs to test whether `caller_git_file_hash` is among the current
hashes, and git blob hashes do not collide across files — and the name columns
want the u32 symbol dictionary. No interactive query should be returning 670 k
callers anyway; `search_functions_regex` still has no result limit.

**Single-host wall time was unchanged at 1x** when the edge table landed:
`callers_git_aware` measured 11.9 / 12.9 / 12.9 ms against `callees_git_aware`
at 11.1 / 11.2 / 11.5 ms, so the reverse lookup was roughly 1.3 ms of an ~11 ms
floor. The edge table changes the slope, not the intercept. That floor turned
out to be entirely manifest generation — see below.

Cost: indexing 1x went from 11.6 s to ~35 s, and sorting 8.4 M edges at 10x took
152 s. That is a post-index step proportional to edge count, and at kernel scale
it needs revisiting — the partitioned sort bounds memory but not time.

## Caching the git manifest

Isolating the edge table exposed the real single-host bottleneck: every
git-aware query called `generate_git_manifest`, which walks the entire git tree
at the commit and builds a path -> blob-oid map. 1200 entries on the synthetic
corpus, roughly 85 k on the kernel, rebuilt from scratch per query.

The tree at a commit is immutable, so one walk per SHA is enough. The manifest
is now cached on `DatabaseManager`, keyed by SHA, and cleared when the workdir
overlay changes since that is merged into the result. It returns
`Arc<HashMap<..>>` rather than a clone — at kernel size, cloning 85 k entries
per query would have replaced most of the cost it removes. Deref coercion means
no call site changed.

| Benchmark | before cache | after cache | |
| --- | ---: | ---: | ---: |
| `chain/callers_git_aware` (leaf/mid/entry) | 11.9 / 12.9 / 12.9 ms | 2.34 / 3.30 / 3.04 ms | −74 to −81% |
| `chain/callees_git_aware` (leaf/mid/entry) | 11.1 / 11.2 / 11.5 ms | 1.69 / 1.79 / 1.60 ms | −84 to −86% |

Cumulatively against the original JSON-plus-uncovered-indices baseline,
`callers_git_aware` went from 33.9 ms to 2.34 ms — 14.5x.

This is the largest single-host win in the effort, and it is a cache, not a
layout change. Worth remembering when the next result points at storage layout:
the floor under every git-aware query was CPU spent re-walking a tree.

## Target deployment: object storage with Rust query nodes

The intended architecture is S3-like storage with stateless Rust query nodes in
front of it. That changes what "fast" means here, and several conclusions above
are stronger under it than they look on a laptop.

**Bytes read and request count are the objective, not local wall time.** The
column projection moved kernel `pread64` from 534 914 to 278 167 but wall time
only from 4.8 s to 4.5 s, because the page cache absorbed the difference. On a
query node with a cold cache that halving is the whole result. Benchmarks
should keep reporting syscall/byte counts, not just timings, since timings on a
warm local box systematically understate the win.

**The reverse edge table stops being optional.** A scan-class `callers` lookup
means pulling the whole `functions` table over the network per query — 11.5 GB
before projection, ~6 GB after. No amount of node-side parallelism makes that
acceptable. A point lookup on an indexed `callee` column is the only shape that
works.

**Query-node caching is a first-class component**, not an optimization. Three
tiers matter, in order: table manifests (re-read constantly, tiny), scalar and
LabelList index files (must fit in node memory or local disk to be useful), and
hot data pages. Index size therefore has a budget: an index too large to cache
on a node degrades to remote reads on every probe. This argues again for a
small fixed set of relationship columns rather than many.

**Version pruning becomes a correctness requirement.** `Prune { older_than: 0 }`
already appears to have cost the kernel database a `lore` data file locally.
With several stateless readers each holding a manifest version for the duration
of a query, retention must exceed the longest query, or readers will 404 on
files pruned underneath them.

**Multi-tenancy.** If nodes serve several repositories or branches, follow the
`_namespace` pattern rather than a table per tenant: shared tables with a
namespace column keep write batches large and the object count bounded, which
is the same reason it was done that way for logs.

**Write path.** Indexing already runs in a separate binary from the query
tools, which fits read-only nodes. What does not fit is `merge_insert`'s
read-modify-write against object storage during indexing. Prefer writing
immutable fragments and compacting, or indexing to local disk and publishing.

## Object storage notes

The Lance format itself suits S3 well: 22 data files for 11.5 GB is ~520 MB
objects, good for range GETs. The problem is access pattern, not format. An
unindexed `callers` lookup reads the whole table — 11.5 GB of egress per query
before projection, ~6 GB after. Fixing the reverse-edge path is the
prerequisite for any object-storage story.

`Prune { older_than: 0 }` in `optimize_single_table` is the most likely cause
of the kernel database's missing `lore` data file, and concurrent readers on
distributed storage make that failure more likely.

## Remaining work

The `List<Utf8>` migration is done: `functions.calls`, `functions.types` and
`types.types` are real Arrow lists with LabelList indices, all read sites are
converted, and both `calls LIKE '%"x"%'` filters are now `array_has_any`.

Priorities assume **storage is cheap and query speed is what matters**. That
licenses duplicating data freely — extra sort orders, materialized snapshots,
denormalized columns — and Lance being columnar means an unread column costs
nothing at query time as long as queries project.

Next, roughly in value order:

1. **Materialize a HEAD snapshot.** Every git-aware query still reads rows for
   all indexed commits and filters by manifest, so cost scales with history
   rather than tree size. With storage free this is just a `current_functions`
   / `current_call_edges` pair rewritten per index run. On the kernel, where
   history dwarfs the tree, this is the largest remaining win.
2. **`git_commits` is still schema #1.** `symbols`, `files` and `parent_sha`
   are JSON arrays, so "which commits touched X" is a LIKE scan — and it
   materializes the `diff` column on every row it touches. Same list treatment,
   plus a reverse `commit_symbols` table sorted by symbol.
3. **Trim what `call_edges` reads.** `caller_file_path` can go entirely: the
   manifest filter only needs to know whether `caller_git_file_hash` is among
   the current hashes, and git blob hashes do not collide across files. This is
   a query-speed fix, not a storage one — those repeated columns are why a high
   fan-in lookup reads 363 MB.
4. **Widen `symbol_filename` into a covering index.** `(symbol, kind,
   file_path, line_start, git_file_hash)` answers "where is X defined" without
   touching the wide `functions` table. Cheap now that extra columns are free.
5. **Second sort order on edges.** `call_edges` sorted by `callee` serves
   reverse lookups; a `caller`-sorted copy would serve forward traversal
   contiguously too. Pure duplication, which is now an acceptable trade.
6. **Drop the `line_start`/`line_end` BTrees.** Not for space — they serve no
   query, and index files still compete for page cache.
7. **Symbol dictionary.** u32 ids instead of repeated name strings. Reframed:
   the win is bytes *read* per lookup and a smaller index working set, not
   bytes stored.
8. **Namespace column** for multi-tenant query nodes.

Also outstanding, found while measuring:

* Index coverage is invisible without `index_stats()`. Whatever ships next
  should assert coverage after indexing rather than trusting that
  `create_index` succeeded — that is how this went unnoticed.

* The file survey was the last full scan on the git-aware path; it is now
  served by `call_edges`, which also carries `kind = "type_use"` edges from
  `functions.types` and `types.types`. That took survey from 0.53s to 0.27s
  and grew the edge table only 17%, since type references collapse hard.
  Referrer identity lost line-level granularity in the process: a
  declaration and definition of one symbol in one file now count once.

* `src/search.rs` and `src/database/search.rs` are separate query paths — the
  CLI uses the former, the DB API, MCP and benchmarks the latter. Optimising
  one while measuring the other cost a full cycle here.
* `show_callchain_to_writer` (unbounded) vs the CLI's
  `show_callchain_with_limits` (`calls_limit=15`): 28.5 s vs 1.1 s on the same
  symbol.
* `search_functions_regex` has no result limit where `search_functions_fuzzy`
  has one. Adding it is a prerequisite for measuring anything at large result
  counts.
