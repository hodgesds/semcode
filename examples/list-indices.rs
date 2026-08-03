// SPDX-License-Identifier: MIT OR Apache-2.0
//! Print the indices Lance actually created for a table.
//!
//! Index creation in `SchemaManager` logs failures at debug level, so a
//! silently-missing index looks identical to a working one from the outside.
//!
//! Usage: cargo run --example list-indices -- <db_path> <table>

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let db_path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: list-indices <db_path> <table>"))?;
    let table_name = args.next().unwrap_or_else(|| "functions".to_string());

    let connection = lancedb::connect(&db_path).execute().await?;
    let table = connection.open_table(&table_name).execute().await?;

    println!("{table_name}: {} rows", table.count_rows(None).await?);
    for idx in table.list_indices().await? {
        print!(
            "  {} on {:?} -> {:?}",
            idx.name, idx.columns, idx.index_type
        );
        // Rows appended after an index was built are not covered by it until
        // the table is optimized, and the planner falls back to a scan.
        match table.index_stats(&idx.name).await? {
            Some(s) => println!(
                "  indexed={} unindexed={}",
                s.num_indexed_rows, s.num_unindexed_rows
            ),
            None => println!("  (no stats)"),
        }
    }

    Ok(())
}
