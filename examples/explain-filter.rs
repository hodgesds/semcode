// SPDX-License-Identifier: MIT OR Apache-2.0
//! Print the physical plan and wall time for a filter against a table.
//!
//! Used to confirm whether a scalar index is actually being used: an index
//! probe shows up as `ScalarIndexQuery`/`MaterializeIndex`, a scan as
//! `LanceRead` with the filter pushed into the reader.
//!
//! Usage: cargo run --example explain-filter -- <db_path> <table> <filter>

use anyhow::Result;
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let db_path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: explain-filter <db_path> <table> <filter>"))?;
    let table_name = args.next().unwrap();
    let filter = args.next().unwrap();

    // Columns to project, comma separated; empty means all of them.
    let columns = args.next().unwrap_or_default();
    // Number of query executions.  Use 1 when counting syscalls under strace,
    // so the measurement covers exactly one query.
    let runs: usize =
        args.next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(if std::env::var("EXPLAIN").is_ok() {
                2
            } else {
                1
            });

    let connection = lancedb::connect(&db_path).execute().await?;
    let table = connection.open_table(&table_name).execute().await?;

    let build = |f: String| {
        let q = table.query().only_if(f);
        if columns.is_empty() {
            q
        } else {
            q.select(lancedb::query::Select::Columns(
                columns.split(',').map(str::to_string).collect(),
            ))
        }
    };

    if std::env::var("EXPLAIN").is_ok() {
        let plan = build(filter.clone()).explain_plan(true).await?;
        println!("--- plan for: {filter}\n{plan}");
    }

    for run in 0..runs {
        let start = std::time::Instant::now();
        let batches = build(filter.clone())
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        println!("run {run}: {:?}  {rows} rows", start.elapsed());
    }

    Ok(())
}
