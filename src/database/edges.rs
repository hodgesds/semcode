// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reverse relationship edges, one row per edge.
//!
//! Forward edges ("what does X call") live in-row on `functions.calls`, where
//! they are colocated with a row the caller has usually already fetched.
//! Reverse edges ("who calls X") cannot be served that way: asking which rows
//! contain X in their list is a question about the whole table, and it stays
//! that way however the list is encoded.
//!
//! Measured on the synthetic corpus, a LabelList probe over `functions.calls`
//! grows linearly with the table -- 10x the rows costs 10.2x the bytes read --
//! because a bitmap index is a scan with a good constant factor rather than a
//! probe. A BTree lookup over the same 10x jump costs 2.1x. This table exists
//! so reverse lookups get the second curve instead of the first.
//!
//! See docs/query-performance.md for the measurements.

use anyhow::Result;
use arrow::array::{ArrayRef, RecordBatch, StringArray, StringBuilder};
use futures::TryStreamExt;
use lancedb::connection::Connection;
use lancedb::query::{ExecutableQuery, QueryBase};
use std::sync::Arc;

use crate::database::connection::OPTIMAL_BATCH_SIZE;
use crate::database::get_column;
use crate::types::FunctionInfo;

/// Relationship kinds stored in the `kind` column.
///
/// A discriminator column rather than a table per relationship: `kind` is
/// low-cardinality so it costs almost nothing, and every extra table
/// multiplies per-fragment metadata across the whole database.
pub const KIND_CALL: &str = "call";
/// A function or type referencing a type. Both `functions.types` and
/// `types.types` produce these; the referring entity is identified by
/// (caller, caller_file_path) either way.
pub const KIND_TYPE_USE: &str = "type_use";

/// One reverse edge: `caller` references `callee`.
#[derive(Debug, Clone)]
pub struct CallEdge {
    pub callee: String,
    pub caller: String,
    pub caller_file_path: String,
    /// Every blob hash this caller was seen in. Git-aware filtering asks
    /// whether any of them is present at the commit; the caller's path is not
    /// stored, since a hash match already means that content is present.
    pub caller_git_file_hashes: Vec<String>,
    /// [`KIND_CALL`] or [`KIND_TYPE_USE`].
    pub kind: String,
}

pub struct CallEdgeStore {
    connection: Connection,
}

/// Key-range split points for the sorted rewrite.
///
/// One partition per leading character across the range C and Rust identifiers
/// occupy, plus open-ended partitions at each end so nothing is dropped.
fn partition_bounds() -> Vec<String> {
    let mut bounds = vec![String::new()];
    for c in ('0'..='9').chain('A'..='Z').chain(std::iter::once('_')) {
        bounds.push(c.to_string());
    }
    for c in 'a'..='z' {
        bounds.push(c.to_string());
    }
    bounds
}

