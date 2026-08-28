//! The ClickHouse row types for the v2 snapshot tape, and the one place a
//! typed value becomes a column.
//!
//! # Why the columns are `Decimal(38, 28)`
//!
//! A persisted snapshot has to reconstruct to **exactly** what was generated —
//! that is what lets #49's exports read the warehouse instead of replaying the
//! simulation, and it is an acceptance criterion of issue #56. `Float64` cannot
//! promise that: a `Decimal` premium round-tripped through a binary float comes
//! back a different number. A fixed-point column can, as long as the scale is
//! at least as fine as the values written, and `rust_decimal` carries at most
//! 28 fractional digits — so 28 is the scale that never truncates.
//!
//! On the wire a `Decimal(38, 28)` is an `Int128` holding the value scaled by
//! `10^28`, which is what [`to_storage_decimal`] produces. The cost is a ten
//! digit ceiling on the integer part (`10^38 / 10^28`); a price above ten
//! billion is rejected with a typed error rather than silently wrapping. The
//! way back is not symmetric — see [`from_storage_decimal`], which has to undo
//! the scaling rather than read the mantissa verbatim.
//!
//! # Why the timestamps are `DateTime64(9)`
//!
//! Same reason. `start_at` arrives from a client as RFC 3339 and may carry
//! sub-millisecond precision, and every simulated instant is derived from it,
//! so millisecond columns would truncate a value the client can observe. Nanos
//! cover the years 1678–2262, which is past any simulated horizon; an instant
//! outside that range is a typed error.

use super::record::{ContractQuote, ContractSide, ExpirationRecord, QuoteRow, SnapshotRecord};
use crate::utils::ChainError;
use chrono::{DateTime, Utc};
use clickhouse::Row;
use optionstratlib::greeks::GreeksSnapshot;
use positive::Positive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The scale every decimal column is stored at.
///
/// Matches `rust_decimal`'s maximum, so no value written here is ever rounded.
pub(crate) const DECIMAL_SCALE: u32 = 28;

/// The table holding one row per materialised step.
pub(crate) const SNAPSHOTS_TABLE: &str = "simulation_snapshots";

/// The table holding one row per step, expiration and strike.
pub(crate) const QUOTES_TABLE: &str = "simulation_option_quotes";

/// One row of [`SNAPSHOTS_TABLE`]: the metadata and completion marker of one
/// step.
///
/// Written **after** every quote row of the same snapshot has been accepted, so
/// its presence is the signal that the snapshot is whole. `quote_count` is the
/// audit that goes with the signal.
#[derive(Debug, Clone, PartialEq, Row, Serialize, Deserialize)]
pub(crate) struct SnapshotMetaRow {
    /// The simulation's id, as text.
    pub(crate) simulation_id: String,
    /// The generation the snapshot was produced under.
    pub(crate) simulation_generation: u64,
    /// The 0-based step.
    pub(crate) step: u64,
    /// The deterministic snapshot identity, as text.
    pub(crate) snapshot_id: String,
    /// The simulated instant, as unix nanoseconds (`DateTime64(9)`).
    pub(crate) simulated_at: i64,
    /// The ticker symbol.
    pub(crate) symbol: String,
    /// The underlying price, scaled by `10^28`.
    pub(crate) underlying_price: i128,
    /// The base implied volatility, scaled by `10^28`.
    pub(crate) base_volatility: i128,
    /// How many quote rows belong to this snapshot.
    pub(crate) quote_count: u64,
    /// How many expirations belong to this snapshot.
    pub(crate) expiration_count: u64,
    /// The completion marker. Only ever written `true`; a reader that finds no
    /// row at all, or a row whose `quote_count` disagrees with the rows on
    /// disk, treats the snapshot as absent.
    pub(crate) complete: bool,
    /// Ingestion time, as unix milliseconds.
    ///
    /// Doubles as the `ReplacingMergeTree` version — the newest write of a
    /// coordinate wins — and as the anchor of the retention TTL. Two writes
    /// inside the same millisecond tie, which is harmless: a snapshot is
    /// deterministic, so the tied rows are identical.
    pub(crate) inserted_at_ms: u64,
}

/// One row of [`QUOTES_TABLE`]: one strike of one expiration at one step.
///
/// Snapshot-level values (`snapshot_id`, `simulated_at`, `symbol`) are repeated
/// on every row deliberately: they cost almost nothing after compression —
/// every row in a part shares them — and they let a contract history be served
/// without joining the metadata table.
#[derive(Debug, Clone, PartialEq, Row, Serialize, Deserialize)]
pub(crate) struct OptionQuoteRow {
    /// The simulation's id, as text.
    pub(crate) simulation_id: String,
    /// The generation the snapshot was produced under.
    pub(crate) simulation_generation: u64,
    /// The 0-based step.
    pub(crate) step: u64,
    /// The absolute expiration, as unix nanoseconds (`DateTime64(9)`).
    pub(crate) expires_at: i64,
    /// The strike, scaled by `10^28`.
    pub(crate) strike: i128,
    /// The deterministic snapshot identity, as text.
    pub(crate) snapshot_id: String,
    /// The simulated instant, as unix nanoseconds (`DateTime64(9)`).
    pub(crate) simulated_at: i64,
    /// The ticker symbol.
    pub(crate) symbol: String,
    /// Fractional days to expiration, scaled by `10^28`.
    pub(crate) days_to_expiration: i128,
    /// Every schedule rule this expiration satisfies, sorted.
    pub(crate) labels: Vec<String>,
    /// The per-strike implied volatility, scaled by `10^28`.
    pub(crate) implied_volatility: i128,
    /// The call bid, scaled by `10^28`.
    pub(crate) call_bid: Option<i128>,
    /// The call ask, scaled by `10^28`.
    pub(crate) call_ask: Option<i128>,
    /// The call mid, scaled by `10^28`.
    pub(crate) call_mid: Option<i128>,
    /// The put bid, scaled by `10^28`.
    pub(crate) put_bid: Option<i128>,
    /// The put ask, scaled by `10^28`.
    pub(crate) put_ask: Option<i128>,
    /// The put mid, scaled by `10^28`.
    pub(crate) put_mid: Option<i128>,
    /// The call delta, scaled by `10^28`.
    pub(crate) delta_call: Option<i128>,
    /// The put delta, scaled by `10^28`.
    pub(crate) delta_put: Option<i128>,
    /// Gamma, shared by both styles: upstream's convenience mirror, kept
    /// alongside the per-style snapshot columns. Scaled by `10^28`.
    pub(crate) gamma: Option<i128>,
    /// The call's `gamma`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) gamma_call: Option<i128>,
    /// The put's `gamma`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) gamma_put: Option<i128>,
    /// The call's `theta`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) theta_call: Option<i128>,
    /// The put's `theta`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) theta_put: Option<i128>,
    /// The call's `vega`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) vega_call: Option<i128>,
    /// The put's `vega`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) vega_put: Option<i128>,
    /// The call's `rho`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) rho_call: Option<i128>,
    /// The put's `rho`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) rho_put: Option<i128>,
    /// The call's `rho_d`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) rho_d_call: Option<i128>,
    /// The put's `rho_d`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) rho_d_put: Option<i128>,
    /// The call's `alpha`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) alpha_call: Option<i128>,
    /// The put's `alpha`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) alpha_put: Option<i128>,
    /// The call's `vanna`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) vanna_call: Option<i128>,
    /// The put's `vanna`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) vanna_put: Option<i128>,
    /// The call's `vomma`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) vomma_call: Option<i128>,
    /// The put's `vomma`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) vomma_put: Option<i128>,
    /// The call's `veta`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) veta_call: Option<i128>,
    /// The put's `veta`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) veta_put: Option<i128>,
    /// The call's `charm`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) charm_call: Option<i128>,
    /// The put's `charm`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) charm_put: Option<i128>,
    /// The call's `color`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) color_call: Option<i128>,
    /// The put's `color`, scaled by `10^28`. NULL when the strike has no
    /// computable snapshot, or when the row predates issue #74.
    pub(crate) color_put: Option<i128>,
    /// Ingestion time, as unix milliseconds. See [`SnapshotMetaRow`].
    pub(crate) inserted_at_ms: u64,
}

