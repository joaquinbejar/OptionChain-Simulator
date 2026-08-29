//! The Arrow IPC stream encoding, behind the `arrow-export` feature.
//!
//! For consumers that already speak Arrow: IronCondor materialises simulator
//! tapes to Parquet and already pins `arrow`, so an Arrow IPC stream removes a
//! whole conversion stage, and it carries schema, types and nulls with no
//! private convention to agree on.
//!
//! **Feature-gated, off by default.** The `arrow` crate is a large dependency
//! tree, and a deployment that never exports — or a crates.io consumer using
//! this crate as a library — should not pay for it. A request for
//! `format=arrow` on a build without the feature is a typed `400` naming the
//! unavailable format, never a 500 and never a silent fallback to another
//! encoding.
//!
//! # Streaming, and determinism
//!
//! The IPC **stream** format (not the file format) is a sequence of record
//! batches, which is exactly the block shape the export needs: one batch per
//! `OCS_EXPORT_BLOCK_ROWS` rows, so memory is a function of the block width and
//! not of the number of steps.
//!
//! The schema carries no metadata at all — no build identifier, no timestamp,
//! no map with a non-deterministic iteration order — because the endpoint
//! promises byte-identical output on repeat. Column names and order are the
//! CSV header's, so a reader moves between encodings without a mapping table.
//!
//! `labels` is a `Utf8` column here rather than the bitmask `packed` uses:
//! Arrow carries variable-length data natively, and matching the text
//! encodings exactly is worth more to an Arrow consumer than a fixed width.

use crate::api::rest::binary::{BinarySchema, Cell, CellType};
use crate::utils::ChainError;
use arrow::array::{ArrayRef, Float64Builder, Int64Array, StringArray, TimestampNanosecondArray};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use std::sync::Arc;

/// Encodes Arrow IPC record batches.
///
/// Holds at most one block of rows, like the `packed` writer, and owns the
/// `StreamWriter` so the schema message is written exactly once, at the front.
pub(super) struct ArrowWriter {
    schema: BinarySchema,
    arrow_schema: Arc<Schema>,
    block_rows: usize,
    buffered: Vec<Vec<Cell>>,
    writer: Option<StreamWriter<Vec<u8>>>,
}

