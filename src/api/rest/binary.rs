//! The binary export encodings, and the typed rows both of them read.
//!
//! `json` and `csv` are text, so every consumer pays a parse. For the two that
//! matter that parse IS the bottleneck: a browser materialising a whole tape
//! spends hundreds of milliseconds in `JSON.parse` and allocates an object per
//! row, and a Rust consumer writing Parquet re-parses text this service already
//! held in typed form.
//!
//! Two binary encodings answer that, and they share everything except who they
//! are for:
//!
//! - **`arrow`** — Arrow IPC stream, for consumers that already speak Arrow.
//!   Feature-gated behind `arrow-export`, off by default, because the `arrow`
//!   crate is a large tree and a deployment that never exports should not pay
//!   for it. Lives in the `arrow_export` module, compiled only with that
//!   feature.
//! - **`packed`** — a minimal self-describing columnar block format with no
//!   dependencies, for the browser. Arrow's JavaScript library is a large
//!   bundle for a page whose only need is "these columns as typed arrays";
//!   `packed` is a few dozen lines of decoder and zero bundle weight.
//!
//! # Both stream, in blocks
//!
//! The export endpoint streams: the module it lives in exists so a tape is
//! never materialised in memory. Columnar encoding pulls the other way, since a
//! column cannot be emitted until its last row is known. Both formats resolve
//! it the same way — **blocks of a fixed number of rows, each columnar within
//! itself**. Arrow IPC already works exactly like that (a stream of record
//! batches); `packed` adopts the same shape, and
//! `OCS_EXPORT_BLOCK_ROWS` sets the width for both.
//!
//! # Precision
//!
//! Numeric columns are `f64`, the same values `json` and `csv` already render
//! through `decimal_to_f64`. Binary is a faster route to the same numbers, NOT
//! a route to the underlying `Decimal(38, 28)` precision.
//!
//! # The `packed` layout
//!
//! Little-endian throughout. Every payload starts at an 8-byte aligned offset,
//! which is the whole point: it is what lets a browser do
//! `new Float64Array(buffer, offset, count)` with no copy at all. An unaligned
//! offset makes that constructor throw, so the alignment is a correctness
//! requirement rather than a nicety.
//!
//! ```text
//! file        := header block* footer
//! header      := "OCSP" u32:version u32:block_rows
//!                u32:dictionary_count dictionary_entry*
//!                u32:column_count column_desc*
//!                pad to 8
//! dict_entry  := u32:len utf8:value            (the symbol, then the rule ids)
//! column_desc := u32:name_len utf8:name u8:type_code u8:nullable pad to 4
//! block       := u32:row_count pad to 8 column_payload*
//! payload     := [validity bitmap if nullable, padded to 8] values, padded to 8
//! footer      := u32:0xFFFFFFFF pad to 8 u64:total_rows
//! ```
//!
//! **The footer is required, and a decoder must check it.** A block's row count
//! can never be `0xFFFFFFFF`, so that value is what tells a reader the blocks
//! have ended; the `u64` after it is the total number of rows the writer
//! emitted. A document that ends without the sentinel was TRUNCATED and must be
//! rejected rather than read as a shorter tape — the response is a 200 whose
//! header goes out before the first byte is produced, so a dropped connection
//! is otherwise indistinguishable from a smaller export. A row count that
//! disagrees with the blocks is the same error.
//!
//! Type codes: `0` = `f64`, `1` = `i64`, `2` = timestamp in nanoseconds since
//! the epoch (`i64`), `3` = a dictionary index into the header's entries
//! (`u32`), `4` = a **label bitmask** (`u64`), one bit per header dictionary
//! entry, set when that rule id labels the row's expiration. The bitmask is
//! what keeps variable-length text out of a block: the rule ids are known
//! before the first row is produced, they are the only text a row carries
//! besides the symbol, and a bitmask over them reconstructs the `csv` column
//! exactly, because both are ordered lexicographically.
//!
//! Validity bitmaps use Arrow's convention — LSB-first, `1` = valid — so the
//! two decoders agree and a reader ports between them.

use crate::api::rest::export::Dataset;
use crate::api::rest::greeks::GreekLevel;
use crate::domain::factors::FactorRow;
use crate::session::SimulationParametersV2;
use crate::utils::ChainError;
use chrono::{DateTime, Utc};

/// The magic that opens a `packed` document.
pub(super) const PACKED_MAGIC: &[u8; 4] = b"OCSP";

/// The `packed` layout version this build writes.
///
/// Bumped when the layout changes in a way a decoder must notice. A decoder
/// that reads a version it does not know must refuse rather than guess.
pub(super) const PACKED_VERSION: u32 = 1;

/// Everything in a block starts on this boundary.
const ALIGNMENT: usize = 8;

/// The row count that marks the footer rather than a block.
///
/// No block can carry `u32::MAX` rows — one would be tens of gigabytes — so a
/// decoder that meets it knows the document ended deliberately.
pub(super) const PACKED_FOOTER_SENTINEL: u32 = u32::MAX;

/// What one column carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CellType {
    /// A nullable double.
    F64,
    /// A step index.
    I64,
    /// An instant, as nanoseconds since the epoch.
    Timestamp,
    /// An index into the header dictionary.
    Dictionary,
    /// A bitmask over the header dictionary.
    LabelMask,
}

impl CellType {
    /// The code written into a `packed` column descriptor.
    #[must_use]
    pub(super) fn code(self) -> u8 {
        match self {
            CellType::F64 => 0,
            CellType::I64 => 1,
            CellType::Timestamp => 2,
            CellType::Dictionary => 3,
            CellType::LabelMask => 4,
        }
    }