/// The metadata columns a read selects, in the order the query lists them.
#[derive(Debug, Clone, PartialEq, Row, Deserialize)]
pub(crate) struct SnapshotMetaReadRow {
    /// The 0-based step.
    pub(crate) step: u64,
    /// The deterministic snapshot identity, as text.
    pub(crate) snapshot_id: String,
    /// The simulated instant, as unix nanoseconds.
    pub(crate) simulated_at: i64,
    /// The ticker symbol.
    pub(crate) symbol: String,
    /// The underlying price, scaled by `10^28`.
    pub(crate) underlying_price: i128,
    /// The base implied volatility, scaled by `10^28`.
    pub(crate) base_volatility: i128,
    /// How many quote rows the writer said belong to this snapshot.
    pub(crate) quote_count: u64,
}

/// The quote columns a whole-snapshot read selects, in query order.
#[derive(Debug, Clone, PartialEq, Row, Deserialize)]
pub(crate) struct QuoteReadRow {
    /// The 0-based step.
    pub(crate) step: u64,
    /// The absolute expiration, as unix nanoseconds.
    pub(crate) expires_at: i64,
    /// Fractional days to expiration, scaled by `10^28`.
    pub(crate) days_to_expiration: i128,
    /// Every schedule rule this expiration satisfies, sorted.
    pub(crate) labels: Vec<String>,
    /// The strike, scaled by `10^28`.
    pub(crate) strike: i128,
    /// The per-strike implied volatility, scaled by `10^28`.
    pub(crate) implied_volatility: i128,
    /// The call bid, scaled by `10^28`.
    pub(crate) call_bid: Option<i128>,
    /// The call ask, scaled by `10^28`.
    pub(crate) call_ask: Option<i128>,
    /// The call mid, scaled by `10^28`.
    pub(crate) call_mid: Option<i128>,
    /// The put bid, scaled by `10^28`.
    pub(crate) put_bid: Option<i128>,
    /// The put ask, scaled by `10^28`.
    pub(crate) put_ask: Option<i128>,
    /// The put mid, scaled by `10^28`.
    pub(crate) put_mid: Option<i128>,
    /// The call delta, scaled by `10^28`.
    pub(crate) delta_call: Option<i128>,
    /// The put delta, scaled by `10^28`.
    pub(crate) delta_put: Option<i128>,
    /// Gamma, shared by both styles. Scaled by `10^28`.
    pub(crate) gamma: Option<i128>,
    /// The call's `gamma`, scaled by `10^28`.
    pub(crate) gamma_call: Option<i128>,
    /// The put's `gamma`, scaled by `10^28`.
    pub(crate) gamma_put: Option<i128>,
    /// The call's `theta`, scaled by `10^28`.
    pub(crate) theta_call: Option<i128>,
    /// The put's `theta`, scaled by `10^28`.
    pub(crate) theta_put: Option<i128>,
    /// The call's `vega`, scaled by `10^28`.
    pub(crate) vega_call: Option<i128>,
    /// The put's `vega`, scaled by `10^28`.
    pub(crate) vega_put: Option<i128>,
    /// The call's `rho`, scaled by `10^28`.
    pub(crate) rho_call: Option<i128>,
    /// The put's `rho`, scaled by `10^28`.
    pub(crate) rho_put: Option<i128>,
    /// The call's `rho_d`, scaled by `10^28`.
    pub(crate) rho_d_call: Option<i128>,
    /// The put's `rho_d`, scaled by `10^28`.
    pub(crate) rho_d_put: Option<i128>,
    /// The call's `alpha`, scaled by `10^28`.
    pub(crate) alpha_call: Option<i128>,
    /// The put's `alpha`, scaled by `10^28`.
    pub(crate) alpha_put: Option<i128>,
    /// The call's `vanna`, scaled by `10^28`.
    pub(crate) vanna_call: Option<i128>,
    /// The put's `vanna`, scaled by `10^28`.
    pub(crate) vanna_put: Option<i128>,
    /// The call's `vomma`, scaled by `10^28`.
    pub(crate) vomma_call: Option<i128>,
    /// The put's `vomma`, scaled by `10^28`.
    pub(crate) vomma_put: Option<i128>,
    /// The call's `veta`, scaled by `10^28`.
    pub(crate) veta_call: Option<i128>,
    /// The put's `veta`, scaled by `10^28`.
    pub(crate) veta_put: Option<i128>,
    /// The call's `charm`, scaled by `10^28`.
    pub(crate) charm_call: Option<i128>,
    /// The put's `charm`, scaled by `10^28`.
    pub(crate) charm_put: Option<i128>,
    /// The call's `color`, scaled by `10^28`.
    pub(crate) color_call: Option<i128>,
    /// The put's `color`, scaled by `10^28`.
    pub(crate) color_put: Option<i128>,
}

/// The columns a contract-history read selects, in query order.
///
/// The side is chosen by the query — `call_bid AS bid` or `put_bid AS bid` —
/// so one row type serves both.
#[derive(Debug, Clone, PartialEq, Row, Deserialize)]
pub(crate) struct ContractReadRow {
    /// The 0-based step.
    pub(crate) step: u64,
    /// The simulated instant, as unix nanoseconds.
    pub(crate) simulated_at: i64,
    /// The absolute expiration, as unix nanoseconds.
    pub(crate) expires_at: i64,
    /// Fractional days to expiration, scaled by `10^28`.
    pub(crate) days_to_expiration: i128,
    /// The strike, scaled by `10^28`.
    pub(crate) strike: i128,
    /// The per-strike implied volatility, scaled by `10^28`.
    pub(crate) implied_volatility: i128,
    /// The selected side's bid, scaled by `10^28`.
    pub(crate) bid: Option<i128>,
    /// The selected side's ask, scaled by `10^28`.
    pub(crate) ask: Option<i128>,
    /// The selected side's mid, scaled by `10^28`.
    pub(crate) mid: Option<i128>,
    /// The selected side's delta, scaled by `10^28`.
    pub(crate) delta: Option<i128>,
    /// The shared gamma mirror, scaled by `10^28`. What a row written before
    /// issue #74 carries, and what the `gamma` field of a quote still reports.
    pub(crate) gamma: Option<i128>,
    /// The selected side's snapshot `gamma`, scaled by `10^28`. Equal to the
    /// mirror where both exist; separate so a pre-#74 row can carry one and not
    /// the other.
    pub(crate) snapshot_gamma: Option<i128>,
    /// The selected side's `theta`, scaled by `10^28`.
    pub(crate) theta: Option<i128>,
    /// The selected side's `vega`, scaled by `10^28`.
    pub(crate) vega: Option<i128>,
    /// The selected side's `rho`, scaled by `10^28`.
    pub(crate) rho: Option<i128>,
    /// The selected side's `rho_d`, scaled by `10^28`.
    pub(crate) rho_d: Option<i128>,
    /// The selected side's `alpha`, scaled by `10^28`.
    pub(crate) alpha: Option<i128>,
    /// The selected side's `vanna`, scaled by `10^28`.
    pub(crate) vanna: Option<i128>,
    /// The selected side's `vomma`, scaled by `10^28`.
    pub(crate) vomma: Option<i128>,
    /// The selected side's `veta`, scaled by `10^28`.
    pub(crate) veta: Option<i128>,
    /// The selected side's `charm`, scaled by `10^28`.
    pub(crate) charm: Option<i128>,
    /// The selected side's `color`, scaled by `10^28`.
    pub(crate) color: Option<i128>,
}

