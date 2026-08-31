//! A decoder for the packed export encoding, ported from the writer's own
//! reader so a test can compare its values with the text encodings.
//!
//! The format is documented in `src/api/rest/binary.rs`. This is deliberately
//! an INDEPENDENT reader rather than a call into the crate: the service keeps
//! its decoder private, and a test that used the writer's own code to check
//! the writer would prove less than one that reads the bytes the way a
//! consumer must.
//!
//! Every payload in a packed document starts at an 8-byte aligned offset,
//! because a browser reading one with `new Float64Array(buffer, offset, n)`
//! throws otherwise. That is a correctness property, not a nicety, so this
//! decoder records the offsets and [`PackedDocument::misaligned`] reports any
//! that were not.

/// The magic every packed document starts with.
const MAGIC: &[u8; 4] = b"OCSP";

/// The version this decoder understands.
const VERSION: u32 = 1;

/// The row count that marks the footer rather than a block.
const FOOTER_SENTINEL: u32 = u32::MAX;

/// Payload alignment, in bytes.
const ALIGNMENT: usize = 8;

/// A decoded packed document.
#[derive(Debug)]
pub struct PackedDocument {
    /// Column names, in order.
    pub columns: Vec<String>,
    /// Rows, rendered as the text encodings render them.
    pub rows: Vec<Vec<String>>,
    /// The row count the footer declared.
    pub declared_rows: u64,
    /// Payload offsets that were not 8-byte aligned.
    pub misaligned: Vec<usize>,
}

/// What a packed document can be wrong about.
#[derive(Debug)]
pub struct PackedError(pub String);

impl std::fmt::Display for PackedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for PackedError {}

/// The cell types the format carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellType {
    F64,
    I64,
    Timestamp,
    Dictionary,
    LabelMask,
}

impl CellType {
    /// Only `f64` columns carry a validity bitmap.
    fn nullable(self) -> bool {
        matches!(self, Self::F64)
    }
}