    /// Whether a column of this type carries a validity bitmap.
    ///
    /// Only the doubles are nullable: a step, an instant, a symbol and a label
    /// set are present on every row a dataset produces.
    #[must_use]
    pub(super) fn nullable(self) -> bool {
        matches!(self, CellType::F64)
    }

    /// The width of one value, in bytes.
    #[must_use]
    pub(super) fn width(self) -> usize {
        match self {
            CellType::F64 | CellType::I64 | CellType::Timestamp | CellType::LabelMask => 8,
            CellType::Dictionary => 4,
        }
    }
}

/// One value of one row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Cell {
    /// A nullable double, exactly what `json` and `csv` render.
    F64(Option<f64>),
    /// A step index.
    I64(i64),
    /// An instant, as nanoseconds since the epoch.
    Timestamp(i64),
    /// An index into the header dictionary.
    Dictionary(u32),
    /// A bitmask over the header dictionary.
    LabelMask(u64),
}

impl Cell {
    /// The column type this value belongs to.
    ///
    /// The schema decides the column types up front, so nothing on the encoding
    /// path asks a value what it is; this exists so a test can assert a row's
    /// cells line up with the schema that describes them.
    #[cfg(test)]
    #[must_use]
    pub(super) fn cell_type(&self) -> CellType {
        match self {
            Cell::F64(_) => CellType::F64,
            Cell::I64(_) => CellType::I64,
            Cell::Timestamp(_) => CellType::Timestamp,
            Cell::Dictionary(_) => CellType::Dictionary,
            Cell::LabelMask(_) => CellType::LabelMask,
        }
    }
}

/// The schema a binary export writes, derived from the text header.
///
/// Same names and the same order as the CSV header, so a consumer moves
/// between encodings without a mapping table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BinarySchema {
    /// Column names, in order.
    pub(super) names: Vec<&'static str>,
    /// Column types, in the same order.
    pub(super) types: Vec<CellType>,
    /// The text values a row can carry: the symbol first, then the rule ids in
    /// lexicographic order, which is the order the planner merges labels in.
    pub(super) dictionary: Vec<String>,
}

impl BinarySchema {
    /// Derives the schema of one dataset at one greek level.
    ///
    /// The dictionary is built HERE, before the first row is produced, from the
    /// simulation's own parameters: the symbol is fixed for a simulation and
    /// the rule ids are fixed by its schedule. That is what allows a block to
    /// carry no variable-length data and still reproduce the text columns.
    #[must_use]
    pub(super) fn new(
        dataset: Dataset,
        level: GreekLevel,
        parameters: &SimulationParametersV2,
    ) -> Self {
        let names = dataset.header(level);
        let types = names
            .iter()
            .map(|name| match *name {
                "step" => CellType::I64,
                "simulated_at" | "expires_at" => CellType::Timestamp,
                "symbol" => CellType::Dictionary,
                "labels" => CellType::LabelMask,
                _ => CellType::F64,
            })
            .collect();

        let mut dictionary = vec![parameters.symbol.clone()];
        let mut rule_ids: Vec<String> = parameters
            .schedule
            .rules()
            .iter()
            .map(|rule| rule.rule_id().to_string())
            .collect();
        // Lexicographic, matching the `BTreeSet` the planner merges labels in,
        // so a bitmask reconstructs the joined `csv` value character for
        // character.
        rule_ids.sort();
        rule_ids.dedup();
        dictionary.extend(rule_ids);

        Self {
            names,
            types,
            dictionary,
        }
    }

    /// The dictionary index of the symbol, which is always the first entry.
    #[must_use]
    pub(super) fn symbol_index(&self) -> u32 {
        0
    }

    /// The bitmask of a label set.
    ///
    /// Bit `n` is dictionary entry `n + 1`: entry 0 is the SYMBOL and is never
    /// a label. Skipping it is not cosmetic — a rule id that happens to equal
    /// the symbol would otherwise take bit 0 and be reconstructed ahead of
    /// every other rule id, which is a different `labels` string from the one
    /// the text encodings render.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Internal`] when a label is not in the dictionary.
    /// It cannot be, for a simulation whose own schedule produced it, and
    /// dropping one silently is the one thing this encoding must not do: its
    /// whole contract is that it carries the same values the text encodings do.
    pub(super) fn label_mask(&self, labels: &[String]) -> Result<u64, ChainError> {
        let mut mask = 0_u64;
        for label in labels {
            let position = self
                .dictionary
                .iter()
                .skip(1)
                .position(|entry| entry == label)
                .ok_or_else(|| {
                    ChainError::Internal(format!(
                        "the label {label:?} is not one of this simulation's rule ids"
                    ))
                })?;
            if position >= u64::BITS as usize {
                return Err(ChainError::Internal(format!(
                    "the label {label:?} is past the {MAX_LABEL_RULES} a mask carries"
                )));
            }
            mask |= 1 << position;
        }
        Ok(mask)
    }

    /// The rule ids a mask names, in the dictionary's order.
    ///
    /// The inverse of [`BinarySchema::label_mask`], and the reason a decoder can
    /// rebuild the text column: joined with `|` this is the `csv` value. The
    /// Arrow encoding calls it directly, since it writes the labels as text.
    #[cfg_attr(not(any(test, feature = "arrow-export")), allow(dead_code))]
    #[must_use]
    pub(super) fn labels_of(&self, mask: u64) -> Vec<&str> {
        // `skip(1)` for the symbol, so bit `n` names entry `n + 1`, the inverse
        // of `label_mask`.
        self.dictionary
            .iter()
            .skip(1)
            .enumerate()
            .filter(|(position, _)| *position < u64::BITS as usize && mask & (1 << position) != 0)
            .map(|(_, entry)| entry.as_str())
            .collect()
    }
}

