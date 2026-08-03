// SPDX-License-Identifier: MIT OR Apache-2.0
//! Query benchmarks against a pre-built database.
//!
//! Unlike the indexing benchmarks, these cannot be self-referential: semcode's
//! own database holds a few hundred functions, and a full table scan of a few
//! hundred rows is fast no matter how badly the layout is chosen. Query costs
//! only become visible on a real corpus, so these benchmarks run against a
//! database you point them at:
//!
//! ```bash
//! SEMCODE_BENCH_DB=~/linux cargo bench --bench query
//! ```
//!
//! Without `SEMCODE_BENCH_DB` the whole suite is skipped.
//!
//! | Variable | Default | Meaning |
//! | --- | --- | --- |
//! | `SEMCODE_BENCH_DB` | *(unset — skips)* | `.semcode.db`, or a directory containing one |
//! | `SEMCODE_BENCH_REPO` | the database's parent directory | git repository the database was built from |
//! | `SEMCODE_BENCH_SYMBOL` | `mutex_lock` | function name used for exact and caller lookups |
//! | `SEMCODE_BENCH_SUBSTRING` | `alloc` | pattern used for substring and regex searches |
//!
//! The benchmarks deliberately call the non-git-aware lookups. Git-aware
//! variants layer manifest filtering on top of these same queries, so the
//! plain versions isolate the database cost that the layout controls.
//!
//! Setup opens the database read-only and never calls `create_tables`, so a
//! database with an unrelated damaged table can still be measured.

use criterion::{criterion_group, criterion_main, Criterion};
use semcode::{process_database_path, DatabaseManager};
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Scan-bound queries are opt-in so the default suite stays under a minute.
fn scans_enabled() -> bool {
    std::env::var("SEMCODE_BENCH_SCANS").is_ok()
}

/// Criterion defaults (100 samples, 3s warm-up) blow the one-minute budget on
/// their own. Every group uses this instead.
fn configure(group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>) {
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_secs(3));
}

/// Everything the benchmarks need, or `None` when `SEMCODE_BENCH_DB` is unset.
struct Corpus {
    db: Arc<DatabaseManager>,
    runtime: Runtime,
    git_sha: String,
    symbol: String,
    substring: String,
    /// Representative symbols at increasing call-graph depth, discovered from
    /// the generated corpus.json when present. Chain cost depends heavily on
    /// where in the graph you start, so one symbol cannot characterise it.
    chain_symbols: Vec<(String, String)>,
}