impl CallEdgeStore {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }

    /// Derive call edges from a batch of functions and store them.
    ///
    /// A function with no `calls` contributes nothing; a null and an empty
    /// list are equivalent here, since neither produces edges.
    pub async fn insert_from_functions(&self, functions: &[FunctionInfo]) -> Result<()> {
        let mut edges = Vec::new();
        for func in functions {
            let mut push = |callee: &String, kind: &str| {
                edges.push(CallEdge {
                    callee: callee.clone(),
                    caller: func.name.clone(),
                    caller_file_path: func.file_path.clone(),
                    caller_git_file_hashes: vec![func.git_file_hash.clone()],
                    kind: kind.to_string(),
                });
            };
            for callee in func.calls.iter().flatten() {
                push(callee, KIND_CALL);
            }
            for referenced in func.types.iter().flatten() {
                push(referenced, KIND_TYPE_USE);
            }
        }

        self.insert_batch(edges).await
    }

    /// Type-to-type references, so "what references this type" is a lookup
    /// rather than a scan of the types table.
    pub async fn insert_from_types(&self, types: &[crate::types::TypeInfo]) -> Result<()> {
        let mut edges = Vec::new();
        for type_info in types {
            for referenced in type_info.types.iter().flatten() {
                edges.push(CallEdge {
                    callee: referenced.clone(),
                    caller: type_info.name.clone(),
                    caller_file_path: type_info.file_path.clone(),
                    caller_git_file_hashes: vec![type_info.git_file_hash.clone()],
                    kind: KIND_TYPE_USE.to_string(),
                });
            }
        }

        self.insert_batch(edges).await
    }

    pub async fn insert_batch(&self, edges: Vec<CallEdge>) -> Result<()> {
        if edges.is_empty() {
            return Ok(());
        }

        let table = self.connection.open_table("call_edges").execute().await?;

        for chunk in edges.chunks(OPTIMAL_BATCH_SIZE) {
            self.insert_chunk(&table, chunk).await?;
        }

        Ok(())
    }

    async fn insert_chunk(&self, table: &lancedb::table::Table, edges: &[CallEdge]) -> Result<()> {
        // Deduplicate in memory first, the same way SymbolFilenameStore does:
        // a function calling the same callee twice is one edge, and doing this
        // here keeps merge_insert's workload down.
        use std::collections::HashSet;

        let mut seen = HashSet::new();
        let mut callee_builder = StringBuilder::new();
        let mut caller_builder = StringBuilder::new();
        let mut path_builder = StringBuilder::new();
        let mut hashes_builder = arrow::array::ListBuilder::new(StringBuilder::new());
        let mut kind_builder = StringBuilder::new();

        // Indexing streams batches, so it writes one row per version here and
        // the sort pass collapses them into one row per referring entity.
        for edge in edges {
            let hash = edge.caller_git_file_hashes.first().map(String::as_str);
            let key = (
                edge.callee.as_str(),
                edge.caller.as_str(),
                edge.caller_file_path.as_str(),
                hash,
                edge.kind.as_str(),
            );
            if !seen.insert(key) {
                continue;
            }
            callee_builder.append_value(&edge.callee);
            caller_builder.append_value(&edge.caller);
            path_builder.append_value(&edge.caller_file_path);
            crate::database::append_string_list(
                &mut hashes_builder,
                Some(&edge.caller_git_file_hashes),
            );
            kind_builder.append_value(&edge.kind);
        }

        if seen.is_empty() {
            return Ok(());
        }

        let batch = RecordBatch::try_from_iter(vec![
            ("callee", Arc::new(callee_builder.finish()) as ArrayRef),
            ("caller", Arc::new(caller_builder.finish()) as ArrayRef),
            (
                "caller_file_path",
                Arc::new(path_builder.finish()) as ArrayRef,
            ),
            (
                "caller_git_file_hashes",
                Arc::new(hashes_builder.finish()) as ArrayRef,
            ),
            ("kind", Arc::new(kind_builder.finish()) as ArrayRef),
        ])?;

        // Plain append, not merge_insert: a merge keyed on (callee, caller)
        // would overwrite the hash list and lose every version but the last,
        // and the hash cannot be part of the key now that it is a list.
        // Duplicates are fine because the sort pass is authoritative -- it
        // groups by (callee, caller) and unions the hashes, so running it
        // again over re-indexed data is idempotent.
        table.add(batch).execute().await?;

        Ok(())
    }

    /// Rewrite the table in `callee` order.
    ///
    /// The BTree on `callee` yields row ids, but if the matching rows are
    /// scattered the reader still fetches a page per row. Measured on the
    /// synthetic corpus, a high fan-in lookup against an unsorted edge table
    /// read 132 MB out of a 38 MB table -- worse than scanning it. Sorted, the
    /// same query reads 9.8 MB and a low fan-in lookup reads 0.17 MB.
    ///
    /// Sorting happens in key-range partitions rather than all at once, so
    /// peak memory is bounded by the largest partition instead of the whole
    /// edge set. Each partition read is served by the BTree.
    pub async fn sort_by_callee(&self) -> Result<()> {
        let table = self.connection.open_table("call_edges").execute().await?;
        let total = table.count_rows(None).await?;
        if total == 0 {
            return Ok(());
        }

        let schema = table.schema().await?;
        let _ = self.connection.drop_table("call_edges_sorting", &[]).await;
        let dst = self
            .connection
            .create_empty_table("call_edges_sorting", schema)
            .execute()
            .await?;

        // Partition boundaries over the printable ASCII range that C
        // identifiers live in. Symbols outside it land in the first or last
        // partition, which is correct if not perfectly balanced.
        let bounds = partition_bounds();
        let mut ranges: Vec<(String, Option<String>)> = bounds
            .windows(2)
            .map(|w| (w[0].clone(), Some(w[1].clone())))
            .collect();
        // Anything sorting at or above the last bound, so no symbol is lost.
        ranges.push((bounds.last().cloned().unwrap_or_default(), None));

        let mut written = 0usize;
        for (lo, hi) in &ranges {
            let mut rows = self.read_range(&table, lo, hi.as_deref()).await?;
            if rows.is_empty() {
                continue;
            }
            rows.sort_unstable();
            written += rows.len();
            self.append_sorted(&dst, &rows).await?;
        }

        // Every source row must reach staging. A dropped partition would
        // silently lose callers, which is the worst failure mode here.
        if written != total {
            anyhow::bail!("staging call_edges lost rows: {written} of {total}");
        }

        // Copy back over the original rather than renaming: rename_table is
        // NotSupported on the OSS listing backend, and dropping the original
        // first would destroy the table if the swap then failed.  Overwrite
        // replaces the contents in one commit, so `call_edges` is never
        // missing.
        //
        // The copy-back re-reads by key range rather than scanning the temp
        // table, because a plain scan reads fragments in parallel and hands
        // back batches in arbitrary order -- which silently undoes the sort.
        // Ranges are read in key order, so what lands on disk is ordered.
        //
        // This leaves one fragment per partition, which is the shape we want:
        // a lookup for a single callee falls entirely inside one fragment and
        // opens one file.  Compacting afterwards measurably hurt, since it
        // merges across key ranges.
        // Partitions are accumulated into larger ordered flushes rather than
        // written one fragment apiece: order is what makes matches contiguous,
        // but a probe still pays per fragment it has to consider, so fewer and
        // larger is better as long as the order across them is preserved.
        const FLUSH_ROWS: usize = 500_000;
        let mut first = true;
        let mut buffer: Vec<(String, String, String, String, Vec<String>)> = Vec::new();
        let mut collapsed = 0usize;

        for (lo, hi) in &ranges {
            let rows = self.read_range(&dst, lo, hi.as_deref()).await?;
            if rows.is_empty() {
                continue;
            }

            // Collapse the versions of each edge into one row.  Every row for
            // a given callee falls in one partition, so grouping per partition
            // is complete.  A BTreeMap also yields the sort order for free.
            let mut grouped: std::collections::BTreeMap<
                (String, String, String, String),
                std::collections::BTreeSet<String>,
            > = std::collections::BTreeMap::new();
            for (callee, caller, path, kind, hash) in rows {
                grouped
                    .entry((callee, caller, path, kind))
                    .or_default()
                    .insert(hash);
            }

            collapsed += grouped.len();
            for ((callee, caller, path, kind), hashes) in grouped {
                buffer.push((callee, caller, path, kind, hashes.into_iter().collect()));
            }

            if buffer.len() >= FLUSH_ROWS {
                self.flush_buffer(&table, &mut buffer, &mut first).await?;
            }
        }
        if !buffer.is_empty() {
            self.flush_buffer(&table, &mut buffer, &mut first).await?;
        }

        // Row count drops here by design -- versions of an edge collapse into
        // one row -- so the check is against the collapsed count, not `total`.
        let final_count = table.count_rows(None).await?;
        if final_count != collapsed {
            anyhow::bail!("sorted call_edges has {final_count} rows, expected {collapsed}");
        }

        self.connection
            .drop_table("call_edges_sorting", &[])
            .await?;

        tracing::info!("Sorted {} call edges by callee", total);
        Ok(())
    }

    async fn read_range(
        &self,
        table: &lancedb::table::Table,
        lo: &str,
        hi: Option<&str>,
    ) -> Result<Vec<(String, String, String, String, String)>> {
        let filter = match hi {
            Some(hi) => format!("callee >= '{lo}' AND callee < '{hi}'"),
            None => format!("callee >= '{lo}'"),
        };
        let batches = table
            .query()
            .only_if(filter)
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;

        let mut rows = Vec::new();
        for batch in &batches {
            let callee: &StringArray = get_column(batch, "callee")?;
            let caller: &StringArray = get_column(batch, "caller")?;
            let path: &StringArray = get_column(batch, "caller_file_path")?;
            let kind: &StringArray = get_column(batch, "kind")?;
            let hashes: &arrow::array::ListArray = get_column(batch, "caller_git_file_hashes")?;
            for i in 0..batch.num_rows() {
                let list = crate::database::read_string_list(hashes, i).unwrap_or_default();
                for hash in list {
                    rows.push((
                        callee.value(i).to_string(),
                        caller.value(i).to_string(),
                        path.value(i).to_string(),
                        kind.value(i).to_string(),
                        hash,
                    ));
                }
            }
        }
        Ok(rows)
    }

    /// Write one accumulated run, overwriting on the first flush so the table
    /// is replaced rather than appended to.
    async fn flush_buffer(
        &self,
        table: &lancedb::table::Table,
        buffer: &mut Vec<(String, String, String, String, Vec<String>)>,
        first: &mut bool,
    ) -> Result<()> {
        let mode = if *first {
            lancedb::table::AddDataMode::Overwrite
        } else {
            lancedb::table::AddDataMode::Append
        };
        self.write_rows(table, buffer, mode).await?;
        *first = false;
        buffer.clear();
        Ok(())
    }

    /// Append to the staging table, one row per version (not yet collapsed).
    async fn append_sorted(
        &self,
        dst: &lancedb::table::Table,
        rows: &[(String, String, String, String, String)],
    ) -> Result<()> {
        let expanded: Vec<(String, String, String, String, Vec<String>)> = rows
            .iter()
            .map(|(callee, caller, path, kind, hash)| {
                (
                    callee.clone(),
                    caller.clone(),
                    path.clone(),
                    kind.clone(),
                    vec![hash.clone()],
                )
            })
            .collect();
        self.write_rows(dst, &expanded, lancedb::table::AddDataMode::Append)
            .await
    }

    async fn write_rows(
        &self,
        dst: &lancedb::table::Table,
        rows: &[(String, String, String, String, Vec<String>)],
        mode: lancedb::table::AddDataMode,
    ) -> Result<()> {
        let mut callee_b = StringBuilder::new();
        let mut caller_b = StringBuilder::new();
        let mut path_b = StringBuilder::new();
        let mut hash_b = arrow::array::ListBuilder::new(StringBuilder::new());
        let mut kind_b = StringBuilder::new();

        for (callee, caller, path, kind, hashes) in rows {
            callee_b.append_value(callee);
            caller_b.append_value(caller);
            path_b.append_value(path);
            crate::database::append_string_list(&mut hash_b, Some(hashes));
            kind_b.append_value(kind);
        }

        let batch = RecordBatch::try_from_iter(vec![
            ("callee", Arc::new(callee_b.finish()) as ArrayRef),
            ("caller", Arc::new(caller_b.finish()) as ArrayRef),
            ("caller_file_path", Arc::new(path_b.finish()) as ArrayRef),
            (
                "caller_git_file_hashes",
                Arc::new(hash_b.finish()) as ArrayRef,
            ),
            ("kind", Arc::new(kind_b.finish()) as ArrayRef),
        ])?;

        dst.add(batch).mode(mode).execute().await?;
        Ok(())
    }

    /// Every edge pointing at `callee`. A BTree point lookup on the sorted key.
    /// Every edge pointing at any of `callees`, in one query.
    ///
    /// The file survey asks about every symbol defined in a file at once.
    /// Doing that as one range-scan per key over the sorted table beats
    /// scanning `functions` and `types` end to end, and unlike a scan the cost
    /// tracks the number of symbols asked about rather than the table size.
    pub async fn find_referrers(&self, callees: &[String]) -> Result<Vec<CallEdge>> {
        if callees.is_empty() {
            return Ok(Vec::new());
        }

        let table = self.connection.open_table("call_edges").execute().await?;
        let mut edges = Vec::new();

        // Chunked so the filter string stays a sane size on a file that
        // defines hundreds of symbols.
        for chunk in callees.chunks(256) {
            let list = chunk
                .iter()
                .map(|c| format!("'{}'", c.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");

            let batches = table
                .query()
                .only_if(format!("callee IN ({list})"))
                .select(lancedb::query::Select::Columns(vec![
                    "callee".to_string(),
                    "caller".to_string(),
                    "caller_file_path".to_string(),
                    "caller_git_file_hashes".to_string(),
                    "kind".to_string(),
                ]))
                .execute()
                .await?
                .try_collect::<Vec<_>>()
                .await?;

            for batch in &batches {
                let callees_col: &StringArray = get_column(batch, "callee")?;
                let callers: &StringArray = get_column(batch, "caller")?;
                let paths: &StringArray = get_column(batch, "caller_file_path")?;
                let kinds: &StringArray = get_column(batch, "kind")?;
                let hashes: &arrow::array::ListArray = get_column(batch, "caller_git_file_hashes")?;

                for row in 0..batch.num_rows() {
                    edges.push(CallEdge {
                        callee: callees_col.value(row).to_string(),
                        caller: callers.value(row).to_string(),
                        caller_file_path: paths.value(row).to_string(),
                        caller_git_file_hashes: crate::database::read_string_list(hashes, row)
                            .unwrap_or_default(),
                        kind: kinds.value(row).to_string(),
                    });
                }
            }
        }

        Ok(edges)
    }

    pub async fn find_callers(&self, callee: &str) -> Result<Vec<CallEdge>> {
        let table = self.connection.open_table("call_edges").execute().await?;
        let escaped = callee.replace('\'', "''");

        let batches = table
            .query()
            .only_if(format!("callee = '{escaped}' AND kind = '{KIND_CALL}'"))
            // `callee` is already known; not reading it back keeps the Take
            // narrow, which is the whole point of this table.
            // caller_file_path is deliberately not selected: a columnar read
            // does not pay for a column it does not project.
            .select(lancedb::query::Select::Columns(vec![
                "caller".to_string(),
                "caller_git_file_hashes".to_string(),
            ]))
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;

        let mut edges = Vec::new();
        for batch in &batches {
            let callers: &StringArray = get_column(batch, "caller")?;
            let hashes: &arrow::array::ListArray = get_column(batch, "caller_git_file_hashes")?;

            for row in 0..batch.num_rows() {
                edges.push(CallEdge {
                    callee: callee.to_string(),
                    caller: callers.value(row).to_string(),
                    caller_file_path: String::new(),
                    caller_git_file_hashes: crate::database::read_string_list(hashes, row)
                        .unwrap_or_default(),
                    kind: KIND_CALL.to_string(),
                });
            }
        }

        Ok(edges)
    }
}
