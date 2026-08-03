// SPDX-License-Identifier: MIT OR Apache-2.0
pub mod branches;
pub mod calls;
mod connection;
pub mod content;
pub mod edges;
mod functions;
pub mod processed_files;
mod schema;
pub mod search;
mod symbol_filename;
mod types;
mod vectors;

pub use connection::DatabaseManager;

use anyhow::Result;
use arrow::array::RecordBatch;

/// Look up a column by name and downcast to the expected Arrow array type.
pub(crate) fn get_column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("missing column '{name}' in batch"))?
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| anyhow::anyhow!("column '{name}' has unexpected type"))
}

/// Append one row to a `List<Utf8>` column builder.
///
/// `None` writes a null list, which is distinct from an empty list: a function
/// we never analysed has null calls, a leaf function has an empty list.
pub(crate) fn append_string_list(
    builder: &mut arrow::array::ListBuilder<arrow::array::StringBuilder>,
    values: Option<&[String]>,
) {
    match values {
        Some(items) => {
            for item in items {
                builder.values().append_value(item);
            }
            builder.append(true);
        }
        None => builder.append(false),
    }
}

/// Read one row of a `List<Utf8>` column back into owned strings.
///
/// The counterpart to [`append_string_list`]: a null list reads back as `None`,
/// an empty list as `Some(vec![])`.
pub(crate) fn read_string_list(array: &arrow::array::ListArray, row: usize) -> Option<Vec<String>> {
    use arrow::array::Array as _;

    if array.is_null(row) {
        return None;
    }

    let values = array.value(row);
    let strings = values
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()?;

    Some(
        (0..strings.len())
            .map(|i| strings.value(i).to_string())
            .collect(),
    )
}