/// Read a string field from the corpus manifest written by
/// scripts/gen-bench-corpus.py, if the database was built from one.
fn manifest_symbol(repo: &str, field: &str) -> Option<String> {
    let text = std::fs::read_to_string(PathBuf::from(repo).join("corpus.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get(field)?.as_str().map(|s| s.to_string())
}

fn load_corpus() -> Option<Corpus> {
    let Ok(raw_db) = std::env::var("SEMCODE_BENCH_DB") else {
        eprintln!(
            "skipping query benchmarks: set SEMCODE_BENCH_DB to a prebuilt database, e.g.\n  \
             SEMCODE_BENCH_DB=~/linux cargo bench --bench query"
        );
        return None;
    };

    let db_path = process_database_path(Some(&raw_db), None);
    let repo = std::env::var("SEMCODE_BENCH_REPO").unwrap_or_else(|_| {
        PathBuf::from(&db_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string())
    });

    let runtime = Runtime::new().expect("failed to build tokio runtime");
    let db = match runtime.block_on(DatabaseManager::new(&db_path, repo.clone())) {
        Ok(db) => Arc::new(db),
        Err(e) => {
            eprintln!("skipping query benchmarks: cannot open {db_path} ({e})");
            return None;
        }
    };

    let symbol = env_or("SEMCODE_BENCH_SYMBOL", "mutex_lock");
    let substring = env_or("SEMCODE_BENCH_SUBSTRING", "alloc");

    match runtime.block_on(db.find_function(&symbol)) {
        Ok(Some(_)) => {}
        Ok(None) => {
            eprintln!(
                "skipping query benchmarks: function {symbol:?} not found in {db_path}; \
                 set SEMCODE_BENCH_SYMBOL to one that exists"
            );
            return None;
        }
        Err(e) => {
            eprintln!("skipping query benchmarks: lookup of {symbol:?} failed ({e})");
            return None;
        }
    }

    let git_sha = match semcode::get_git_sha(&repo) {
        Ok(Some(sha)) => sha,
        _ => {
            eprintln!("skipping query benchmarks: {repo} is not a git repository");
            return None;
        }
    };

    // Chain benchmarks need starting points at different depths. Fall back to
    // the single configured symbol when the database was not built from a
    // generated corpus.
    let mut chain_symbols = Vec::new();
    for (label, field) in [
        ("leaf", "leaf_symbol"),
        ("mid", "mid_symbol"),
        ("entry", "entry_symbol"),
    ] {
        if let Some(name) = manifest_symbol(&repo, field) {
            chain_symbols.push((label.to_string(), name));
        }
    }
    if chain_symbols.is_empty() {
        chain_symbols.push(("symbol".to_string(), symbol.clone()));
    }

    eprintln!("query corpus: {db_path} (repo {repo}), symbol {symbol:?}, substring {substring:?}");
    eprintln!("chain symbols: {chain_symbols:?}");

    Some(Corpus {
        db,
        runtime,
        git_sha,
        symbol,
        substring,
        chain_symbols,
    })
}

/// Run each query once before measuring, reporting wall time and result count.
///
/// A scan-bound query on a large database can take long enough that criterion's
/// own progress output is the first feedback you get minutes in. This prints
/// the shape of every query up front, and makes an accidentally empty result
/// set — which would measure the scan but not the row handling — obvious.
fn probe(corpus: &Corpus) {
    let Corpus {
        db,
        runtime,
        git_sha,
        symbol,
        substring,
        chain_symbols,
    } = corpus;

    macro_rules! timed {
        ($label:expr, $count:expr, $call:expr) => {{
            let start = Instant::now();
            let result = runtime.block_on($call);
            let elapsed = start.elapsed();
            match result {
                Ok(value) => {
                    let n: usize = $count(&value);
                    eprintln!("  probe {:<28} {:>9.1?}  {} results", $label, elapsed, n);
                }
                Err(e) => eprintln!("  probe {:<28} FAILED: {e}", $label),
            }
        }};
    }

    eprintln!("probing queries (one call each):");
    timed!(
        "find_function",
        |v: &Option<_>| v.iter().count(),
        db.find_function(symbol)
    );
    timed!(
        "find_type",
        |v: &Option<_>| v.iter().count(),
        db.find_type(symbol)
    );
    timed!(
        "get_function_callees",
        |v: &Vec<_>| v.len(),
        db.get_function_callees(symbol)
    );
    timed!(
        "get_function_callers",
        |v: &Vec<_>| v.len(),
        db.get_function_callers(symbol)
    );
    timed!(
        "search_functions_fuzzy",
        |v: &Vec<_>| v.len(),
        db.search_functions_fuzzy(substring)
    );
    // search_functions_regex is excluded unless SEMCODE_BENCH_SCANS is set: a
    // single call takes 375s on a kernel-sized database and does not finish in
    // a usable time even on the 144k-function synthetic corpus. Measuring it
    // is opt-in until the query itself is fixed.
    if scans_enabled() {
        timed!(
            "search_functions_regex",
            |v: &Vec<_>| v.len(),
            db.search_functions_regex(substring)
        );
    }

    // Recursive chain queries. show_callchain_to_writer expands every caller
    // at every level; the query CLI instead uses its own bounded walk with a
    // per-level cap, so the library version is far more expensive than what
    // users actually run (28.5s vs 1.1s on this corpus). Keep it opt-in and
    // measure the per-level primitives below instead.
    if !scans_enabled() {
        return;
    }
    for (label, name) in chain_symbols {
        let start = Instant::now();
        let mut sink = std::io::sink();
        let result = runtime.block_on(semcode::callchain::show_callchain_to_writer(
            db, name, &mut sink, git_sha,
        ));
        let elapsed = start.elapsed();
        match result {
            Ok(()) => eprintln!(
                "  probe {:<28} {:>9.1?}",
                format!("callchain[{label}]"),
                elapsed
            ),
            Err(e) => eprintln!("  probe callchain[{label}] FAILED: {e}"),
        }

        // find_all_paths enumerates every route to the target. On a layered
        // graph that is combinatorial and does not finish in a usable time,
        // so it stays opt-in alongside the other unbounded queries.
        if scans_enabled() {
            let start = Instant::now();
            let mut sink = std::io::sink();
            let result = runtime.block_on(semcode::callchain::find_all_paths_to_writer(
                db, name, &mut sink, git_sha,
            ));
            let elapsed = start.elapsed();
            match result {
                Ok(()) => eprintln!(
                    "  probe {:<28} {:>9.1?}",
                    format!("all_paths[{label}]"),
                    elapsed
                ),
                Err(e) => eprintln!("  probe all_paths[{label}] FAILED: {e}"),
            }
        }
    }
}

fn bench_queries(c: &mut Criterion) {
    let Some(corpus) = load_corpus() else {
        return;
    };
    probe(&corpus);

    let Corpus {
        db,
        runtime,
        git_sha,
        symbol,
        substring,
        chain_symbols,
    } = &corpus;

    // Point lookups: served by the BTree index on `name`, so these should stay
    // flat as the corpus grows.
    let mut group = c.benchmark_group("lookup");
    configure(&mut group);
    group.bench_function("find_function", |b| {
        b.iter(|| black_box(runtime.block_on(db.find_function(symbol)).expect("lookup")));
    });
    group.bench_function("find_type", |b| {
        b.iter(|| black_box(runtime.block_on(db.find_type(symbol)).expect("lookup")));
    });
    group.bench_function("get_function_callees", |b| {
        b.iter(|| {
            black_box(
                runtime
                    .block_on(db.get_function_callees(symbol))
                    .expect("callees"),
            )
        });
    });
    group.finish();

    // Recursive chain traversal, the queries whose cost compounds: every level
    // issues its own caller/callee lookup, so a layout change that speeds up
    // one lookup shows up here multiplied by the number of nodes visited.
    // Per-level chain primitives. A call chain is N of these, so a layout
    // change that speeds one up shows here multiplied by the nodes visited.
    let mut group = c.benchmark_group("chain");
    configure(&mut group);
    for (label, name) in chain_symbols {
        group.bench_function(format!("callers_git_aware/{label}"), |b| {
            b.iter(|| {
                black_box(
                    runtime
                        .block_on(db.get_function_callers_git_aware(name, git_sha))
                        .expect("callers"),
                )
            });
        });
        group.bench_function(format!("callees_git_aware/{label}"), |b| {
            b.iter(|| {
                black_box(
                    runtime
                        .block_on(db.get_function_callees_git_aware(name, git_sha))
                        .expect("callees"),
                )
            });
        });
        if scans_enabled() {
            group.bench_function(format!("callchain/{label}"), |b| {
                b.iter(|| {
                    let mut sink = std::io::sink();
                    runtime
                        .block_on(semcode::callchain::show_callchain_to_writer(
                            db, name, &mut sink, git_sha,
                        ))
                        .expect("callchain");
                });
            });
        }
        if scans_enabled() {
            group.bench_function(format!("all_paths/{label}"), |b| {
                b.iter(|| {
                    let mut sink = std::io::sink();
                    runtime
                        .block_on(semcode::callchain::find_all_paths_to_writer(
                            db, name, &mut sink, git_sha,
                        ))
                        .expect("all_paths");
                });
            });
        }
    }
    group.finish();

    // Scan-bound queries: no index can serve a leading-wildcard LIKE, so these
    // grow with the size of the table. Opt-in, because on a kernel-sized
    // database a single iteration of search_functions_regex takes minutes and
    // criterion's ten-sample minimum turns that into an hour.
    if !scans_enabled() {
        eprintln!("skipping scan group: set SEMCODE_BENCH_SCANS=1 to include it");
        return;
    }
    let mut group = c.benchmark_group("scan");
    configure(&mut group);

    group.bench_function("get_function_callers", |b| {
        b.iter(|| {
            black_box(
                runtime
                    .block_on(db.get_function_callers(symbol))
                    .expect("callers"),
            )
        });
    });
    group.bench_function("search_functions_fuzzy", |b| {
        b.iter(|| {
            black_box(
                runtime
                    .block_on(db.search_functions_fuzzy(substring))
                    .expect("fuzzy"),
            )
        });
    });
    group.bench_function("search_functions_regex", |b| {
        b.iter(|| {
            black_box(
                runtime
                    .block_on(db.search_functions_regex(substring))
                    .expect("regex"),
            )
        });
    });
    group.finish();
}

criterion_group!(benches, bench_queries);
criterion_main!(benches);