/// Scales a decimal into the `Int128` a `Decimal(38, 28)` column carries.
///
/// # Errors
///
/// Returns [`ChainError::ClickHouseError`] naming `field` when the value's
/// integer part is too large for the column — above roughly ten billion — since
/// truncating a price is never the right answer.
pub(crate) fn to_storage_decimal(value: Decimal, field: &str) -> Result<i128, ChainError> {
    // `rust_decimal` caps its scale at 28, so the shift is always defined; the
    // checked form keeps that assumption honest if the crate ever changes.
    let shift = DECIMAL_SCALE.checked_sub(value.scale()).ok_or_else(|| {
        ChainError::ClickHouseError(format!(
            "{field} has scale {} beyond the storable {DECIMAL_SCALE}",
            value.scale()
        ))
    })?;
    let factor = 10_i128.checked_pow(shift).ok_or_else(|| {
        ChainError::ClickHouseError(format!("{field} needs an unrepresentable scale factor"))
    })?;

    value.mantissa().checked_mul(factor).ok_or_else(|| {
        ChainError::ClickHouseError(format!(
            "{field} value {value} does not fit a Decimal(38, {DECIMAL_SCALE}) column"
        ))
    })
}

/// Reads back the `Int128` of a `Decimal(38, 28)` column.
///
/// # Why this strips zeros before rebuilding
///
/// A `Decimal` is a 96-bit mantissa with a scale, so it holds at most ~29
/// significant digits — it can represent `5000.25`, but *not* the same number
/// written as `5000.2500000000000000000000000000`, which needs 32. Scaling to
/// 28 on the way in produces exactly that form, so rebuilding the mantissa
/// verbatim would reject values the column stores perfectly well.
///
/// Stripping the trailing zeros first undoes the scaling: it recovers the
/// smallest `(mantissa, scale)` pair for the value, which is representable
/// precisely because the value came from a `Decimal` in the first place. The
/// result is numerically identical either way — `Decimal` compares by value,
/// not by representation.
///
/// # Errors
///
/// Returns [`ChainError::ClickHouseError`] naming `field` when the stored value
/// still needs more significant digits than a `Decimal` has — a column written
/// by something other than this crate.
pub(crate) fn from_storage_decimal(raw: i128, field: &str) -> Result<Decimal, ChainError> {
    let mut mantissa = raw;
    let mut scale = DECIMAL_SCALE;
    while scale > 0 && mantissa % 10 == 0 {
        mantissa /= 10;
        scale -= 1;
    }

    Decimal::try_from_i128_with_scale(mantissa, scale).map_err(|error| {
        ChainError::ClickHouseError(format!("{field} holds an unreadable decimal: {error}"))
    })
}

/// Scales a positive value into its column form.
///
/// # Errors
///
/// As [`to_storage_decimal`].
pub(crate) fn to_storage_positive(value: Positive, field: &str) -> Result<i128, ChainError> {
    to_storage_decimal(value.to_dec(), field)
}

/// Reads back a positive value from its column form.
///
/// # Errors
///
/// Returns [`ChainError::ClickHouseError`] naming `field` when the stored value
/// is unreadable or negative — a negative premium means the column was written
/// by something that is not this crate.
pub(crate) fn from_storage_positive(raw: i128, field: &str) -> Result<Positive, ChainError> {
    let value = from_storage_decimal(raw, field)?;
    Positive::new_decimal(value).map_err(|error| {
        ChainError::ClickHouseError(format!("{field} holds a non-positive value: {error}"))
    })
}

/// Scales an optional decimal into its column form.
///
/// # Errors
///
/// As [`to_storage_decimal`].
fn to_storage_optional(value: Option<Decimal>, field: &str) -> Result<Option<i128>, ChainError> {
    value
        .map(|value| to_storage_decimal(value, field))
        .transpose()
}

/// Scales an optional positive value into its column form.
///
/// # Errors
///
/// As [`to_storage_decimal`].
fn to_storage_optional_positive(
    value: Option<Positive>,
    field: &str,
) -> Result<Option<i128>, ChainError> {
    value
        .map(|value| to_storage_positive(value, field))
        .transpose()
}

/// Reads back an optional decimal.
///
/// # Errors
///
/// As [`from_storage_decimal`].
fn from_storage_optional(raw: Option<i128>, field: &str) -> Result<Option<Decimal>, ChainError> {
    raw.map(|raw| from_storage_decimal(raw, field)).transpose()
}

/// Reads back an optional positive value.
///
/// # Errors
///
/// As [`from_storage_positive`].
fn from_storage_optional_positive(
    raw: Option<i128>,
    field: &str,
) -> Result<Option<Positive>, ChainError> {
    raw.map(|raw| from_storage_positive(raw, field)).transpose()
}

/// Converts an instant into the unix nanoseconds a `DateTime64(9)` carries.
///
/// # Errors
///
/// Returns [`ChainError::ClickHouseError`] naming `field` when the instant
/// falls outside 1678–2262, which nanosecond ticks cannot address.
pub(crate) fn to_storage_instant(value: DateTime<Utc>, field: &str) -> Result<i64, ChainError> {
    value.timestamp_nanos_opt().ok_or_else(|| {
        ChainError::ClickHouseError(format!(
            "{field} instant {value} is outside the storable 1678-2262 range"
        ))
    })
}

/// Reads back an instant from unix nanoseconds.
///
/// Infallible, unlike its counterpart: every `i64` tick names a real instant,
/// so there is nothing left to reject on the way back.
#[must_use]
pub(crate) fn from_storage_instant(raw: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_nanos(raw)
}

/// Builds the completion marker for a snapshot.
///
/// # Errors
///
/// Returns [`ChainError::ClickHouseError`] when a value does not fit its
/// column, or [`ChainError::Validation`] when the step or a count is too large
/// for its column.
pub(crate) fn meta_row(
    record: &SnapshotRecord,
    inserted_at_ms: u64,
) -> Result<SnapshotMetaRow, ChainError> {
    Ok(SnapshotMetaRow {
        simulation_id: record.simulation.to_string(),
        simulation_generation: record.generation,
        step: to_storage_count(record.step, "step")?,
        snapshot_id: record.snapshot_id().to_string(),
        simulated_at: to_storage_instant(record.simulated_at, "simulated_at")?,
        symbol: record.symbol.clone(),
        underlying_price: to_storage_positive(record.spot, "underlying_price")?,
        base_volatility: to_storage_positive(record.base_volatility, "base_volatility")?,
        quote_count: to_storage_count(record.quote_count(), "quote_count")?,
        expiration_count: to_storage_count(record.expirations.len(), "expiration_count")?,
        complete: true,
        inserted_at_ms,
    })
}