/// How many rule ids a label mask can carry.
///
/// One bit each, in a `u64`. The schedule validator already caps a simulation
/// at [`crate::domain::expiry::MAX_SCHEDULE_RULES`] rules, well under this, and
/// the assertion below keeps the two from drifting apart: raising that cap past
/// 64 would silently start dropping labels from the binary encodings.
pub(super) const MAX_LABEL_RULES: usize = 63;

const _: () = assert!(
    crate::domain::expiry::MAX_SCHEDULE_RULES <= MAX_LABEL_RULES,
    "a schedule may carry more rules than a packed label mask has bits"
);

/// An instant as nanoseconds since the epoch, saturating at the representable
/// range rather than wrapping.
#[must_use]
pub(super) fn timestamp_nanos(instant: DateTime<Utc>) -> i64 {
    instant.timestamp_nanos_opt().unwrap_or(i64::MAX)
}

/// The rows one step contributes, as typed cells.
///
/// The same shape, in the same order, as the module's `csv_rows` produces as
/// strings — deliberately built from the same view types rather than from a
/// second traversal, so the encodings cannot disagree about what a row is.
/// Everything one step's rows are built from.
///
/// Grouped rather than passed as seven parameters: they are one thing — the
/// step being encoded — and they travel together to both the visitor and the
/// collecting helper beside it.
pub(super) struct RowContext<'a> {
    /// The columns being written, and the dictionary behind them.
    pub(super) schema: &'a BinarySchema,
    /// Which dataset the rows belong to.
    pub(super) dataset: Dataset,
    /// How much of the greek set the rows carry.
    pub(super) level: GreekLevel,
    /// The step index.
    pub(super) step: usize,
    /// The simulated instant of the step.
    pub(super) simulated_at: DateTime<Utc>,
    /// The factor row the step was priced from.
    pub(super) row: &'a FactorRow,
    /// The step's chains, absent for the datasets that carry none.
    pub(super) chains: Option<super::export::StepChains<'a>>,
}

/// Whether a row visitor wants more rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowFlow {
    /// Keep going.
    Continue,
    /// Stop here: the client is gone, and nothing further is worth encoding.
    Stop,
}

/// Feeds one step's rows to a visitor, ONE AT A TIME.
///
/// The reason this is a visitor rather than a `Vec`: a step of the widest
/// dataset can carry `OCS_MAX_SNAPSHOT_CONTRACTS` rows, and materialising them
/// all before the first block is written would make an export's memory a
/// function of the chain size rather than of the block width, and would delay
/// noticing a disconnected client until the whole step had been encoded.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] when a chain carries a label this
/// simulation's schedule does not name, which would make the binary `labels`
/// column disagree with the text one, and whatever the visitor returns.
pub(super) fn visit_typed_rows<F>(
    context: &RowContext<'_>,
    visit: &mut F,
) -> Result<RowFlow, ChainError>
where
    F: FnMut(Vec<Cell>) -> Result<RowFlow, ChainError>,
{
    let RowContext {
        schema,
        dataset,
        level,
        step,
        simulated_at,
        row,
        chains,
    } = context;

    let step_cell = Cell::I64(i64::try_from(*step).unwrap_or(i64::MAX));
    let instant = Cell::Timestamp(timestamp_nanos(*simulated_at));
    let symbol = Cell::Dictionary(schema.symbol_index());

    match dataset {
        Dataset::Underlying => visit(vec![
            step_cell,
            instant,
            symbol,
            Cell::F64(Some(row.spot.to_f64())),
        ]),
        Dataset::Volatility => visit(vec![
            step_cell,
            instant,
            symbol,
            Cell::F64(Some(row.base_volatility.to_f64())),
        ]),
        Dataset::OptionChains => {
            let Some(chains) = chains else {
                return Ok(RowFlow::Continue);
            };
            for expiration in chains.expirations() {
                let expires_at = Cell::Timestamp(timestamp_nanos(expiration.expires_at));
                let labels = Cell::LabelMask(schema.label_mask(expiration.labels)?);
                for quote in expiration.quotes.quotes() {
                    let mut cells = vec![
                        step_cell,
                        instant,
                        symbol,
                        expires_at,
                        labels,
                        Cell::F64(Some(expiration.days_to_expiration)),
                        Cell::F64(Some(quote.strike)),
                        Cell::F64(Some(quote.implied_volatility)),
                        Cell::F64(quote.call_bid),
                        Cell::F64(quote.call_ask),
                        Cell::F64(quote.call_mid),
                        Cell::F64(quote.call_delta),
                        Cell::F64(quote.put_bid),
                        Cell::F64(quote.put_ask),
                        Cell::F64(quote.put_mid),
                        Cell::F64(quote.put_delta),
                        Cell::F64(quote.gamma),
                    ];
                    if level.wants_greeks() {
                        for value in [
                            quote.call_greeks.theta,
                            quote.put_greeks.theta,
                            quote.call_greeks.vega,
                            quote.put_greeks.vega,
                            quote.call_greeks.rho,
                            quote.put_greeks.rho,
                            quote.call_greeks.rho_d,
                            quote.put_greeks.rho_d,
                        ] {
                            cells.push(Cell::F64(value));
                        }
                    }
                    if matches!(level, GreekLevel::All) {
                        for value in [
                            quote.call_greeks.gamma,
                            quote.put_greeks.gamma,
                            quote.call_greeks.alpha,
                            quote.put_greeks.alpha,
                            quote.call_greeks.vanna,
                            quote.put_greeks.vanna,
                            quote.call_greeks.vomma,
                            quote.put_greeks.vomma,
                            quote.call_greeks.veta,
                            quote.put_greeks.veta,
                            quote.call_greeks.charm,
                            quote.put_greeks.charm,
                            quote.call_greeks.color,
                            quote.put_greeks.color,
                        ] {
                            cells.push(Cell::F64(value));
                        }
                    }
                    if visit(cells)? == RowFlow::Stop {
                        return Ok(RowFlow::Stop);
                    }
                }
            }
            Ok(RowFlow::Continue)
        }
    }
}