impl ArrowWriter {
    /// Creates a writer for a schema.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Internal`] when the IPC stream's schema message
    /// cannot be written.
    pub(super) fn new(schema: BinarySchema, block_rows: usize) -> Result<Self, ChainError> {
        let fields: Vec<Field> = schema
            .names
            .iter()
            .zip(&schema.types)
            .map(|(name, cell_type)| {
                Field::new(
                    (*name).to_string(),
                    match cell_type {
                        CellType::F64 => DataType::Float64,
                        CellType::I64 => DataType::Int64,
                        CellType::Timestamp => {
                            DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into()))
                        }
                        // The text columns as text: an Arrow consumer reads the
                        // same values the CSV carries, with no dictionary of
                        // ours to decode.
                        CellType::Dictionary | CellType::LabelMask => DataType::Utf8,
                    },
                    cell_type.nullable(),
                )
            })
            .collect();

        let arrow_schema = Arc::new(Schema::new(fields));
        let writer = StreamWriter::try_new(Vec::new(), &arrow_schema).map_err(|error| {
            ChainError::Internal(format!("failed to open an Arrow stream: {error}"))
        })?;

        Ok(Self {
            schema,
            arrow_schema,
            block_rows: block_rows.max(1),
            buffered: Vec::new(),
            writer: Some(writer),
        })
    }

    /// The schema this writer encodes.
    #[must_use]
    pub(super) fn schema(&self) -> &BinarySchema {
        &self.schema
    }

    /// The bytes written so far, taken out of the stream writer's buffer.
    fn drain(&mut self) -> Vec<u8> {
        match self.writer.as_mut() {
            Some(writer) => std::mem::take(writer.get_mut()),
            None => Vec::new(),
        }
    }

    /// The IPC schema message, which opens the stream.
    ///
    /// # Errors
    ///
    /// Never fails today; fallible for symmetry with the other writers, whose
    /// prologue can.
    pub(super) fn header(&mut self) -> Result<Vec<u8>, ChainError> {
        Ok(self.drain())
    }

    /// Buffers ONE row, returning a batch only when that row completed one.
    ///
    /// Row at a time, like the packed writer and for the same reasons: the
    /// footprint stays one batch whatever a step carries, and a finished batch
    /// reaches the client before the next one is encoded.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Internal`] when a batch cannot be built or
    /// written.
    pub(super) fn push_row(&mut self, row: Vec<Cell>) -> Result<Option<Vec<u8>>, ChainError> {
        self.buffered.push(row);
        if self.buffered.len() < self.block_rows {
            return Ok(None);
        }

        let block = std::mem::take(&mut self.buffered);
        Ok(Some(self.write_batch(&block)?))
    }

    /// The same, for a batch of rows. Test-only, as in the packed writer.
    #[cfg(test)]
    pub(super) fn push(&mut self, rows: Vec<Vec<Cell>>) -> Result<Vec<Vec<u8>>, ChainError> {
        let mut chunks = Vec::new();
        for row in rows {
            if let Some(chunk) = self.push_row(row)? {
                chunks.push(chunk);
            }
        }
        Ok(chunks)
    }

    /// The final batch and the stream's end-of-stream marker.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Internal`] when the last batch cannot be written
    /// or the stream cannot be finished.
    pub(super) fn finish(&mut self) -> Result<Option<Vec<u8>>, ChainError> {
        // The last batch's bytes are part of the answer, not a side effect:
        // `write_batch` drains the stream writer's buffer, so dropping what it
        // returns would silently truncate every export whose row count is under
        // one block.
        let mut out = if self.buffered.is_empty() {
            Vec::new()
        } else {
            let block = std::mem::take(&mut self.buffered);
            self.write_batch(&block)?
        };

        let Some(mut writer) = self.writer.take() else {
            return Ok(if out.is_empty() { None } else { Some(out) });
        };
        writer.finish().map_err(|error| {
            ChainError::Internal(format!("failed to close an Arrow stream: {error}"))
        })?;
        let buffer = writer.into_inner().map_err(|error| {
            ChainError::Internal(format!("failed to close an Arrow stream: {error}"))
        })?;

        out.extend(buffer);
        if out.is_empty() {
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// Builds and writes one record batch, returning the bytes it produced.
    fn write_batch(&mut self, rows: &[Vec<Cell>]) -> Result<Vec<u8>, ChainError> {
        let columns = self.columns(rows)?;
        let batch =
            RecordBatch::try_new(Arc::clone(&self.arrow_schema), columns).map_err(|error| {
                ChainError::Internal(format!("failed to build an Arrow batch: {error}"))
            })?;

        match self.writer.as_mut() {
            Some(writer) => writer.write(&batch).map_err(|error| {
                ChainError::Internal(format!("failed to write an Arrow batch: {error}"))
            })?,
            None => {
                return Err(ChainError::Internal(
                    "the Arrow stream was already closed".to_string(),
                ));
            }
        }
        Ok(self.drain())
    }

    /// Builds one array per column, in schema order.
    fn columns(&self, rows: &[Vec<Cell>]) -> Result<Vec<ArrayRef>, ChainError> {
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(self.schema.types.len());

        for (index, cell_type) in self.schema.types.iter().enumerate() {
            let array: ArrayRef = match cell_type {
                CellType::F64 => {
                    let mut builder = Float64Builder::with_capacity(rows.len());
                    for row in rows {
                        match row.get(index) {
                            // A null is a null: an unset validity bit, never a
                            // sentinel and never a NaN, which is a value a
                            // chain can legitimately hold.
                            Some(Cell::F64(Some(value))) => builder.append_value(*value),
                            _ => builder.append_null(),
                        }
                    }
                    Arc::new(builder.finish()) as ArrayRef
                }
                CellType::I64 => Arc::new(Int64Array::from(
                    rows.iter()
                        .map(|row| match row.get(index) {
                            Some(Cell::I64(value)) => *value,
                            _ => 0,
                        })
                        .collect::<Vec<i64>>(),
                )) as ArrayRef,
                CellType::Timestamp => Arc::new(
                    TimestampNanosecondArray::from(
                        rows.iter()
                            .map(|row| match row.get(index) {
                                Some(Cell::Timestamp(value)) => *value,
                                _ => 0,
                            })
                            .collect::<Vec<i64>>(),
                    )
                    .with_timezone("UTC"),
                ) as ArrayRef,
                CellType::Dictionary => Arc::new(StringArray::from(
                    rows.iter()
                        .map(|row| match row.get(index) {
                            Some(Cell::Dictionary(value)) => self
                                .schema
                                .dictionary
                                .get(*value as usize)
                                .cloned()
                                .unwrap_or_default(),
                            _ => String::new(),
                        })
                        .collect::<Vec<String>>(),
                )) as ArrayRef,
                CellType::LabelMask => Arc::new(StringArray::from(
                    rows.iter()
                        .map(|row| match row.get(index) {
                            // Joined exactly as the text encodings join them,
                            // from the same lexicographic dictionary.
                            Some(Cell::LabelMask(mask)) => self.schema.labels_of(*mask).join("|"),
                            _ => String::new(),
                        })
                        .collect::<Vec<String>>(),
                )) as ArrayRef,
            };
            columns.push(array);
        }

        // Every column must agree on its length, or `RecordBatch::try_new`
        // rejects the batch — which is the check itself, so nothing is asserted
        // here beyond letting it run.
        debug_assert!(
            columns
                .iter()
                .all(|column| column.len() == rows.len() || rows.is_empty()),
            "every Arrow column must carry one value per row"
        );
        Ok(columns)
    }
}

/// Reads an Arrow IPC stream back into rows of optional doubles and strings.
///
/// Test-only: the cross-format equality tests decode what the writer produced
/// rather than trusting it, which is the only way the "identical to the json
/// export" claim means anything.
#[cfg(test)]
pub(super) fn decode_stream(bytes: &[u8]) -> Result<Vec<Vec<String>>, ChainError> {
    use arrow::array::{Array, Float64Array};
    use arrow::ipc::reader::StreamReader;

    let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).map_err(|error| {
        ChainError::Internal(format!("failed to read an Arrow stream: {error}"))
    })?;

    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch
            .map_err(|error| ChainError::Internal(format!("failed to read a batch: {error}")))?;
        for index in 0..batch.num_rows() {
            let mut values = Vec::with_capacity(batch.num_columns());
            for column in batch.columns() {
                if column.is_null(index) {
                    values.push(String::new());
                    continue;
                }
                if let Some(array) = column.as_any().downcast_ref::<Float64Array>() {
                    values.push(array.value(index).to_string());
                } else if let Some(array) = column.as_any().downcast_ref::<Int64Array>() {
                    values.push(array.value(index).to_string());
                } else if let Some(array) =
                    column.as_any().downcast_ref::<TimestampNanosecondArray>()
                {
                    // Rendered the way every other v2 timestamp is rendered, so
                    // a decoded batch compares against the CSV directly and the
                    // equality claim means something.
                    values.push(
                        chrono::DateTime::from_timestamp_nanos(array.value(index))
                            .to_utc()
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    );
                } else if let Some(array) = column.as_any().downcast_ref::<StringArray>() {
                    values.push(array.value(index).to_string());
                } else {
                    return Err(ChainError::Internal(
                        "an Arrow column carried an unexpected type".to_string(),
                    ));
                }
            }
            rows.push(values);
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> BinarySchema {
        BinarySchema {
            names: vec!["step", "simulated_at", "symbol", "labels", "price"],
            types: vec![
                CellType::I64,
                CellType::Timestamp,
                CellType::Dictionary,
                CellType::LabelMask,
                CellType::F64,
            ],
            dictionary: vec![
                "SPX".to_string(),
                "monthlies".to_string(),
                "zero_dte".to_string(),
            ],
        }
    }

    fn row(step: i64, price: Option<f64>, mask: u64) -> Vec<Cell> {
        vec![
            Cell::I64(step),
            Cell::Timestamp(1_767_623_400_000_000_000),
            Cell::Dictionary(0),
            Cell::LabelMask(mask),
            Cell::F64(price),
        ]
    }

    fn encode(rows: Vec<Vec<Cell>>, block_rows: usize) -> Vec<u8> {
        let mut writer = match ArrowWriter::new(schema(), block_rows) {
            Ok(writer) => writer,
            Err(error) => panic!("the writer must open: {error}"),
        };

        let mut bytes = match writer.header() {
            Ok(header) => header,
            Err(error) => panic!("the header must encode: {error}"),
        };
        match writer.push(rows) {
            Ok(chunks) => bytes.extend(chunks.into_iter().flatten()),
            Err(error) => panic!("the rows must encode: {error}"),
        }
        match writer.finish() {
            Ok(Some(tail)) => bytes.extend(tail),
            Ok(None) => {}
            Err(error) => panic!("the stream must close: {error}"),
        }
        bytes
    }

    /// Rows survive the round trip, values and nulls alike.
    #[test]
    fn test_rows_round_trip_through_the_stream() {
        // Bits 0 and 1: `monthlies` and `zero_dte`, the dictionary's two rule ids.
        let bytes = encode(vec![row(0, Some(1.5), 0b11), row(1, None, 0)], 8);

        match decode_stream(&bytes) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][0], "0");
                assert_eq!(rows[0][1], "2026-01-05T14:30:00Z");
                assert_eq!(rows[0][2], "SPX");
                assert_eq!(rows[0][3], "monthlies|zero_dte");
                assert_eq!(rows[0][4], "1.5");
                assert_eq!(rows[1][4], "", "a null must read back as a null");
                assert_eq!(rows[1][3], "", "an empty label set is an empty string");
            }
            Err(error) => panic!("the stream must decode: {error}"),
        }
    }

    /// A block width smaller than the row count produces several batches, and
    /// the decoded rows are the same either way.
    #[test]
    fn test_the_block_width_does_not_change_the_values() {
        let rows = vec![
            row(0, Some(1.0), 0),
            row(1, Some(2.0), 0),
            row(2, None, 0),
            row(3, Some(4.0), 0),
        ];

        let narrow = match decode_stream(&encode(rows.clone(), 1)) {
            Ok(decoded) => decoded,
            Err(error) => panic!("the narrow stream must decode: {error}"),
        };
        let wide = match decode_stream(&encode(rows, 64)) {
            Ok(decoded) => decoded,
            Err(error) => panic!("the wide stream must decode: {error}"),
        };

        assert_eq!(narrow, wide);
    }

    /// The same rows encode to the same bytes, every time.
    ///
    /// The endpoint promises byte-identical output on repeat, and an IPC
    /// stream is where that is easiest to lose: schema metadata carrying a
    /// build id or a timestamp would break it silently.
    #[test]
    fn test_the_encoding_is_byte_identical_on_repeat() {
        let rows = vec![row(0, Some(1.0), 0b10), row(1, None, 0)];

        assert_eq!(encode(rows.clone(), 2), encode(rows, 2));
    }

    /// The schema carries no metadata, which is what makes the repeat stable.
    #[test]
    fn test_the_schema_carries_no_metadata() {
        let writer = match ArrowWriter::new(schema(), 8) {
            Ok(writer) => writer,
            Err(error) => panic!("the writer must open: {error}"),
        };

        assert!(
            writer.arrow_schema.metadata().is_empty(),
            "schema metadata is a determinism hazard: {:?}",
            writer.arrow_schema.metadata()
        );
        assert_eq!(
            writer
                .arrow_schema
                .fields()
                .iter()
                .map(|field| field.name().as_str())
                .collect::<Vec<_>>(),
            vec!["step", "simulated_at", "symbol", "labels", "price"],
            "the column order is the csv header's"
        );
    }
}