/// Flattens a snapshot into its quote rows, in storage order.
///
/// The order is the record's own — expirations ascending, strikes ascending —
/// which is also the table's sorting key, so one snapshot inserts as one
/// already-sorted block.
///
/// # Errors
///
/// As [`meta_row`].
pub(crate) fn quote_rows(
    record: &SnapshotRecord,
    inserted_at_ms: u64,
) -> Result<Vec<OptionQuoteRow>, ChainError> {
    let simulation_id = record.simulation.to_string();
    let snapshot_id = record.snapshot_id().to_string();
    let simulated_at = to_storage_instant(record.simulated_at, "simulated_at")?;
    let step = to_storage_count(record.step, "step")?;

    let mut rows = Vec::with_capacity(record.quote_count());
    for expiration in &record.expirations {
        let expires_at = to_storage_instant(expiration.expires_at, "expires_at")?;
        let days_to_expiration =
            to_storage_positive(expiration.days_to_expiration, "days_to_expiration")?;

        for quote in &expiration.quotes {
            rows.push(OptionQuoteRow {
                simulation_id: simulation_id.clone(),
                simulation_generation: record.generation,
                step,
                expires_at,
                strike: to_storage_positive(quote.strike, "strike")?,
                snapshot_id: snapshot_id.clone(),
                simulated_at,
                symbol: record.symbol.clone(),
                days_to_expiration,
                labels: expiration.labels.clone(),
                implied_volatility: to_storage_positive(
                    quote.implied_volatility,
                    "implied_volatility",
                )?,
                call_bid: to_storage_optional_positive(quote.call_bid, "call_bid")?,
                call_ask: to_storage_optional_positive(quote.call_ask, "call_ask")?,
                call_mid: to_storage_optional_positive(quote.call_mid, "call_mid")?,
                put_bid: to_storage_optional_positive(quote.put_bid, "put_bid")?,
                put_ask: to_storage_optional_positive(quote.put_ask, "put_ask")?,
                put_mid: to_storage_optional_positive(quote.put_mid, "put_mid")?,
                delta_call: to_storage_optional(quote.delta_call, "delta_call")?,
                delta_put: to_storage_optional(quote.delta_put, "delta_put")?,
                gamma: to_storage_optional(quote.gamma, "gamma")?,
                gamma_call: to_storage_optional(
                    quote.greeks_call.as_ref().map(|greeks| greeks.gamma),
                    "gamma_call",
                )?,
                gamma_put: to_storage_optional(
                    quote.greeks_put.as_ref().map(|greeks| greeks.gamma),
                    "gamma_put",
                )?,
                theta_call: to_storage_optional(
                    quote.greeks_call.as_ref().map(|greeks| greeks.theta),
                    "theta_call",
                )?,
                theta_put: to_storage_optional(
                    quote.greeks_put.as_ref().map(|greeks| greeks.theta),
                    "theta_put",
                )?,
                vega_call: to_storage_optional(
                    quote.greeks_call.as_ref().map(|greeks| greeks.vega),
                    "vega_call",
                )?,
                vega_put: to_storage_optional(
                    quote.greeks_put.as_ref().map(|greeks| greeks.vega),
                    "vega_put",
                )?,
                rho_call: to_storage_optional(
                    quote.greeks_call.as_ref().and_then(|greeks| greeks.rho),
                    "rho_call",
                )?,
                rho_put: to_storage_optional(
                    quote.greeks_put.as_ref().and_then(|greeks| greeks.rho),
                    "rho_put",
                )?,
                rho_d_call: to_storage_optional(
                    quote.greeks_call.as_ref().and_then(|greeks| greeks.rho_d),
                    "rho_d_call",
                )?,
                rho_d_put: to_storage_optional(
                    quote.greeks_put.as_ref().and_then(|greeks| greeks.rho_d),
                    "rho_d_put",
                )?,
                alpha_call: to_storage_optional(
                    quote.greeks_call.as_ref().and_then(|greeks| greeks.alpha),
                    "alpha_call",
                )?,
                alpha_put: to_storage_optional(
                    quote.greeks_put.as_ref().and_then(|greeks| greeks.alpha),
                    "alpha_put",
                )?,
                vanna_call: to_storage_optional(
                    quote.greeks_call.as_ref().map(|greeks| greeks.vanna),
                    "vanna_call",
                )?,
                vanna_put: to_storage_optional(
                    quote.greeks_put.as_ref().map(|greeks| greeks.vanna),
                    "vanna_put",
                )?,
                vomma_call: to_storage_optional(
                    quote.greeks_call.as_ref().map(|greeks| greeks.vomma),
                    "vomma_call",
                )?,
                vomma_put: to_storage_optional(
                    quote.greeks_put.as_ref().map(|greeks| greeks.vomma),
                    "vomma_put",
                )?,
                veta_call: to_storage_optional(
                    quote.greeks_call.as_ref().map(|greeks| greeks.veta),
                    "veta_call",
                )?,
                veta_put: to_storage_optional(
                    quote.greeks_put.as_ref().map(|greeks| greeks.veta),
                    "veta_put",
                )?,
                charm_call: to_storage_optional(
                    quote.greeks_call.as_ref().map(|greeks| greeks.charm),
                    "charm_call",
                )?,
                charm_put: to_storage_optional(
                    quote.greeks_put.as_ref().map(|greeks| greeks.charm),
                    "charm_put",
                )?,
                color_call: to_storage_optional(
                    quote.greeks_call.as_ref().map(|greeks| greeks.color),
                    "color_call",
                )?,
                color_put: to_storage_optional(
                    quote.greeks_put.as_ref().map(|greeks| greeks.color),
                    "color_put",
                )?,
                inserted_at_ms,
            });
        }
    }

    Ok(rows)
}

/// Rebuilds a snapshot from the rows one step selected.
///
/// `quotes` must be the rows of exactly that step, ordered by expiration and
/// then strike — the order every read query asks for. Rows sharing an
/// expiration are adjacent, which is what lets the grouping be a single pass.
///
/// # Errors
///
/// Returns [`ChainError::ClickHouseError`] when a column is unreadable, or when
/// the stored `snapshot_id` is not the one this coordinate derives — a row
/// written under a different identity scheme, which must never be served as if
/// it were this simulation's.
pub(crate) fn record_from_rows(
    simulation: Uuid,
    generation: u64,
    meta: &SnapshotMetaReadRow,
    quotes: &[QuoteReadRow],
) -> Result<SnapshotRecord, ChainError> {
    let step = usize::try_from(meta.step).map_err(|_| {
        ChainError::ClickHouseError(format!("step {} is not addressable here", meta.step))
    })?;

    let expected_id = super::record::snapshot_id(simulation, generation, step);
    if meta.snapshot_id != expected_id.to_string() {
        return Err(ChainError::ClickHouseError(format!(
            "snapshot {simulation}/{generation}/{step} is stored under identity {} instead of \
             {expected_id}",
            meta.snapshot_id
        )));
    }

    let mut expirations: Vec<ExpirationRecord> = Vec::new();
    for row in quotes {
        let expires_at = from_storage_instant(row.expires_at);
        let quote = quote_from_row(row)?;

        match expirations.last_mut() {
            Some(current) if current.expires_at == expires_at => current.quotes.push(quote),
            _ => expirations.push(ExpirationRecord {
                expires_at,
                days_to_expiration: from_storage_positive(
                    row.days_to_expiration,
                    "days_to_expiration",
                )?,
                labels: row.labels.clone(),
                quotes: vec![quote],
            }),
        }
    }

    Ok(SnapshotRecord {
        simulation,
        generation,
        step,
        simulated_at: from_storage_instant(meta.simulated_at),
        symbol: meta.symbol.clone(),
        spot: from_storage_positive(meta.underlying_price, "underlying_price")?,
        base_volatility: from_storage_positive(meta.base_volatility, "base_volatility")?,
        expirations,
    })
}