/// The same rows, collected.
///
/// Test-only, and written over the visitor rather than beside it: two copies of
/// the row builder would be two things to keep in step, and only one of them
/// would be the one that ships.
#[cfg(test)]
pub(super) fn typed_rows(context: &RowContext<'_>) -> Result<Vec<Vec<Cell>>, ChainError> {
    let mut rows = Vec::new();
    visit_typed_rows(context, &mut |cells| {
        rows.push(cells);
        Ok(RowFlow::Continue)
    })?;
    Ok(rows)
}

/// Encodes `packed` blocks.
///
/// Holds at most one block of rows, which is what keeps the export streaming:
/// memory is a function of the block width, never of the number of steps.
pub(super) struct PackedWriter {
    schema: BinarySchema,
    block_rows: usize,
    buffered: Vec<Vec<Cell>>,
    /// Rows written so far, reported in the footer so a truncated download is
    /// detectable rather than looking like a shorter tape.
    written: usize,
}

impl PackedWriter {
    /// Creates a writer for a schema.
    #[must_use]
    pub(super) fn new(schema: BinarySchema, block_rows: usize) -> Self {
        Self {
            schema,
            block_rows: block_rows.max(1),
            buffered: Vec::new(),
            written: 0,
        }
    }

    /// The schema this writer encodes.
    #[must_use]
    pub(super) fn schema(&self) -> &BinarySchema {
        &self.schema
    }

