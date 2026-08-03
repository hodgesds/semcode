// SPDX-License-Identifier: MIT OR Apache-2.0
//! Run `OptimizeAction::Index` on a table so existing indices cover the rows
//! that were appended after the index was created.
//!
//! Deliberately does not compact or prune -- this isolates index coverage from
//! the other two things `optimize_single_table` does at the same time.
//!
//! Usage: cargo run --example optimize-indices -- <db_path> <table>...

use anyhow::Result;
use lancedb::table::OptimizeAction;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let db_path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: optimize-indices <db_path> <table>..."))?;
    let tables: Vec<String> = args.collect();

    let connection = lancedb::connect(&db_path).execute().await?;

    // SORT_EDGES=1 runs the full post-index step: sort call_edges by callee,
    // then refresh index coverage.  Indexing does this automatically now; this
    // is the migration path for a database built before that, since the BTree
    // on callee is near-useless while the rows it points at are scattered.
    if std::env::var("SORT_EDGES").is_ok() {
        let start = std::time::Instant::now();
        let db = semcode::DatabaseManager::new(&db_path, ".".to_string()).await?;
        db.optimize_scalar_indices().await?;
        println!("post-index step took {:?}", start.elapsed());
    }

    for table_name in &tables {
        let table = connection.open_table(table_name).execute().await?;

        // COMPACT=1 also merges data fragments.  Deliberately opt-in and
        // separate from the index step: compaction rewrites data files, the
        // index step only builds coverage over rows already written.
        if std::env::var("COMPACT").is_ok() {
            let start = std::time::Instant::now();
            table
                .optimize(OptimizeAction::Compact {
                    options: Default::default(),
                    remap_options: None,
                })
                .await?;
            println!("{table_name}: compact took {:?}", start.elapsed());
        }

        let start = std::time::Instant::now();
        table
            .optimize(OptimizeAction::Index(Default::default()))
            .await?;
        println!("{table_name}: index optimize took {:?}", start.elapsed());

        for idx in table.list_indices().await? {
            if let Some(s) = table.index_stats(&idx.name).await? {
                println!(
                    "  {} indexed={} unindexed={}",
                    idx.name, s.num_indexed_rows, s.num_unindexed_rows
                );
            }
        }
    }

    Ok(())
}