/// The column names of one option style, already suffixed.
///
/// Static, so naming a column in an error costs nothing on a read path that
/// touches twelve of them per style per row.
struct GreekColumns {
    /// The style's `delta` column.
    delta: &'static str,
    /// The style's `gamma` column.
    gamma: &'static str,
    /// The style's `theta` column.
    theta: &'static str,
    /// The style's `vega` column.
    vega: &'static str,
    /// The style's `rho` column.
    rho: &'static str,
    /// The style's `rho_d` column.
    rho_d: &'static str,
    /// The style's `alpha` column.
    alpha: &'static str,
    /// The style's `vanna` column.
    vanna: &'static str,
    /// The style's `vomma` column.
    vomma: &'static str,
    /// The style's `veta` column.
    veta: &'static str,
    /// The style's `charm` column.
    charm: &'static str,
    /// The style's `color` column.
    color: &'static str,
}

/// The call's column names.
const CALL_COLUMNS: GreekColumns = GreekColumns {
    delta: "delta_call",
    gamma: "gamma_call",
    theta: "theta_call",
    vega: "vega_call",
    rho: "rho_call",
    rho_d: "rho_d_call",
    alpha: "alpha_call",
    vanna: "vanna_call",
    vomma: "vomma_call",
    veta: "veta_call",
    charm: "charm_call",
    color: "color_call",
};

/// The put's column names.
const PUT_COLUMNS: GreekColumns = GreekColumns {
    delta: "delta_put",
    gamma: "gamma_put",
    theta: "theta_put",
    vega: "vega_put",
    rho: "rho_put",
    rho_d: "rho_d_put",
    alpha: "alpha_put",
    vanna: "vanna_put",
    vomma: "vomma_put",
    veta: "veta_put",
    charm: "charm_put",
    color: "color_put",
};

/// Rebuilds one style's greek snapshot from its stored columns.
///
/// `delta` is read from `delta_call` / `delta_put`, the columns that also serve
/// as upstream's convenience mirrors, so that number is never written twice and
/// cannot drift between two homes.
///
/// `gamma` is not the same story and deliberately so: it reads the dedicated
/// `gamma_call` / `gamma_put`, while the shared `gamma` mirror keeps its own
/// column. The duplication buys the pre-#74 rows, which have only the mirror,
/// and a future non-European style where upstream's gamma stops being shared.
///
/// Returns `Ok(None)` when any value the snapshot requires is NULL, which is
/// both the degenerate-strike case and the row-written-before-issue-#74 case.
/// A partial snapshot is never invented: three of the twelve values are
/// genuinely optional upstream, and defaulting the other nine to zero would
/// turn "not computed" into a number a client would trade on.
///
/// # Errors
///
/// As [`from_storage_decimal`].
fn greeks_from_columns(
    stored: &StoredGreeks,
    names: &GreekColumns,
) -> Result<Option<GreeksSnapshot>, ChainError> {
    // The names are already suffixed with the side, so nothing is formatted
    // here: this runs per greek, per style, per row, and a max-size read is a
    // million rows.
    let column = |raw: Option<i128>, field: &str| -> Result<Option<Decimal>, ChainError> {
        from_storage_optional(raw, field)
    };

    let (
        Some(delta),
        Some(gamma),
        Some(theta),
        Some(vega),
        Some(vanna),
        Some(vomma),
        Some(veta),
        Some(charm),
        Some(color),
    ) = (
        column(stored.delta, names.delta)?,
        column(stored.gamma, names.gamma)?,
        column(stored.theta, names.theta)?,
        column(stored.vega, names.vega)?,
        column(stored.vanna, names.vanna)?,
        column(stored.vomma, names.vomma)?,
        column(stored.veta, names.veta)?,
        column(stored.charm, names.charm)?,
        column(stored.color, names.color)?,
    )
    else {
        return Ok(None);
    };

    Ok(Some(GreeksSnapshot {
        delta,
        gamma,
        theta,
        vega,
        rho: column(stored.rho, names.rho)?,
        rho_d: column(stored.rho_d, names.rho_d)?,
        alpha: column(stored.alpha, names.alpha)?,
        vanna,
        vomma,
        veta,
        charm,
        color,
    }))
}

/// One style's greek columns, in their stored form.
///
/// A struct rather than twelve arguments: the values are positional and
/// interchangeable in type, which is exactly the shape that invites a silent
/// transposition at a call site.
struct StoredGreeks {
    /// The style's delta column.
    delta: Option<i128>,
    /// The style's gamma column.
    gamma: Option<i128>,
    /// The style's theta column.
    theta: Option<i128>,
    /// The style's vega column.
    vega: Option<i128>,
    /// The style's rho column.
    rho: Option<i128>,
    /// The style's rho_d column.
    rho_d: Option<i128>,
    /// The style's alpha column.
    alpha: Option<i128>,
    /// The style's vanna column.
    vanna: Option<i128>,
    /// The style's vomma column.
    vomma: Option<i128>,
    /// The style's veta column.
    veta: Option<i128>,
    /// The style's charm column.
    charm: Option<i128>,
    /// The style's color column.
    color: Option<i128>,
}

/// Rebuilds one strike from its stored row.
///
/// # Errors
///
/// As [`from_storage_decimal`].
fn quote_from_row(row: &QuoteReadRow) -> Result<QuoteRow, ChainError> {
    let call_columns = StoredGreeks {
        delta: row.delta_call,
        gamma: row.gamma_call,
        theta: row.theta_call,
        vega: row.vega_call,
        rho: row.rho_call,
        rho_d: row.rho_d_call,
        alpha: row.alpha_call,
        vanna: row.vanna_call,
        vomma: row.vomma_call,
        veta: row.veta_call,
        charm: row.charm_call,
        color: row.color_call,
    };
    let put_columns = StoredGreeks {
        delta: row.delta_put,
        gamma: row.gamma_put,
        theta: row.theta_put,
        vega: row.vega_put,
        rho: row.rho_put,
        rho_d: row.rho_d_put,
        alpha: row.alpha_put,
        vanna: row.vanna_put,
        vomma: row.vomma_put,
        veta: row.veta_put,
        charm: row.charm_put,
        color: row.color_put,
    };

    Ok(QuoteRow {
        strike: from_storage_positive(row.strike, "strike")?,
        implied_volatility: from_storage_positive(row.implied_volatility, "implied_volatility")?,
        call_bid: from_storage_optional_positive(row.call_bid, "call_bid")?,
        call_ask: from_storage_optional_positive(row.call_ask, "call_ask")?,
        call_mid: from_storage_optional_positive(row.call_mid, "call_mid")?,
        put_bid: from_storage_optional_positive(row.put_bid, "put_bid")?,
        put_ask: from_storage_optional_positive(row.put_ask, "put_ask")?,
        put_mid: from_storage_optional_positive(row.put_mid, "put_mid")?,
        delta_call: from_storage_optional(row.delta_call, "delta_call")?,
        delta_put: from_storage_optional(row.delta_put, "delta_put")?,
        gamma: from_storage_optional(row.gamma, "gamma")?,
        greeks_call: greeks_from_columns(&call_columns, &CALL_COLUMNS)?,
        greeks_put: greeks_from_columns(&put_columns, &PUT_COLUMNS)?,
    })
}