/// Decodes one packed document.
///
/// # Errors
///
/// [`PackedError`] when the bytes are not a packed document this version
/// understands, when they end early, or when the footer disagrees with the
/// blocks — which is how a truncated download is detected.
// `take!` advances the cursor on every use; the last advance in a document
// that ends on its footer is genuinely never read again, which is what the
// dead-store warning sees.
#[allow(unused_assignments)]
pub fn decode(bytes: &[u8]) -> Result<PackedDocument, PackedError> {
    let mut cursor = 0_usize;
    let mut misaligned = Vec::new();

    let short = || PackedError("the packed document ended early".to_string());

    // `cursor` is advanced by every one of these; the final assignment in a
    // document that ends on a footer is genuinely unread, which is what the
    // dead-store warning is about.
    macro_rules! take {
        ($count:expr) => {{
            let end = cursor.checked_add($count).ok_or_else(short)?;
            let slice = bytes.get(cursor..end).ok_or_else(short)?;
            cursor = end;
            slice
        }};
    }
    macro_rules! take_u32 {
        () => {{
            let slice = take!(4);
            let array: [u8; 4] = slice.try_into().map_err(|_| short())?;
            u32::from_le_bytes(array)
        }};
    }
    macro_rules! take_i64 {
        () => {{
            let slice = take!(8);
            let array: [u8; 8] = slice.try_into().map_err(|_| short())?;
            i64::from_le_bytes(array)
        }};
    }
    macro_rules! align {
        () => {{
            let remainder = cursor % ALIGNMENT;
            if remainder != 0 {
                cursor += ALIGNMENT - remainder;
            }
        }};
    }

    if take!(4) != MAGIC {
        return Err(PackedError("not a packed document".to_string()));
    }
    let version = take_u32!();
    if version != VERSION {
        return Err(PackedError(format!("unknown packed version {version}")));
    }
    let _block_rows = take_u32!();

    let dictionary_len = take_u32!() as usize;
    let mut dictionary = Vec::with_capacity(dictionary_len);
    for _ in 0..dictionary_len {
        let len = take_u32!() as usize;
        let raw = take!(len);
        dictionary.push(
            String::from_utf8(raw.to_vec())
                .map_err(|_| PackedError("a dictionary entry is not UTF-8".to_string()))?,
        );
    }

    let column_count = take_u32!() as usize;
    let mut columns = Vec::with_capacity(column_count);
    let mut types = Vec::with_capacity(column_count);
    for _ in 0..column_count {
        let len = take_u32!() as usize;
        let raw = take!(len);
        columns.push(
            String::from_utf8(raw.to_vec())
                .map_err(|_| PackedError("a column name is not UTF-8".to_string()))?,
        );
        let code = *take!(1).first().ok_or_else(short)?;
        let _nullable = *take!(1).first().ok_or_else(short)?;
        types.push(match code {
            0 => CellType::F64,
            1 => CellType::I64,
            2 => CellType::Timestamp,
            3 => CellType::Dictionary,
            4 => CellType::LabelMask,
            other => return Err(PackedError(format!("unknown type code {other}"))),
        });
        // A column descriptor is padded to four, not eight.
        let remainder = cursor % 4;
        if remainder != 0 {
            cursor += 4 - remainder;
        }
    }
    align!();

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut declared_rows = None;

    while cursor < bytes.len() {
        let marker = take_u32!();
        if marker == FOOTER_SENTINEL {
            align!();
            let declared = take_i64!();
            if declared < 0 {
                return Err(PackedError(format!("the footer declares {declared} rows")));
            }
            declared_rows = Some(declared as u64);
            break;
        }

        let row_count = marker as usize;
        align!();

        let mut block: Vec<Vec<String>> = vec![Vec::with_capacity(column_count); row_count];
        for cell_type in &types {
            let mut validity = vec![true; row_count];
            if cell_type.nullable() {
                let bitmap_len = row_count.div_ceil(8);
                let bitmap = take!(bitmap_len).to_vec();
                for (position, valid) in validity.iter_mut().enumerate() {
                    let byte = bitmap.get(position / 8).copied().unwrap_or(0);
                    *valid = byte & (1 << (position % 8)) != 0;
                }
                align!();
            }

            // Every payload must start aligned; that is what makes a
            // zero-copy typed-array view possible on the other side.
            if !cursor.is_multiple_of(ALIGNMENT) {
                misaligned.push(cursor);
            }

            for (position, row) in block.iter_mut().enumerate() {
                let rendered = match cell_type {
                    CellType::F64 => {
                        let slice = take!(8);
                        let array: [u8; 8] = slice.try_into().map_err(|_| short())?;
                        if validity.get(position).copied().unwrap_or(false) {
                            f64::from_le_bytes(array).to_string()
                        } else {
                            String::new()
                        }
                    }
                    CellType::I64 => take_i64!().to_string(),
                    CellType::Timestamp => render_timestamp(take_i64!()),
                    CellType::Dictionary => {
                        let index = take_u32!() as usize;
                        dictionary.get(index).cloned().unwrap_or_default()
                    }
                    CellType::LabelMask => {
                        let slice = take!(8);
                        let array: [u8; 8] = slice.try_into().map_err(|_| short())?;
                        labels_of(&dictionary, u64::from_le_bytes(array)).join("|")
                    }
                };
                row.push(rendered);
            }
            align!();
        }
        rows.extend(block);
    }

    let declared_rows = declared_rows.ok_or_else(|| {
        PackedError("the packed document has no footer; the download was truncated".to_string())
    })?;
    if declared_rows as usize != rows.len() {
        return Err(PackedError(format!(
            "the footer declares {declared_rows} rows, the blocks carried {}",
            rows.len()
        )));
    }

    Ok(PackedDocument {
        columns,
        rows,
        declared_rows,
        misaligned,
    })
}

/// Renders a nanosecond timestamp the way the text encodings render one.
fn render_timestamp(nanos: i64) -> String {
    let seconds = nanos.div_euclid(1_000_000_000);
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);

    // Days since the Unix epoch to a civil date, the standard algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3_600,
        (time % 3_600) / 60,
        time % 60
    )
}

/// The labels a mask names.
///
/// Entry zero of the dictionary is the symbol, so the mask's bit `i` names
/// entry `i + 1`.
fn labels_of(dictionary: &[String], mask: u64) -> Vec<String> {
    let mut labels = Vec::new();
    for (position, label) in dictionary.iter().skip(1).enumerate() {
        if position < 64 && mask & (1 << position) != 0 {
            labels.push(label.clone());
        }
    }
    labels
}