    /// The document header.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Internal`] when a name or dictionary entry is
    /// longer than the layout's `u32` length can describe.
    pub(super) fn header(&self) -> Result<Vec<u8>, ChainError> {
        let mut out = Vec::new();
        out.extend_from_slice(PACKED_MAGIC);
        out.extend_from_slice(&PACKED_VERSION.to_le_bytes());
        out.extend_from_slice(&u32_of(self.block_rows)?.to_le_bytes());

        out.extend_from_slice(&u32_of(self.schema.dictionary.len())?.to_le_bytes());
        for entry in &self.schema.dictionary {
            out.extend_from_slice(&u32_of(entry.len())?.to_le_bytes());
            out.extend_from_slice(entry.as_bytes());
        }

        out.extend_from_slice(&u32_of(self.schema.names.len())?.to_le_bytes());
        for (name, cell_type) in self.schema.names.iter().zip(&self.schema.types) {
            out.extend_from_slice(&u32_of(name.len())?.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.push(cell_type.code());
            out.push(u8::from(cell_type.nullable()));
            pad_to(&mut out, 4);
        }

        // So the first block, and therefore its first payload, starts aligned.
        pad_to(&mut out, ALIGNMENT);
        Ok(out)
    }

    /// Buffers ONE row, returning a block only when that row completed one.
    ///
    /// Row at a time on purpose: it is what keeps the writer's footprint at one
    /// block regardless of how many rows a step carries, and it lets the caller
    /// hand a finished block to the client before encoding the next one, so a
    /// disconnect is noticed within a block rather than within a step.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Internal`] when a block's row count does not fit
    /// the layout's `u32`.
    pub(super) fn push_row(&mut self, row: Vec<Cell>) -> Result<Option<Vec<u8>>, ChainError> {
        self.buffered.push(row);
        if self.buffered.len() < self.block_rows {
            return Ok(None);
        }

        let block = std::mem::take(&mut self.buffered);
        self.written = self.written.saturating_add(block.len());
        Ok(Some(self.encode_block(&block)?))
    }

    /// The same, for a batch of rows. Test-only: the serving path pushes one
    /// row at a time so a step never sits in memory.
    #[cfg(test)]
    pub(super) fn push(&mut self, rows: Vec<Vec<Cell>>) -> Result<Vec<Vec<u8>>, ChainError> {
        let mut blocks = Vec::new();
        for row in rows {
            if let Some(block) = self.push_row(row)? {
                blocks.push(block);
            }
        }
        Ok(blocks)
    }

    /// The last partial block, followed by the footer that closes the document.
    ///
    /// The footer is what makes a truncated download detectable: without it a
    /// connection dropped mid-stream produces a shorter document that decodes
    /// as a perfectly valid smaller tape, under a 200 whose header went out
    /// before the first byte was produced.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Internal`] when the last block cannot be encoded.
    pub(super) fn flush(&mut self) -> Result<Option<Vec<u8>>, ChainError> {
        let mut out = if self.buffered.is_empty() {
            Vec::new()
        } else {
            let block = std::mem::take(&mut self.buffered);
            self.written = self.written.saturating_add(block.len());
            self.encode_block(&block)?
        };

        // A row count no block can carry, so a decoder that meets it knows the
        // document is over rather than guessing from a short read.
        out.extend_from_slice(&PACKED_FOOTER_SENTINEL.to_le_bytes());
        pad_to(&mut out, ALIGNMENT);
        out.extend_from_slice(&(self.written as u64).to_le_bytes());
        Ok(Some(out))
    }

    /// Encodes one block: every column, in schema order.
    fn encode_block(&self, rows: &[Vec<Cell>]) -> Result<Vec<u8>, ChainError> {
        let mut out = Vec::new();
        out.extend_from_slice(&u32_of(rows.len())?.to_le_bytes());
        pad_to(&mut out, ALIGNMENT);

        for (index, cell_type) in self.schema.types.iter().enumerate() {
            if cell_type.nullable() {
                let mut bitmap = vec![0_u8; rows.len().div_ceil(8)];
                for (position, row) in rows.iter().enumerate() {
                    let valid = matches!(row.get(index), Some(Cell::F64(Some(_))));
                    // LSB-first, `1` = valid: Arrow's convention, so one
                    // decoder serves both formats. Indexed through `get_mut`
                    // rather than `[]`: the bound is provable, but a request
                    // path does not index unchecked.
                    if let Some(byte) = bitmap.get_mut(position / 8)
                        && valid
                    {
                        *byte |= 1 << (position % 8);
                    }
                }
                out.extend_from_slice(&bitmap);
                pad_to(&mut out, ALIGNMENT);
            }

            for row in rows {
                match row.get(index) {
                    Some(Cell::F64(value)) => {
                        // A null writes zero bytes under a cleared validity
                        // bit, never a sentinel and never a NaN: a NaN is a
                        // value a chain can legitimately hold.
                        out.extend_from_slice(&value.unwrap_or(0.0).to_le_bytes());
                    }
                    Some(Cell::I64(value) | Cell::Timestamp(value)) => {
                        out.extend_from_slice(&value.to_le_bytes());
                    }
                    Some(Cell::Dictionary(value)) => out.extend_from_slice(&value.to_le_bytes()),
                    Some(Cell::LabelMask(value)) => out.extend_from_slice(&value.to_le_bytes()),
                    // Unreachable: every row of a dataset carries every column.
                    // Written as zeroes of the right width rather than skipped,
                    // so a short row could never shift the column that follows.
                    None => out.extend_from_slice(&vec![0_u8; cell_type.width()]),
                }
            }
            pad_to(&mut out, ALIGNMENT);
        }

        Ok(out)
    }
}

/// Pads a buffer with zeroes until its length is a multiple of `boundary`.
fn pad_to(out: &mut Vec<u8>, boundary: usize) {
    let remainder = out.len() % boundary;
    if remainder != 0 {
        out.extend(std::iter::repeat_n(0_u8, boundary - remainder));
    }
}

/// A length as the `u32` the layout writes.
///
/// Checked, not saturating: `u32::MAX` is itself a length a decoder would
/// trust, so writing it in place of an overflowing one would corrupt the
/// document rather than refuse it. Every caller passes a column name, a
/// dictionary entry or a block width, so this cannot fire in practice.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] when the value does not fit.
fn u32_of(value: usize) -> Result<u32, ChainError> {
    u32::try_from(value).map_err(|_| {
        ChainError::Internal(format!("a packed length of {value} does not fit in a u32"))
    })
}

/// Refuses a schema whose rule ids cannot be label-masked.
///
/// An INTERNAL invariant, not an API behaviour a client can trip: a simulation
/// is already capped at [`crate::domain::expiry::MAX_SCHEDULE_RULES`] rules at
/// creation, and the compile-time assertion beside [`MAX_LABEL_RULES`] keeps
/// that cap below the mask's width. This is the runtime half of the same guard,
/// so a future widening of the schedule cap fails loudly here rather than
/// silently dropping labels from a binary export.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] when the schema carries more rule ids than
/// a `u64` mask has bits.
pub(super) fn ensure_label_capacity(schema: &BinarySchema) -> Result<(), ChainError> {
    // The symbol takes the first entry, so the rule ids are the rest. An empty
    // dictionary is not reachable — `BinarySchema::new` always writes the
    // symbol — and is read as "no rules" rather than underflowing.
    let rules = match schema.dictionary.len() {
        0 => 0,
        length => length - 1,
    };
    if rules > MAX_LABEL_RULES {
        return Err(ChainError::Internal(format!(
            "a binary export carries at most {MAX_LABEL_RULES} schedule rules, \
             this schema has {rules}"
        )));
    }
    Ok(())
}

/// Decodes a `packed` document back into the values the text encodings render.
///
/// Test-only, and deliberately written against the layout documented in this
/// module rather than against the writer's internals: it is what makes the
/// cross-format equality tests mean something, and it doubles as the reference
/// a browser decoder is ported from.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] when the bytes are not a `packed` document
/// this build understands, or end early.
#[cfg(test)]
pub(super) fn decode_packed(bytes: &[u8]) -> Result<(Vec<String>, Vec<Vec<String>>), ChainError> {
    use chrono::SecondsFormat;

    let short = || ChainError::Internal("the packed document ended early".to_string());
    let mut cursor = 0_usize;

    let take = |cursor: &mut usize, count: usize| -> Result<&[u8], ChainError> {
        let end = cursor.checked_add(count).ok_or_else(short)?;
        let slice = bytes.get(*cursor..end).ok_or_else(short)?;
        *cursor = end;
        Ok(slice)
    };
    let take_u32 = |cursor: &mut usize| -> Result<u32, ChainError> {
        let slice = take(cursor, 4)?;
        let array: [u8; 4] = slice.try_into().map_err(|_| short())?;
        Ok(u32::from_le_bytes(array))
    };
    let take_i64 = |cursor: &mut usize| -> Result<i64, ChainError> {
        let slice = take(cursor, 8)?;
        let array: [u8; 8] = slice.try_into().map_err(|_| short())?;
        Ok(i64::from_le_bytes(array))
    };
    let align = |cursor: &mut usize| {
        let remainder = *cursor % ALIGNMENT;
        if remainder != 0 {
            *cursor += ALIGNMENT - remainder;
        }
    };

    if take(&mut cursor, 4)? != PACKED_MAGIC {
        return Err(ChainError::Internal("not a packed document".to_string()));
    }
    let version = take_u32(&mut cursor)?;
    if version != PACKED_VERSION {
        return Err(ChainError::Internal(format!(
            "unknown packed version {version}"
        )));
    }
    let _block_rows = take_u32(&mut cursor)?;

    let dictionary_len = take_u32(&mut cursor)? as usize;
    let mut dictionary = Vec::with_capacity(dictionary_len);
    for _ in 0..dictionary_len {
        let len = take_u32(&mut cursor)? as usize;
        let raw = take(&mut cursor, len)?;
        dictionary
            .push(String::from_utf8(raw.to_vec()).map_err(|_| {
                ChainError::Internal("a dictionary entry is not UTF-8".to_string())
            })?);
    }

    let column_count = take_u32(&mut cursor)? as usize;
    let mut names = Vec::with_capacity(column_count);
    let mut types = Vec::with_capacity(column_count);
    for _ in 0..column_count {
        let len = take_u32(&mut cursor)? as usize;
        let raw = take(&mut cursor, len)?;
        names.push(
            String::from_utf8(raw.to_vec())
                .map_err(|_| ChainError::Internal("a column name is not UTF-8".to_string()))?,
        );
        let code = *take(&mut cursor, 1)?.first().ok_or_else(short)?;
        let _nullable = *take(&mut cursor, 1)?.first().ok_or_else(short)?;
        types.push(match code {
            0 => CellType::F64,
            1 => CellType::I64,
            2 => CellType::Timestamp,
            3 => CellType::Dictionary,
            4 => CellType::LabelMask,
            other => {
                return Err(ChainError::Internal(format!("unknown type code {other}")));
            }
        });
        // The descriptor is padded to four, not eight.
        let remainder = cursor % 4;
        if remainder != 0 {
            cursor += 4 - remainder;
        }
    }
    align(&mut cursor);

    let schema = BinarySchema {
        names: Vec::new(),
        types: types.clone(),
        dictionary: dictionary.clone(),
    };

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut closed = false;
    while cursor < bytes.len() {
        let marker = take_u32(&mut cursor)?;
        if marker == PACKED_FOOTER_SENTINEL {
            align(&mut cursor);
            let declared = take_i64(&mut cursor)? as usize;
            if declared != rows.len() {
                return Err(ChainError::Internal(format!(
                    "the footer declares {declared} rows, the blocks carried {}",
                    rows.len()
                )));
            }
            closed = true;
            break;
        }
        let row_count = marker as usize;
        align(&mut cursor);

        let mut block: Vec<Vec<String>> = vec![Vec::with_capacity(column_count); row_count];
        for cell_type in &types {
            let mut validity = vec![true; row_count];
            if cell_type.nullable() {
                let bitmap_len = row_count.div_ceil(8);
                let bitmap = take(&mut cursor, bitmap_len)?.to_vec();
                for (position, valid) in validity.iter_mut().enumerate() {
                    let byte = bitmap.get(position / 8).copied().unwrap_or(0);
                    *valid = byte & (1 << (position % 8)) != 0;
                }
                align(&mut cursor);
            }

            for (position, row) in block.iter_mut().enumerate() {
                let rendered = match cell_type {
                    CellType::F64 => {
                        let slice = take(&mut cursor, 8)?;
                        let array: [u8; 8] = slice.try_into().map_err(|_| short())?;
                        if validity.get(position).copied().unwrap_or(false) {
                            f64::from_le_bytes(array).to_string()
                        } else {
                            String::new()
                        }
                    }
                    CellType::I64 => take_i64(&mut cursor)?.to_string(),
                    CellType::Timestamp => {
                        let nanos = take_i64(&mut cursor)?;
                        DateTime::from_timestamp_nanos(nanos)
                            .to_utc()
                            .to_rfc3339_opts(SecondsFormat::Secs, true)
                    }
                    CellType::Dictionary => {
                        let index = take_u32(&mut cursor)? as usize;
                        dictionary.get(index).cloned().unwrap_or_default()
                    }
                    CellType::LabelMask => {
                        let slice = take(&mut cursor, 8)?;
                        let array: [u8; 8] = slice.try_into().map_err(|_| short())?;
                        schema.labels_of(u64::from_le_bytes(array)).join("|")
                    }
                };
                row.push(rendered);
            }
            align(&mut cursor);
        }
        rows.extend(block);
    }

    if !closed {
        return Err(ChainError::Internal(
            "the packed document has no footer; the download was truncated".to_string(),
        ));
    }
    Ok((names, rows))
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
                "weeklies".to_string(),
                "zero_dte".to_string(),
            ],
        }
    }

    /// The header of a writer, or a panic naming why it could not be written.
    fn header_of(writer: &PackedWriter) -> Vec<u8> {
        match writer.header() {
            Ok(header) => header,
            Err(error) => panic!("the header must encode: {error}"),
        }
    }

    /// The blocks a push completed.
    fn push_rows(writer: &mut PackedWriter, rows: Vec<Vec<Cell>>) -> Vec<Vec<u8>> {
        match writer.push(rows) {
            Ok(blocks) => blocks,
            Err(error) => panic!("the rows must encode: {error}"),
        }
    }

    /// The tail and footer a flush produced.
    fn flush_of(writer: &mut PackedWriter) -> Vec<u8> {
        match writer.flush() {
            Ok(Some(tail)) => tail,
            Ok(None) => Vec::new(),
            Err(error) => panic!("the flush must encode: {error}"),
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

    /// A mask round-trips to the label set it came from.
    ///
    /// This is what lets `packed` carry no text inside a block and still
    /// reproduce the `csv` column exactly.
    #[test]
    fn test_a_label_mask_round_trips() {
        let schema = schema();

        let labels = vec!["monthlies".to_string(), "zero_dte".to_string()];
        let mask = match schema.label_mask(&labels) {
            Ok(mask) => mask,
            Err(error) => panic!("the labels are the schema's own rule ids: {error}"),
        };

        assert_eq!(schema.labels_of(mask), vec!["monthlies", "zero_dte"]);
        assert_eq!(
            schema.labels_of(mask).join("|"),
            labels.join("|"),
            "the reconstruction must be the csv value character for character"
        );
    }

    /// An empty label set is a zero mask, and reads back empty.
    #[test]
    fn test_an_empty_label_set_is_a_zero_mask() {
        let schema = schema();

        match schema.label_mask(&[]) {
            Ok(mask) => assert_eq!(mask, 0),
            Err(error) => panic!("an empty label set must mask: {error}"),
        }
        assert!(schema.labels_of(0).is_empty());
    }

    /// Every payload in a block starts on an 8-byte boundary.
    ///
    /// The whole point of the format: an unaligned offset makes a browser's
    /// `new Float64Array(buffer, offset, count)` throw, so this is a
    /// correctness requirement rather than a nicety.
    #[test]
    fn test_every_payload_offset_is_aligned() {
        let schema = schema();
        let mut writer = PackedWriter::new(schema.clone(), 4);

        let header = header_of(&writer);
        assert_eq!(header.len() % ALIGNMENT, 0, "the header must end aligned");

        let blocks = push_rows(
            &mut writer,
            vec![
                row(0, Some(1.0), 1),
                row(1, None, 2),
                row(2, Some(3.0), 4),
                row(3, Some(4.0), 8),
            ],
        );
        assert_eq!(blocks.len(), 1, "four rows at a width of four is one block");

        // Walk the block the way a decoder does, asserting the offset of every
        // payload as it goes.
        let block = &blocks[0];
        let mut offset = 0_usize;
        let rows = u32::from_le_bytes(match block[0..4].try_into() {
            Ok(bytes) => bytes,
            Err(error) => panic!("the row count must be readable: {error}"),
        }) as usize;
        assert_eq!(rows, 4);
        offset += 4;
        offset += (ALIGNMENT - offset % ALIGNMENT) % ALIGNMENT;

        for cell_type in &schema.types {
            assert_eq!(
                offset % ALIGNMENT,
                0,
                "a payload must start aligned, got {offset}"
            );
            if cell_type.nullable() {
                let bitmap = rows.div_ceil(8);
                offset += bitmap + (ALIGNMENT - bitmap % ALIGNMENT) % ALIGNMENT;
                assert_eq!(offset % ALIGNMENT, 0, "a bitmap must be padded");
            }
            let values = cell_type.width() * rows;
            offset += values + (ALIGNMENT - values % ALIGNMENT) % ALIGNMENT;
        }
        assert_eq!(offset, block.len(), "the walk must consume the whole block");
    }

    /// A null writes a cleared validity bit and zero bytes, not a sentinel.
    #[test]
    fn test_a_null_is_a_cleared_bit_rather_than_a_sentinel() {
        let mut writer = PackedWriter::new(schema(), 2);
        let mut document = header_of(&writer);
        for block in push_rows(&mut writer, vec![row(0, Some(1.5), 0), row(1, None, 0)]) {
            document.extend(block);
        }
        document.extend(flush_of(&mut writer));

        // Read back through the documented layout rather than by counting bytes
        // from the end: a null must come out absent, not zero and not NaN.
        match decode_packed(&document) {
            Ok((_, rows)) => {
                assert_eq!(rows[0][4], "1.5");
                assert_eq!(rows[1][4], "", "a cleared validity bit reads as absent");
            }
            Err(error) => panic!("the document must decode: {error}"),
        }
    }

    /// Rows accumulate into blocks of exactly the configured width.
    #[test]
    fn test_rows_are_emitted_in_blocks_of_the_configured_width() {
        let mut writer = PackedWriter::new(schema(), 2);

        assert!(
            push_rows(&mut writer, vec![row(0, Some(1.0), 0)]).is_empty(),
            "a partial block waits"
        );
        assert_eq!(
            push_rows(&mut writer, vec![row(1, Some(2.0), 0)]).len(),
            1,
            "the second row completes it"
        );
        // The flush still emits the footer, which closes every document.
        assert!(
            !flush_of(&mut writer).is_empty(),
            "the footer closes the document even with nothing buffered"
        );
    }

    /// The same rows encode to the same bytes, every time.
    #[test]
    fn test_the_encoding_is_byte_identical_on_repeat() {
        let rows = vec![row(0, Some(1.0), 3), row(1, None, 0)];

        let mut first = PackedWriter::new(schema(), 2);
        let mut second = PackedWriter::new(schema(), 2);

        assert_eq!(header_of(&first), header_of(&second));
        assert_eq!(
            push_rows(&mut first, rows.clone()),
            push_rows(&mut second, rows)
        );
        assert_eq!(flush_of(&mut first), flush_of(&mut second));
    }

    /// The writer never holds more than one block, whatever it is fed.
    ///
    /// This is the memory claim the streaming contract rests on: an export's
    /// footprint is a function of the block width, not of the number of steps,
    /// so a hundred-thousand-step tape costs the same as a ten-step one.
    #[test]
    fn test_the_writer_never_buffers_more_than_one_block() {
        let block_rows = 8;
        let mut writer = PackedWriter::new(schema(), block_rows);

        for batch in 0..500 {
            push_rows(
                &mut writer,
                (0..7)
                    .map(|index| row(batch * 7 + index, Some(1.0), 0))
                    .collect(),
            );
            assert!(
                writer.buffered.len() < block_rows,
                "a full block must be emitted rather than held: {} buffered",
                writer.buffered.len()
            );
        }
        let _ = flush_of(&mut writer);
        assert!(
            writer.buffered.is_empty(),
            "the flush must empty the buffer"
        );
    }

    /// A rule id equal to the symbol still masks to itself.
    ///
    /// The symbol holds dictionary entry 0 and is never a label, so the mask
    /// starts at entry 1. Without that, a rule id that happens to equal the
    /// symbol would take bit 0 and be reconstructed AHEAD of every other rule
    /// id, which is a different `labels` string from the one the text
    /// encodings render — and both charsets allow the collision.
    #[test]
    fn test_a_rule_id_equal_to_the_symbol_keeps_its_place() {
        let schema = BinarySchema {
            names: vec!["labels"],
            types: vec![CellType::LabelMask],
            // The symbol is `weeklies`, and so is one of the rules.
            dictionary: vec![
                "weeklies".to_string(),
                "monthlies".to_string(),
                "weeklies".to_string(),
            ],
        };

        let labels = vec!["monthlies".to_string(), "weeklies".to_string()];
        let mask = match schema.label_mask(&labels) {
            Ok(mask) => mask,
            Err(error) => panic!("both labels are rule ids: {error}"),
        };

        assert_eq!(
            schema.labels_of(mask).join("|"),
            "monthlies|weeklies",
            "the order must be the text encodings' order"
        );
    }

    /// A label the schedule does not name is an error, not a dropped bit.
    #[test]
    fn test_an_unknown_label_is_refused() {
        match schema().label_mask(&["not_a_rule".to_string()]) {
            Ok(mask) => panic!("an unknown label must not mask silently, got {mask}"),
            Err(ChainError::Internal(message)) => {
                assert!(message.contains("not_a_rule"), "it must name it: {message}");
            }
            Err(error) => panic!("expected an internal failure, got {error:?}"),
        }
    }

    /// A truncated document is refused rather than read as a shorter tape.
    #[test]
    fn test_a_truncated_document_is_refused() {
        let mut writer = PackedWriter::new(schema(), 2);
        let mut document = header_of(&writer);
        for block in push_rows(
            &mut writer,
            vec![row(0, Some(1.0), 0), row(1, Some(2.0), 0)],
        ) {
            document.extend(block);
        }
        document.extend(flush_of(&mut writer));

        // Everything but the footer: what a dropped connection leaves behind.
        let truncated = &document[..document.len() - ALIGNMENT * 2];
        match decode_packed(truncated) {
            Ok((_, rows)) => panic!("a truncated document must not decode, got {rows:?}"),
            Err(ChainError::Internal(message)) => {
                assert!(message.contains("truncated"), "{message}");
            }
            Err(error) => panic!("expected an internal failure, got {error:?}"),
        }
    }

    /// A schedule too wide to mask is refused, rather than silently truncated.
    #[test]
    fn test_too_many_rules_are_refused() {
        let mut schema = schema();
        schema.dictionary = std::iter::once("SPX".to_string())
            .chain((0..MAX_LABEL_RULES + 1).map(|index| format!("rule_{index}")))
            .collect();

        match ensure_label_capacity(&schema) {
            Ok(()) => panic!("a schema wider than the mask must be refused"),
            Err(ChainError::Internal(message)) => {
                assert!(
                    message.contains(&MAX_LABEL_RULES.to_string()),
                    "the failure must name the bound: {message}"
                );
            }
            Err(error) => panic!("expected an internal failure, got {error:?}"),
        }
    }

    /// A document round-trips through the documented layout.
    ///
    /// The decoder walks the bytes the way the module doc describes rather than
    /// reading the writer's fields, so the two can only agree if the layout is
    /// what it says it is.
    #[test]
    fn test_a_document_round_trips_through_the_decoder() {
        let mut writer = PackedWriter::new(schema(), 2);
        let mut bytes = header_of(&writer);
        for block in push_rows(
            &mut writer,
            vec![
                row(0, Some(1.5), 0b101),
                row(1, None, 0),
                row(2, Some(-3.25), 0b001),
            ],
        ) {
            bytes.extend(block);
        }
        bytes.extend(flush_of(&mut writer));

        match decode_packed(&bytes) {
            Ok((names, rows)) => {
                assert_eq!(
                    names,
                    vec!["step", "simulated_at", "symbol", "labels", "price"]
                );
                assert_eq!(rows.len(), 3);
                assert_eq!(rows[0][0], "0");
                assert_eq!(rows[0][1], "2026-01-05T14:30:00Z");
                assert_eq!(rows[0][2], "SPX");
                assert_eq!(rows[0][3], "monthlies|zero_dte");
                assert_eq!(rows[0][4], "1.5");
                assert_eq!(rows[1][4], "", "a null decodes as an absent value");
                assert_eq!(rows[2][4], "-3.25");
                assert_eq!(rows[2][3], "monthlies");
            }
            Err(error) => panic!("the document must decode: {error}"),
        }
    }

    /// A row's cells line up with the schema that describes them.
    #[test]
    fn test_a_row_matches_its_schema() {
        let schema = schema();
        let row = row(0, Some(1.0), 0);

        assert_eq!(row.len(), schema.types.len());
        for (cell, expected) in row.iter().zip(&schema.types) {
            assert_eq!(cell.cell_type(), *expected);
        }
    }

    /// A schedule that fits is accepted.
    #[test]
    fn test_a_schedule_that_fits_is_accepted() {
        match ensure_label_capacity(&schema()) {
            Ok(()) => {}
            Err(error) => panic!("three rules must fit in a 64-bit mask: {error}"),
        }
    }
}