/// Rebuilds one point of a contract's history.
///
/// # Errors
///
/// As [`from_storage_decimal`].
pub(crate) fn contract_quote_from_row(
    row: &ContractReadRow,
    side: ContractSide,
) -> Result<ContractQuote, ChainError> {
    let columns = StoredGreeks {
        delta: row.delta,
        gamma: row.snapshot_gamma,
        theta: row.theta,
        vega: row.vega,
        rho: row.rho,
        rho_d: row.rho_d,
        alpha: row.alpha,
        vanna: row.vanna,
        vomma: row.vomma,
        veta: row.veta,
        charm: row.charm,
        color: row.color,
    };

    Ok(ContractQuote {
        step: usize::try_from(row.step).map_err(|_| {
            ChainError::ClickHouseError(format!("step {} is not addressable here", row.step))
        })?,
        simulated_at: from_storage_instant(row.simulated_at),
        expires_at: from_storage_instant(row.expires_at),
        days_to_expiration: from_storage_positive(row.days_to_expiration, "days_to_expiration")?,
        strike: from_storage_positive(row.strike, "strike")?,
        side,
        implied_volatility: from_storage_positive(row.implied_volatility, "implied_volatility")?,
        bid: from_storage_optional_positive(row.bid, "bid")?,
        ask: from_storage_optional_positive(row.ask, "ask")?,
        mid: from_storage_optional_positive(row.mid, "mid")?,
        delta: from_storage_optional(row.delta, "delta")?,
        gamma: from_storage_optional(row.gamma, "gamma")?,
        greeks: greeks_from_columns(
            &columns,
            match side {
                ContractSide::Call => &CALL_COLUMNS,
                ContractSide::Put => &PUT_COLUMNS,
            },
        )?,
    })
}

/// Narrows a count into the `UInt64` its column carries.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] naming `field` when the count does not
/// fit, which cannot happen on a 64-bit target and is checked anyway rather
/// than cast.
fn to_storage_count(value: usize, field: &str) -> Result<u64, ChainError> {
    u64::try_from(value).map_err(|_| ChainError::Validation {
        field: field.to_string(),
        reason: format!("{value} does not fit a UInt64 column"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};
    use positive::pos_or_panic;
    use rust_decimal_macros::dec;
    use std::str::FromStr;

    fn instant(day: u32) -> DateTime<Utc> {
        match Utc.with_ymd_and_hms(2026, 1, day, 14, 30, 0).single() {
            Some(instant) => instant,
            None => panic!("the test instant must be valid"),
        }
    }

    /// A quote whose values are deliberately awkward: a 28-digit premium, a
    /// negative delta, a missing put side.
    /// A call's greek snapshot, every value distinct so a transposed column is
    /// a mismatch rather than a coincidence, and every value at the full 28
    /// digits the storage scale promises never to round.
    ///
    /// `delta` and `gamma` repeat the mirrors on purpose: they share those
    /// columns, which is the property the round-trip has to prove.
    fn fixture_greeks_call() -> GreeksSnapshot {
        GreeksSnapshot {
            delta: dec!(0.5123),
            gamma: dec!(0.00312345),
            theta: dec!(-0.0289390751520225679360302935),
            vega: dec!(0.0833669467282696047688677110),
            rho: Some(dec!(0.0169894176861345909734753825)),
            rho_d: Some(dec!(-0.0175414508192199992738499204)),
            alpha: Some(dec!(-1.7676156552525424476306277983)),
            vanna: dec!(1.2473442995808183501801769442),
            vomma: dec!(0.2838300607436393803867085535),
            veta: dec!(0.0000348161009099551782779877),
            charm: dec!(-0.0044637844401925672319270818),
            color: dec!(-0.0002301924225239124326433752),
        }
    }

    /// The put of the same strike, with a different `charm` and a sign-flipped
    /// `rho`, and `alpha` absent — so the round-trip also proves that an
    /// optional greek upstream did not compute survives as absent, not as zero.
    fn fixture_greeks_put() -> GreeksSnapshot {
        GreeksSnapshot {
            delta: dec!(-0.4877),
            gamma: dec!(0.00312345),
            theta: dec!(-0.0215745200014529089799921330),
            vega: dec!(0.0833669467282696047688677110),
            rho: Some(dec!(-0.0690286875414646276502282280)),
            rho_d: Some(dec!(0.0645490601096514193310401325)),
            alpha: None,
            vanna: dec!(1.2473442995808183501801769442),
            vomma: dec!(0.2838300607436393803867085535),
            veta: dec!(0.0000348161009099551782779877),
            charm: dec!(-0.0045048296956570029453340524),
            color: dec!(-0.0002301924225239124326433752),
        }
    }

    fn quote(strike: f64) -> QuoteRow {
        let long = match Decimal::from_str("1.2345678901234567890123456789") {
            Ok(value) => value,
            Err(error) => panic!("the fixture decimal must parse: {error}"),
        };
        let long_positive = match Positive::new_decimal(long) {
            Ok(value) => value,
            Err(error) => panic!("the fixture premium must be positive: {error}"),
        };

        QuoteRow::new(pos_or_panic!(strike), pos_or_panic!(0.185))
            .with_call(
                Some(long_positive),
                Some(pos_or_panic!(1.3)),
                Some(pos_or_panic!(1.25)),
                Some(dec!(0.5123)),
            )
            .with_put(None, Some(pos_or_panic!(1.1)), None, Some(dec!(-0.4877)))
            .with_gamma(Some(dec!(0.00312345)))
            .with_greeks_call(Some(fixture_greeks_call()))
            .with_greeks_put(Some(fixture_greeks_put()))
    }

    fn expiration(day: u32, strikes: &[f64]) -> ExpirationRecord {
        ExpirationRecord::new(
            instant(day),
            pos_or_panic!(f64::from(day)),
            vec!["weeklies".to_string(), "zero_dte".to_string()],
            strikes.iter().copied().map(quote).collect(),
        )
    }

    fn record() -> SnapshotRecord {
        SnapshotRecord::new(
            Uuid::from_u128(42),
            3,
            7,
            instant(5),
            "SPX".to_string(),
            pos_or_panic!(5000.25),
            pos_or_panic!(0.18),
            vec![
                expiration(6, &[4975.0, 5000.0, 5025.0]),
                expiration(9, &[4975.0, 5000.0]),
            ],
        )
    }

    /// The rows a read would produce for a written snapshot.
    ///
    /// The whole point of the round-trip tests: the storage rows are turned
    /// back into the read rows the SELECTs project, without a server.
    fn read_rows(record: &SnapshotRecord) -> (SnapshotMetaReadRow, Vec<QuoteReadRow>) {
        let meta = match meta_row(record, 1_700_000_000_000) {
            Ok(row) => row,
            Err(error) => panic!("the fixture must convert: {error}"),
        };
        let quotes = match quote_rows(record, 1_700_000_000_000) {
            Ok(rows) => rows,
            Err(error) => panic!("the fixture must convert: {error}"),
        };

        let meta = SnapshotMetaReadRow {
            step: meta.step,
            snapshot_id: meta.snapshot_id,
            simulated_at: meta.simulated_at,
            symbol: meta.symbol,
            underlying_price: meta.underlying_price,
            base_volatility: meta.base_volatility,
            quote_count: meta.quote_count,
        };
        let quotes = quotes
            .into_iter()
            .map(|row| QuoteReadRow {
                step: row.step,
                expires_at: row.expires_at,
                days_to_expiration: row.days_to_expiration,
                labels: row.labels,
                strike: row.strike,
                implied_volatility: row.implied_volatility,
                call_bid: row.call_bid,
                call_ask: row.call_ask,
                call_mid: row.call_mid,
                put_bid: row.put_bid,
                put_ask: row.put_ask,
                put_mid: row.put_mid,
                delta_call: row.delta_call,
                delta_put: row.delta_put,
                gamma: row.gamma,
                gamma_call: row.gamma_call,
                gamma_put: row.gamma_put,
                theta_call: row.theta_call,
                theta_put: row.theta_put,
                vega_call: row.vega_call,
                vega_put: row.vega_put,
                rho_call: row.rho_call,
                rho_put: row.rho_put,
                rho_d_call: row.rho_d_call,
                rho_d_put: row.rho_d_put,
                alpha_call: row.alpha_call,
                alpha_put: row.alpha_put,
                vanna_call: row.vanna_call,
                vanna_put: row.vanna_put,
                vomma_call: row.vomma_call,
                vomma_put: row.vomma_put,
                veta_call: row.veta_call,
                veta_put: row.veta_put,
                charm_call: row.charm_call,
                charm_put: row.charm_put,
                color_call: row.color_call,
                color_put: row.color_put,
            })
            .collect();

        (meta, quotes)
    }

    /// A decimal survives the column exactly, including all 28 digits.
    #[test]
    fn test_a_decimal_round_trips_exactly() {
        let values = [
            dec!(0),
            dec!(5000.25),
            dec!(-0.4877),
            dec!(0.00000000000000000000000001),
            match Decimal::from_str("1.2345678901234567890123456789") {
                Ok(value) => value,
                Err(error) => panic!("the fixture decimal must parse: {error}"),
            },
        ];

        for value in values {
            let raw = match to_storage_decimal(value, "test") {
                Ok(raw) => raw,
                Err(error) => panic!("{value} must be storable: {error}"),
            };
            match from_storage_decimal(raw, "test") {
                Ok(read) => assert_eq!(read, value, "{value} did not survive the column"),
                Err(error) => panic!("{value} must be readable: {error}"),
            }
        }
    }

    /// The scale is the one that never truncates a `rust_decimal`.
    #[test]
    fn test_the_scale_matches_the_decimal_maximum() {
        assert_eq!(DECIMAL_SCALE, 28);
    }

    /// A value past the column's ten-digit integer ceiling is a typed error,
    /// never a wrapped number.
    #[test]
    fn test_an_oversized_decimal_is_rejected() {
        let huge = match Decimal::from_str("100000000000") {
            Ok(value) => value,
            Err(error) => panic!("the fixture decimal must parse: {error}"),
        };

        match to_storage_decimal(huge, "underlying_price") {
            Err(ChainError::ClickHouseError(message)) => {
                assert!(message.contains("underlying_price"), "{message}");
            }
            other => panic!("expected a ClickHouse error, got {other:?}"),
        }
    }

    /// An instant survives the column to the nanosecond, which is what a
    /// client-supplied `start_at` can carry.
    #[test]
    fn test_an_instant_round_trips_to_the_nanosecond() {
        let precise = match instant(5).with_nanosecond(123_456_789) {
            Some(value) => value,
            None => panic!("the fixture nanosecond must be valid"),
        };

        let raw = match to_storage_instant(precise, "simulated_at") {
            Ok(raw) => raw,
            Err(error) => panic!("the instant must be storable: {error}"),
        };
        assert_eq!(from_storage_instant(raw), precise);
    }

    /// A snapshot flattens into exactly its quote count, in sorted order.
    #[test]
    fn test_a_snapshot_flattens_into_its_quote_count() {
        let record = record();
        let rows = match quote_rows(&record, 1) {
            Ok(rows) => rows,
            Err(error) => panic!("the record must convert: {error}"),
        };

        assert_eq!(rows.len(), record.quote_count());
        assert_eq!(rows.len(), 5);
        for pair in rows.windows(2) {
            if let [left, right] = pair {
                assert!(
                    (left.expires_at, left.strike) < (right.expires_at, right.strike),
                    "rows must be written in the table's sorting order"
                );
            }
        }
    }

    /// Every row carries the snapshot's identity, so a quote can be traced back
    /// without joining.
    #[test]
    fn test_every_quote_row_carries_the_snapshot_identity() {
        let record = record();
        let expected = record.snapshot_id().to_string();
        let rows = match quote_rows(&record, 9) {
            Ok(rows) => rows,
            Err(error) => panic!("the record must convert: {error}"),
        };

        for row in &rows {
            assert_eq!(row.snapshot_id, expected);
            assert_eq!(row.simulation_id, record.simulation.to_string());
            assert_eq!(row.simulation_generation, record.generation);
            assert_eq!(row.inserted_at_ms, 9);
        }
    }

    /// The marker carries the count and the completion flag a reader checks.
    #[test]
    fn test_the_marker_carries_the_expected_counts() {
        let record = record();
        let meta = match meta_row(&record, 5) {
            Ok(row) => row,
            Err(error) => panic!("the record must convert: {error}"),
        };

        assert!(meta.complete);
        assert_eq!(meta.quote_count, 5);
        assert_eq!(meta.expiration_count, 2);
        assert_eq!(meta.snapshot_id, record.snapshot_id().to_string());
        assert_eq!(meta.inserted_at_ms, 5);
    }

    /// The acceptance criterion, without a server: a snapshot written and read
    /// back is the snapshot that was written — every timestamp, label, strike,
    /// premium, Greek and `None`.
    #[test]
    fn test_a_snapshot_round_trips_through_the_row_types() {
        let original = record();
        let (meta, quotes) = read_rows(&original);

        match record_from_rows(original.simulation, original.generation, &meta, &quotes) {
            Ok(reconstructed) => assert_eq!(reconstructed, original),
            Err(error) => panic!("the snapshot must reconstruct: {error}"),
        }
    }

    /// An empty snapshot reconstructs as an empty snapshot rather than failing.
    #[test]
    fn test_an_empty_snapshot_round_trips() {
        let mut original = record();
        original.expirations.clear();
        let (meta, quotes) = read_rows(&original);

        assert!(quotes.is_empty());
        match record_from_rows(original.simulation, original.generation, &meta, &quotes) {
            Ok(reconstructed) => assert_eq!(reconstructed, original),
            Err(error) => panic!("the snapshot must reconstruct: {error}"),
        }
    }

    /// Rows stored under a different identity scheme are refused rather than
    /// served as this simulation's.
    #[test]
    fn test_a_foreign_snapshot_identity_is_refused() {
        let original = record();
        let (mut meta, quotes) = read_rows(&original);
        meta.snapshot_id = Uuid::from_u128(1).to_string();

        match record_from_rows(original.simulation, original.generation, &meta, &quotes) {
            Err(ChainError::ClickHouseError(message)) => {
                assert!(message.contains("stored under identity"), "{message}");
            }
            other => panic!("expected a ClickHouse error, got {other:?}"),
        }
    }

    /// A row written before issue #74 still reads.
    ///
    /// Its greek columns do not exist yet, so ClickHouse returns NULL for every
    /// one of them. That must rebuild as "no snapshot" — the honest answer —
    /// rather than failing the whole read or inventing a snapshot of zeros that
    /// a client would trade on. Every field the row DOES carry survives.
    #[test]
    fn test_a_row_written_before_the_greek_columns_reads_as_no_snapshot() {
        let strike = match to_storage_decimal(dec!(5000), "strike") {
            Ok(raw) => raw,
            Err(error) => panic!("the fixture decimal must convert: {error}"),
        };
        let volatility = match to_storage_decimal(dec!(0.185), "implied_volatility") {
            Ok(raw) => raw,
            Err(error) => panic!("the fixture decimal must convert: {error}"),
        };
        let delta_call = match to_storage_decimal(dec!(0.5123), "delta_call") {
            Ok(raw) => Some(raw),
            Err(error) => panic!("the fixture decimal must convert: {error}"),
        };
        let gamma = match to_storage_decimal(dec!(0.00312345), "gamma") {
            Ok(raw) => Some(raw),
            Err(error) => panic!("the fixture decimal must convert: {error}"),
        };

        let row = QuoteReadRow {
            step: 3,
            expires_at: 0,
            days_to_expiration: strike,
            labels: vec!["zero_dte".to_string()],
            strike,
            implied_volatility: volatility,
            call_bid: None,
            call_ask: None,
            call_mid: None,
            put_bid: None,
            put_ask: None,
            put_mid: None,
            delta_call,
            delta_put: None,
            gamma,
            gamma_call: None,
            gamma_put: None,
            theta_call: None,
            theta_put: None,
            vega_call: None,
            vega_put: None,
            rho_call: None,
            rho_put: None,
            rho_d_call: None,
            rho_d_put: None,
            alpha_call: None,
            alpha_put: None,
            vanna_call: None,
            vanna_put: None,
            vomma_call: None,
            vomma_put: None,
            veta_call: None,
            veta_put: None,
            charm_call: None,
            charm_put: None,
            color_call: None,
            color_put: None,
        };

        match quote_from_row(&row) {
            Ok(quote) => {
                assert_eq!(quote.greeks_call, None, "an old row has no snapshot");
                assert_eq!(quote.greeks_put, None, "an old row has no snapshot");
                // The mirrors it did carry are untouched, which is why they are
                // kept as their own columns rather than folded into the set.
                assert_eq!(quote.delta_call, Some(dec!(0.5123)));
                assert_eq!(quote.gamma, Some(dec!(0.00312345)));
                assert_eq!(quote.strike, pos_or_panic!(5000.0));
            }
            Err(error) => panic!("an old row must still read: {error}"),
        }
    }

    /// A snapshot missing one required value rebuilds as absent, not as a
    /// partial snapshot.
    ///
    /// Nine of the twelve values are non-optional upstream. If one of those
    /// columns is NULL — a half-written row, or a column added later than its
    /// siblings — defaulting it to zero would hand a client a number that was
    /// never computed. The whole side is reported absent instead.
    #[test]
    fn test_a_partially_stored_snapshot_rebuilds_as_absent() {
        let (_, rows) = read_rows(&record());
        let mut read = match rows.first() {
            Some(row) => row.clone(),
            None => panic!("the fixture must produce rows"),
        };
        assert!(
            quote_from_row(&read)
                .map(|quote| quote.greeks_call.is_some())
                .unwrap_or(false),
            "the fixture must start with a call snapshot"
        );

        // `veta` is required, so losing it loses the side.
        read.veta_call = None;

        match quote_from_row(&read) {
            Ok(quote) => {
                assert_eq!(
                    quote.greeks_call, None,
                    "a partial snapshot is not a snapshot"
                );
                assert!(
                    quote.greeks_put.is_some(),
                    "the other side is unaffected: {quote:?}"
                );
            }
            Err(error) => panic!("a partial row must still read: {error}"),
        }
    }

    /// The two sides of one strike carry genuinely different greeks.
    ///
    /// The columns are per style, so a write that put one side's values in both
    /// column sets — or a read that projected one for both — would still round
    /// trip. This is what catches that.
    #[test]
    fn test_the_two_sides_store_different_greeks() {
        let (_, rows) = read_rows(&record());
        let row = match rows.first() {
            Some(row) => row,
            None => panic!("the fixture must produce rows"),
        };
        assert_ne!(row.charm_call, row.charm_put, "charm is per style");
        assert_ne!(row.theta_call, row.theta_put, "theta is per style");
        assert_ne!(row.rho_call, row.rho_put, "rho is per style");
        assert_eq!(row.vega_call, row.vega_put, "vega is not");
        assert_eq!(row.gamma_call, row.gamma_put, "gamma is not");
        // `alpha` is absent on the put side of the fixture, so this also pins
        // that an optional greek stores as NULL rather than as a zero.
        assert!(row.alpha_call.is_some());
        assert_eq!(row.alpha_put, None);
    }

    /// A contract row rebuilds the side the query selected.
    #[test]
    fn test_a_contract_row_rebuilds_its_side() {
        let row = ContractReadRow {
            step: 7,
            simulated_at: match to_storage_instant(instant(5), "simulated_at") {
                Ok(raw) => raw,
                Err(error) => panic!("the fixture instant must convert: {error}"),
            },
            expires_at: match to_storage_instant(instant(9), "expires_at") {
                Ok(raw) => raw,
                Err(error) => panic!("the fixture instant must convert: {error}"),
            },
            days_to_expiration: match to_storage_decimal(dec!(4.0), "days_to_expiration") {
                Ok(raw) => raw,
                Err(error) => panic!("the fixture decimal must convert: {error}"),
            },
            strike: match to_storage_decimal(dec!(5000), "strike") {
                Ok(raw) => raw,
                Err(error) => panic!("the fixture decimal must convert: {error}"),
            },
            implied_volatility: match to_storage_decimal(dec!(0.18), "implied_volatility") {
                Ok(raw) => raw,
                Err(error) => panic!("the fixture decimal must convert: {error}"),
            },
            bid: None,
            ask: None,
            mid: None,
            delta: match to_storage_decimal(dec!(-0.4877), "delta") {
                Ok(raw) => Some(raw),
                Err(error) => panic!("the fixture decimal must convert: {error}"),
            },
            gamma: None,
            snapshot_gamma: None,
            theta: None,
            vega: None,
            rho: None,
            rho_d: None,
            alpha: None,
            vanna: None,
            vomma: None,
            veta: None,
            charm: None,
            color: None,
        };

        match contract_quote_from_row(&row, ContractSide::Put) {
            Ok(quote) => {
                assert_eq!(quote.side, ContractSide::Put);
                assert_eq!(quote.step, 7);
                assert_eq!(quote.strike, pos_or_panic!(5000.0));
                assert_eq!(quote.delta, Some(dec!(-0.4877)));
                assert_eq!(quote.bid, None);
                // A row written before issue #74 carries no greek columns, so
                // it rebuilds with no snapshot rather than with a zeroed one.
                assert_eq!(quote.greeks, None);
            }
            Err(error) => panic!("the contract row must rebuild: {error}"),
        }
    }
}
