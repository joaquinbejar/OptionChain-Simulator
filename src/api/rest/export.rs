//! Bulk export of a v2 simulation's complete tape.
//!
//! `GET /api/v2/simulations/{id}/export` replays a simulation from step zero —
//! or over a requested range — and streams it as JSON or CSV. It is what turns
//! a walked-one-request-at-a-time simulation into something a backtester can
//! load in one go.
//!
//! # Read-only, in the strong sense
//!
//! Export takes an **immutable snapshot of the effective parameters** when it
//! starts and replays from those. It never advances the cursor, never changes
//! the state or the revision, and never alters what the next peek returns. A
//! client can export a simulation it has not walked at all and get the whole
//! tape; two clients can export the same simulation concurrently; and an export
//! already streaming is unaffected by the session expiring underneath it,
//! because it stopped needing the store the moment it read the parameters.
//!
//! # Why the work happens off the runtime
//!
//! An `option_chains` export is `steps × expirations × strikes` priced
//! contracts — for a long horizon, minutes of Black-Scholes. Doing that on an
//! Actix worker would block every other request on that thread, so the rows are
//! produced in [`tokio::task::spawn_blocking`] and handed over a bounded
//! channel. The bound is the backpressure: when the client stops reading, the
//! channel fills, the producer blocks, and dropping the response closes the
//! receiver so the producer's next send fails and the task ends. Cancellation
//! costs one row of wasted work.
//!
//! # Where the chains come from
//!
//! When snapshot persistence is on, an `option_chains` export prefers the
//! **persisted** snapshot of a step and replays only the steps the warehouse
//! does not have. The decision is per step, never per export: a warehouse that
//! is missing a step, or missing entirely, or down, costs the export nothing
//! but the pricing it would have done anyway. That is the whole point of a
//! deterministic replay — a gap in the tape is not an incident, so the fallback
//! logs at `DEBUG`.
//!
//! Both sources go through one adapter ([`StepChains`]) that yields the same
//! per-quote view, so a row is byte-identical whichever side produced it — the
//! conversion there renders a value by its number rather than by the form it
//! was stored in, which is what makes that true (issue #152). The
//! factor row still comes from the tape either way: the underlying and
//! volatility datasets are built from it, and it is cheap next to a chain.
//!
//! # Determinism
//!
//! Two exports of the same simulation are byte-identical, including one taken
//! before a tape's rows were filed and one taken after. Every value is a
//! function of the effective parameters and the cursor, timestamps render as
//! whole-second RFC 3339, and numbers use Rust's shortest round-trip
//! formatting — no locale, no thousands separators. That is what lets a
//! backtest harness cache a download and know it is still current.

use crate::api::rest::binary::{
    BinarySchema, CellType, PackedWriter, RowContext, RowFlow, ensure_label_capacity,
    visit_typed_rows,
};
use crate::api::rest::error::map_error;
use crate::api::rest::greeks::GreekLevel;
use crate::api::rest::limits::EXPORT_BLOCK_ROWS;
use crate::domain::factors::FactorTape;
use crate::domain::series::{SeriesBuilder, SeriesSnapshot};
use crate::infrastructure::{
    CURRENT_SNAPSHOT_GENERATION, QuoteRow, SimulationSnapshotRepository, SnapshotRecord,
};
use crate::session::{SimulationManager, SimulationParametersV2};
use crate::utils::ChainError;
use actix_web::{HttpRequest, HttpResponse, Responder, web};
use chrono::{DateTime, SecondsFormat, Utc};
use futures::stream::Stream;
use optionstratlib::chains::OptionData;
use optionstratlib::chains::chain::OptionChain;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tracing::{debug, info, instrument, warn};
use utoipa::ToSchema;
use uuid::Uuid;

/// How many rows are buffered between the producer and the client.
///
/// Small on purpose: it is the backpressure. A slow client fills it, the
/// producer blocks on the next send, and no unbounded queue of priced chains
/// accumulates in memory.
const CHANNEL_CAPACITY: usize = 16;

/// How many steps one warehouse round trip asks for.
///
/// Neither extreme is acceptable. One read per step turns a hundred-thousand
/// step export into a hundred thousand round trips; one read of the whole range
/// materialises the entire tape in memory, which is exactly what the streaming
/// producer exists to avoid — a snapshot can hold two hundred thousand
/// contracts.
///
/// Sixty-four amortises the round trip over a window whose memory cost is
/// bounded by sixty-four snapshots, and it is the *starting* width: a window
/// that a deployment's `OCS_SNAPSHOT_MAX_READ_ROWS` refuses is halved and
/// retried, so a service with very large chains narrows to a width that works
/// instead of silently replaying everything.
const SNAPSHOT_WINDOW_STEPS: usize = 64;

/// Which dataset an export request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Dataset {
    /// One row per step: the simulated instant and the underlying price.
    Underlying,
    /// One row per step: the simulated instant and the base volatility.
    Volatility,
    /// One row per (step × expiration × strike).
    OptionChains,
}

/// Which encoding an export request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Format {
    /// A single valid JSON array of row objects, streamed.
    Json,
    /// RFC 4180 CSV with a header row and CRLF line endings.
    Csv,
    /// Arrow IPC stream, one record batch per block. Available only when the
    /// crate is built with the `arrow-export` feature; without it a request for
    /// this format is a typed 400 rather than a 500 or a silent fallback.
    Arrow,
    /// The `packed` columnar block format: no dependencies, 8-byte aligned
    /// payloads, described in [`crate::api::rest::binary`].
    Packed,
}

impl Format {
    /// The `Content-Type` the response advertises.
    #[must_use]
    fn content_type(self) -> &'static str {
        match self {
            Format::Json => "application/json",
            Format::Csv => "text/csv; charset=utf-8",
            Format::Arrow => "application/vnd.apache.arrow.stream",
            Format::Packed => "application/octet-stream",
        }
    }

    /// Whether this encoding is binary, and therefore columnar and blocked.
    #[must_use]
    fn is_binary(self) -> bool {
        matches!(self, Format::Arrow | Format::Packed)
    }

    /// The extension of the suggested download filename.
    #[must_use]
    fn extension(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Csv => "csv",
            Format::Arrow => "arrow",
            Format::Packed => "ocsp",
        }
    }
}

/// The `option_chains` columns every level carries.
///
/// Frozen: the greek levels only ever APPEND, so a consumer parsing by position
/// keeps working and `greeks=none` stays byte-identical to the export before
/// the parameter existed.
const CHAIN_COLUMNS: &[&str] = &[
    "step",
    "simulated_at",
    "symbol",
    "expires_at",
    "labels",
    "days_to_expiration",
    "strike",
    "implied_volatility",
    "call_bid",
    "call_ask",
    "call_mid",
    "call_delta",
    "put_bid",
    "put_ask",
    "put_mid",
    "put_delta",
    "gamma",
];

/// What `greeks=first` appends: the first-order greeks the default lacks.
///
/// `delta` is not here — it is already `call_delta` / `put_delta`, and the two
/// agree, so a second column could only drift from the first.
const FIRST_ORDER_COLUMNS: &[&str] = &[
    "call_theta",
    "put_theta",
    "call_vega",
    "put_vega",
    "call_rho",
    "put_rho",
    "call_rho_d",
    "put_rho_d",
];

/// What `greeks=all` appends ON TOP of [`FIRST_ORDER_COLUMNS`].
///
/// Split that way so each level's header is a PREFIX of the next: `none` of
/// `first`, `first` of `all`. A consumer that parses by position reads the
/// columns it knows at any level, and a level change never moves one.
///
/// `gamma` IS here, per style, even though the shared `gamma` column already
/// carries it — the one place this set knowingly repeats a number. The shared
/// column is upstream's convenience mirror, which stays defined at expiry and
/// at zero volatility where the snapshot is not, and the per-style pair
/// future-proofs a non-European style whose gamma stops being shared. Same
/// reasoning, and the same three columns, as the warehouse schema. `delta` is
/// not repeated because neither of those applies to it.
const SECOND_ORDER_COLUMNS: &[&str] = &[
    "call_gamma",
    "put_gamma",
    "call_alpha",
    "put_alpha",
    "call_vanna",
    "put_vanna",
    "call_vomma",
    "put_vomma",
    "call_veta",
    "put_veta",
    "call_charm",
    "put_charm",
    "call_color",
    "put_color",
];

impl Dataset {
    /// The dataset's name, used in the suggested filename.
    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            Dataset::Underlying => "underlying",
            Dataset::Volatility => "volatility",
            Dataset::OptionChains => "option_chains",
        }
    }

    /// The CSV header, in the order the rows are written.
    ///
    /// Fixed per level, never variable: a level always emits the same columns
    /// whether or not a particular strike has greeks, so a consumer can parse
    /// by position as well as by name. `greeks` is ignored by the two datasets
    /// that carry no chains.
    #[must_use]
    pub(super) fn header(self, level: GreekLevel) -> Vec<&'static str> {
        match self {
            Dataset::Underlying => vec!["step", "simulated_at", "symbol", "price"],
            Dataset::Volatility => vec!["step", "simulated_at", "symbol", "base_volatility"],
            Dataset::OptionChains => {
                let mut header = CHAIN_COLUMNS.to_vec();
                match level {
                    GreekLevel::None => {}
                    GreekLevel::First => header.extend_from_slice(FIRST_ORDER_COLUMNS),
                    GreekLevel::All => {
                        header.extend_from_slice(FIRST_ORDER_COLUMNS);
                        header.extend_from_slice(SECOND_ORDER_COLUMNS);
                    }
                }
                header
            }
        }
    }

    /// Whether a step of this dataset needs its chains priced.
    ///
    /// `underlying` and `volatility` read straight off the factor tape, so a
    /// multi-year export of either is nearly free — no chain is ever built.
    #[must_use]
    fn needs_chains(self) -> bool {
        matches!(self, Dataset::OptionChains)
    }
}

/// Query parameters for an export.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub(crate) struct ExportQuery {
    /// Which dataset to export.
    pub(super) dataset: Dataset,
    /// Which encoding to return.
    pub(super) format: Format,
    /// First step to include, inclusive. Defaults to `0`.
    #[serde(default)]
    pub(super) from_step: Option<usize>,
    /// Last step to include, inclusive. Defaults to the final generated step.
    #[serde(default)]
    pub(super) to_step: Option<usize>,
    /// How much of the greek set the `option_chains` dataset carries: `none`
    /// (the default), `first` or `all` — the same vocabulary and the same
    /// default as the chain endpoints, so a tape and a live step agree on what
    /// a level means. Kept as a raw string so an unknown value is a typed `400`
    /// naming the field.
    #[serde(default)]
    pub(super) greeks: Option<String>,
}

/// A validated, inclusive step range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StepRange {
    from: usize,
    to: usize,
}

impl StepRange {
    /// Validates a requested range against a simulation's horizon.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming `from_step` or `to_step` when
    /// a bound is past the tape, when the range is reversed, or when it would
    /// produce more rows than the configured cap allows.
    fn resolve(query: &ExportQuery, steps: usize, max_rows: usize) -> Result<Self, ChainError> {
        let last = steps.checked_sub(1).ok_or_else(|| {
            ChainError::Internal(
                "a simulation with no steps cannot exist; `steps >= 1` is validated at creation"
                    .to_string(),
            )
        })?;

        let from = query.from_step.unwrap_or(0);
        let to = query.to_step.unwrap_or(last);

        if from > last {
            return Err(ChainError::Validation {
                field: "from_step".to_string(),
                reason: format!("must not exceed the last step ({last}), got {from}"),
            });
        }
        if to > last {
            return Err(ChainError::Validation {
                field: "to_step".to_string(),
                reason: format!("must not exceed the last step ({last}), got {to}"),
            });
        }
        if from > to {
            return Err(ChainError::Validation {
                field: "from_step".to_string(),
                reason: format!("must not exceed to_step ({to}), got {from}"),
            });
        }

        // The bounds are inclusive, so the count is `to - from + 1`.
        let span = to
            .checked_sub(from)
            .and_then(|span| span.checked_add(1))
            .ok_or_else(|| ChainError::Validation {
                field: "to_step".to_string(),
                reason: "the requested range overflows".to_string(),
            })?;
        if span > max_rows {
            return Err(ChainError::Validation {
                field: "to_step".to_string(),
                reason: format!(
                    "the requested range covers {span} steps, above the {max_rows} the service will export in one request"
                ),
            });
        }

        Ok(Self { from, to })
    }

    /// The steps the range covers.
    fn steps(self) -> impl Iterator<Item = usize> {
        self.from..=self.to
    }
}

/// Renders an instant the way every v2 timestamp is rendered.
#[must_use]
#[inline]
fn render_instant(instant: DateTime<Utc>) -> String {
    instant.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Renders an optional number, or the empty string.
///
/// CSV writes an absent optional as an **empty field** — not `null`, not `0` —
/// so a consumer can tell "not quoted" from "quoted at zero".
#[must_use]
#[inline]
fn render_optional(value: Option<f64>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

/// One option style's greek snapshot, flattened to the export's `f64`.
///
/// `delta` is absent on purpose: it already has its own `call_delta` /
/// `put_delta` column, and the two agree — pinned upstream of here by
/// `test_the_delta_mirror_equals_the_snapshot_delta_at_every_strike`. Emitting
/// it twice would let one column drift from the other.
///
/// The export renders `f64`, not the decimal strings the REST responses carry.
/// A CSV column has no type to distinguish them, and the export's whole
/// contract is that a byte comparison of two runs is meaningful — so both
/// formats render the same `f64` here, and the JSON export matches the CSV
/// value for value.
///
/// Value for value, not character for character: JSON writes `4950.0` where the
/// CSV writes `4950`, and takes exponent form below `1e-5` where the CSV spells
/// the zeros out. Compare the two encodings as numbers.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(super) struct GreeksView {
    /// This side's `gamma`.
    pub(super) gamma: Option<f64>,
    /// This side's `theta`.
    pub(super) theta: Option<f64>,
    /// This side's `vega`.
    pub(super) vega: Option<f64>,
    /// This side's `rho`.
    pub(super) rho: Option<f64>,
    /// This side's `rho_d`.
    pub(super) rho_d: Option<f64>,
    /// This side's `alpha`.
    pub(super) alpha: Option<f64>,
    /// This side's `vanna`.
    pub(super) vanna: Option<f64>,
    /// This side's `vomma`.
    pub(super) vomma: Option<f64>,
    /// This side's `veta`.
    pub(super) veta: Option<f64>,
    /// This side's `charm`.
    pub(super) charm: Option<f64>,
    /// This side's `color`.
    pub(super) color: Option<f64>,
}

impl GreeksView {
    /// Views an upstream snapshot, or nothing when the strike had none.
    #[must_use]
    fn of(snapshot: Option<&optionstratlib::greeks::GreeksSnapshot>) -> Self {
        let Some(snapshot) = snapshot else {
            return Self::default();
        };
        // Destructured, not field-accessed: a thirteenth upstream greek is then
        // a COMPILE ERROR here rather than a column this export silently stops
        // carrying while the response and the warehouse both gain it. Same
        // discipline `ApiWalkType` uses for a new `WalkType` variant.
        //
        // `delta` is bound and dropped on purpose — it has its own column.
        let optionstratlib::greeks::GreeksSnapshot {
            delta: _,
            gamma,
            theta,
            vega,
            rho,
            rho_d,
            alpha,
            vanna,
            vomma,
            veta,
            charm,
            color,
        } = snapshot;
        Self {
            gamma: decimal_to_f64(*gamma),
            theta: decimal_to_f64(*theta),
            vega: decimal_to_f64(*vega),
            rho: rho.and_then(decimal_to_f64),
            rho_d: rho_d.and_then(decimal_to_f64),
            alpha: alpha.and_then(decimal_to_f64),
            vanna: decimal_to_f64(*vanna),
            vomma: decimal_to_f64(*vomma),
            veta: decimal_to_f64(*veta),
            charm: decimal_to_f64(*charm),
            color: decimal_to_f64(*color),
        }
    }
}

/// One strike of one expiration, flattened to exactly what a row carries.
///
/// The common view both sources are reduced to. Every conversion from a source
/// value to the wire's `f64` happens here and only here, which is what makes a
/// persisted row and a replayed row byte-identical rather than merely similar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct QuoteView {
    pub(super) strike: f64,
    pub(super) implied_volatility: f64,
    pub(super) call_bid: Option<f64>,
    pub(super) call_ask: Option<f64>,
    pub(super) call_mid: Option<f64>,
    pub(super) call_delta: Option<f64>,
    pub(super) put_bid: Option<f64>,
    pub(super) put_ask: Option<f64>,
    pub(super) put_mid: Option<f64>,
    pub(super) put_delta: Option<f64>,
    pub(super) gamma: Option<f64>,
    /// The call's greek snapshot, empty below `greeks=first`.
    pub(super) call_greeks: GreeksView,
    /// The put's greek snapshot, empty below `greeks=first`.
    pub(super) put_greeks: GreeksView,
}

impl QuoteView {
    /// Views a strike that was just priced.
    #[must_use]
    fn replayed(data: &OptionData) -> Self {
        Self {
            strike: positive_to_f64(data.strike_price),
            implied_volatility: positive_to_f64(data.implied_volatility),
            call_bid: data.call_bid.map(positive_to_f64),
            call_ask: data.call_ask.map(positive_to_f64),
            call_mid: data.call_middle.map(positive_to_f64),
            call_delta: data.delta_call.and_then(decimal_to_f64),
            put_bid: data.put_bid.map(positive_to_f64),
            put_ask: data.put_ask.map(positive_to_f64),
            put_mid: data.put_middle.map(positive_to_f64),
            put_delta: data.delta_put.and_then(decimal_to_f64),
            gamma: data.gamma.and_then(decimal_to_f64),
            call_greeks: GreeksView::of(data.greeks_call.as_ref()),
            put_greeks: GreeksView::of(data.greeks_put.as_ref()),
        }
    }

    /// Views a strike that was read back from the warehouse.
    #[must_use]
    fn stored(row: &QuoteRow) -> Self {
        Self {
            strike: positive_to_f64(row.strike),
            implied_volatility: positive_to_f64(row.implied_volatility),
            call_bid: row.call_bid.map(positive_to_f64),
            call_ask: row.call_ask.map(positive_to_f64),
            call_mid: row.call_mid.map(positive_to_f64),
            call_delta: row.delta_call.and_then(decimal_to_f64),
            put_bid: row.put_bid.map(positive_to_f64),
            put_ask: row.put_ask.map(positive_to_f64),
            put_mid: row.put_mid.map(positive_to_f64),
            put_delta: row.delta_put.and_then(decimal_to_f64),
            gamma: row.gamma.and_then(decimal_to_f64),
            call_greeks: GreeksView::of(row.greeks_call.as_ref()),
            put_greeks: GreeksView::of(row.greeks_put.as_ref()),
        }
    }
}

/// The strikes of one expiration, from whichever source produced them.
#[derive(Debug, Clone, Copy)]
pub(super) enum QuoteSource<'a> {
    /// Upstream's priced chain, iterated by ascending strike.
    Replayed(&'a OptionChain),
    /// The stored rows, which the repository returns by ascending strike.
    Stored(&'a [QuoteRow]),
}

impl<'a> QuoteSource<'a> {
    /// The strikes, ascending.
    ///
    /// Exactly one of the two options is `Some`, so concatenating them with
    /// [`Iterator::chain`] *is* the branch — one concrete iterator type, no
    /// boxing on a path that runs once per contract.
    pub(super) fn quotes(self) -> impl Iterator<Item = QuoteView> + 'a {
        let replayed = match self {
            QuoteSource::Replayed(chain) => Some(chain.iter()),
            QuoteSource::Stored(_) => None,
        };
        let stored = match self {
            QuoteSource::Replayed(_) => None,
            QuoteSource::Stored(quotes) => Some(quotes.iter()),
        };

        replayed
            .into_iter()
            .flatten()
            .map(QuoteView::replayed)
            .chain(stored.into_iter().flatten().map(QuoteView::stored))
    }
}

/// One expiration of one step, from whichever source produced it.
#[derive(Debug, Clone, Copy)]
pub(super) struct ExpirationView<'a> {
    pub(super) expires_at: DateTime<Utc>,
    pub(super) days_to_expiration: f64,
    pub(super) labels: &'a [String],
    pub(super) quotes: QuoteSource<'a>,
}

/// The chains of one step, from whichever source produced them.
///
/// The adapter the whole preference rests on: a stored snapshot and a replayed
/// one are the same simulated market, so they must render the same rows. Giving
/// the two sources one view — instead of two row builders that happen to agree
/// today — is what makes preferring the warehouse safe.
#[derive(Debug, Clone, Copy)]
pub(super) enum StepChains<'a> {
    /// Priced here and now, from the effective parameters.
    Replayed(&'a SeriesSnapshot),
    /// Read back from the warehouse exactly as it was served.
    Stored(&'a SnapshotRecord),
}

impl<'a> StepChains<'a> {
    /// The live expirations, ascending — the order both sources guarantee.
    pub(super) fn expirations(self) -> impl Iterator<Item = ExpirationView<'a>> {
        let replayed = match self {
            StepChains::Replayed(snapshot) => Some(snapshot.chains.iter()),
            StepChains::Stored(_) => None,
        };
        let stored = match self {
            StepChains::Replayed(_) => None,
            StepChains::Stored(record) => Some(record.expirations.iter()),
        };

        replayed
            .into_iter()
            .flatten()
            .map(|chain| ExpirationView {
                expires_at: chain.expires_at,
                days_to_expiration: positive_to_f64(chain.days_to_expiration),
                labels: &chain.labels,
                quotes: QuoteSource::Replayed(&chain.chain),
            })
            .chain(
                stored
                    .into_iter()
                    .flatten()
                    .map(|expiration| ExpirationView {
                        expires_at: expiration.expires_at,
                        days_to_expiration: positive_to_f64(expiration.days_to_expiration),
                        labels: &expiration.labels,
                        quotes: QuoteSource::Stored(&expiration.quotes),
                    }),
            )
    }
}

/// The last step a window starting at `from` covers.
///
/// Clamped to `last` so a window never asks for steps past the range, and
/// checked so a `from` near `usize::MAX` cannot wrap into a reversed range.
#[must_use]
#[inline]
fn window_end(from: usize, window: usize, last: usize) -> usize {
    match window
        .checked_sub(1)
        .and_then(|span| from.checked_add(span))
    {
        Some(end) => end.min(last),
        None => last,
    }
}

/// Reads persisted snapshots ahead of the producer, a window of steps at a time.
///
/// Lives on the blocking thread that produces the rows, so its reads have to
/// cross back onto the runtime — hence the [`Handle`]. Blocking on a future from
/// a `spawn_blocking` thread is sound (it is not an async context); doing it
/// from an Actix worker would not be, which is exactly why the whole producer
/// runs off the runtime.
struct StoredSteps {
    repository: Arc<dyn SimulationSnapshotRepository>,
    runtime: Handle,
    simulation: Uuid,
    /// What the current window found, ascending by step and consumed in order.
    loaded: VecDeque<SnapshotRecord>,
    /// The last step the current window covered; `None` before the first read.
    window_end: Option<usize>,
    /// How many steps a window asks for. Narrows on a refused read.
    window: usize,
    /// Set once a read fails for a reason a narrower window cannot fix.
    ///
    /// After that the export replays everything. A warehouse that is down will
    /// be down for the next window too, and a failed round trip per window
    /// would add latency to an export that is already producing correct rows
    /// without it.
    degraded: bool,
}

impl StoredSteps {
    /// Prepares to read one simulation's persisted tape.
    #[must_use]
    fn new(
        repository: Arc<dyn SimulationSnapshotRepository>,
        simulation: Uuid,
        runtime: Handle,
    ) -> Self {
        Self {
            repository,
            runtime,
            simulation,
            loaded: VecDeque::new(),
            window_end: None,
            window: SNAPSHOT_WINDOW_STEPS,
            degraded: false,
        }
    }

    /// The persisted snapshot of `step`, when the warehouse has a complete one.
    ///
    /// `last` bounds the prefetch, so a window never reads past the export's
    /// range. Steps are requested in ascending order, which is what lets the
    /// window be consumed from the front instead of indexed.
    fn take(&mut self, step: usize, last: usize) -> Option<SnapshotRecord> {
        if self.degraded {
            return None;
        }
        if self.window_end.is_none_or(|end| step > end) {
            self.load(step, last);
        }

        // Anything below `step` belongs to a step already produced: the
        // repository skips what was never completed, so a gap is normal.
        while self.loaded.front().is_some_and(|record| record.step < step) {
            self.loaded.pop_front();
        }
        match self.loaded.front() {
            Some(record) if record.step == step => self.loaded.pop_front(),
            _ => None,
        }
    }

    /// Fills the window that starts at `from`.
    ///
    /// A window refused as too wide is halved and retried: that failure is a
    /// property of this deployment's snapshot size against
    /// `OCS_SNAPSHOT_MAX_READ_ROWS`, so a narrower window keeps working for the
    /// rest of the export. Every other failure degrades to replay. The loop
    /// terminates because the width strictly decreases and stops at one.
    fn load(&mut self, from: usize, last: usize) {
        self.loaded.clear();
        loop {
            let to = window_end(from, self.window, last);
            match self.read(from, to) {
                Ok(records) => {
                    debug!(
                        simulation_id = %self.simulation,
                        from_step = from,
                        to_step = to,
                        found = records.len(),
                        "Prefetched persisted snapshots for an export window"
                    );
                    self.loaded = VecDeque::from(records);
                    self.window_end = Some(to);
                    return;
                }
                Err(ChainError::Validation { field, reason }) if self.window > 1 => {
                    // Integer division by a non-zero constant, bottoming out at
                    // one: no saturation, no zero-width window.
                    self.window /= 2;
                    debug!(
                        simulation_id = %self.simulation,
                        window = self.window,
                        field = %field,
                        reason = %reason,
                        "Narrowed the snapshot read window and retried"
                    );
                }
                Err(error) => {
                    // DEBUG, not WARN: a cold or absent warehouse is the default
                    // configuration, and replay produces the same rows.
                    debug!(
                        simulation_id = %self.simulation,
                        from_step = from,
                        error = %error,
                        "Could not read persisted snapshots; the export replays instead"
                    );
                    self.degraded = true;
                    return;
                }
            }
        }
    }

    /// Runs one range read on the runtime and waits for it.
    fn read(&self, from: usize, to: usize) -> Result<Vec<SnapshotRecord>, ChainError> {
        let repository = Arc::clone(&self.repository);
        let simulation = self.simulation;
        self.runtime.block_on(async move {
            repository
                .read_range(simulation, CURRENT_SNAPSHOT_GENERATION, from, to)
                .await
        })
    }
}

/// Streams rows from a bounded channel as an HTTP body.
///
/// Dropping this — which is what actix does when the client disconnects —
/// closes the receiver, so the producer's next send fails and its task ends.
struct RowStream {
    receiver: mpsc::Receiver<Result<Vec<u8>, ChainError>>,
}

impl Stream for RowStream {
    type Item = Result<web::Bytes, actix_web::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.receiver.poll_recv(cx) {
            Poll::Ready(Some(Ok(chunk))) => Poll::Ready(Some(Ok(web::Bytes::from(chunk)))),
            Poll::Ready(Some(Err(error))) => {
                // The response has already begun, so a failure mid-stream can
                // only truncate the body — the status line is long gone. Log it
                // loudly and end the stream rather than pretending it finished.
                warn!(%error, "a v2 export failed after the response had started");
                Poll::Ready(Some(Err(actix_web::error::ErrorInternalServerError(
                    error.to_string(),
                ))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// What the `format` query parameter accepts, IN THIS BUILD.
///
/// Two constants rather than one sentence, because the document must not
/// advertise a format the binary reading it would refuse: `arrow` is a Cargo
/// feature, and a build without it rejects the format with a typed 400 (issue
/// #148). Selected by `cfg`, so the advertised list is whatever this binary
/// actually serves.
#[cfg(feature = "arrow-export")]
const FORMAT_PARAMETER: &str = "json | csv | arrow | packed. The two binary encodings carry the same values as the text ones, in blocks of OCS_EXPORT_BLOCK_ROWS rows, with the same column names in the same order. Numeric columns are f64, exactly what json and csv render: binary is a faster route to the same numbers, NOT a route to the underlying Decimal(38, 28) precision. `arrow` is an Arrow IPC stream, carried by this build. `packed` is a dependency-free columnar block format with 8-byte aligned payloads, documented in the crate docs, whose `labels` column is a bitmask over the schedule's rule ids.";

/// As [`FORMAT_PARAMETER`], for a build without the `arrow-export` feature.
#[cfg(not(feature = "arrow-export"))]
const FORMAT_PARAMETER: &str = "json | csv | packed. The two binary encodings carry the same values as the text ones, in blocks of OCS_EXPORT_BLOCK_ROWS rows, with the same column names in the same order. Numeric columns are f64, exactly what json and csv render: binary is a faster route to the same numbers, NOT a route to the underlying Decimal(38, 28) precision. `packed` is a dependency-free columnar block format with 8-byte aligned payloads, documented in the crate docs, whose `labels` column is a bitmask over the schedule's rule ids. This build carries no `arrow-export` feature, so it does not offer the Arrow IPC stream and answers a request for it with a 400 naming the format.";

#[utoipa::path(
    get,
    path = "/api/v2/simulations/{id}/export",
    description = "Export a simulation's complete tape, or a step range of it, as JSON, CSV, or one \
        of the two binary encodings (Arrow IPC and the dependency-free `packed` columnar format). \
        Read-only: it replays from an immutable snapshot of the effective parameters and never \
        advances the cursor, changes the state or version, or alters what the next peek returns. \
        A simulation that has not been walked at all exports its whole tape. Where snapshot \
        persistence is enabled, an option_chains export serves the steps the warehouse holds from \
        it and replays the rest; the rows are identical either way, at every greek level — a stored step that predates the greek columns is replayed rather than served short. JSON is \
        a single array of row objects; CSV is RFC 4180 with a header row and CRLF line endings. \
        Repeating the same export at the same greek level yields byte-identical output. The two \
        encodings carry the same values, though not always the same notation: JSON takes exponent \
        form for very small numbers where the CSV spells them out. The binary encodings carry the \
        same values again, in blocks, with the same column names in the same order.",
    params(
        ("id" = String, Path, description = "The simulation's identifier"),
        ("dataset" = String, Query, description = "underlying | volatility | option_chains"),
        ("format" = String, Query, description = FORMAT_PARAMETER),
        ("from_step" = Option<usize>, Query, description = "First step, inclusive; defaults to 0"),
        ("to_step" = Option<usize>, Query, description = "Last step, inclusive; defaults to the final step"),
        ("greeks" = Option<String>, Query, description = "How much of the greek set the option_chains dataset carries: `none` (default), `first` (appends call_theta, put_theta, call_vega, put_vega, call_rho, put_rho, call_rho_d, put_rho_d) or `all` (appends seven more per style, gamma through color: fourteen columns). Each level's header is a PREFIX of the next, so raising it appends columns and never moves one. Values are per ONE LONG CONTRACT. Accepted but immaterial for the underlying and volatility datasets, which carry no chains. An unknown value is a 400")
    ),
    responses(
        (status = 200, description = "The exported rows, streamed. Text for json and csv; an Arrow IPC stream or a packed columnar document, both binary, for arrow and packed.", body = String),
        (status = 400, description = "Every rejection carries the typed `{error, field}` of ValidationErrorResponse. `field` names the offender where it is known — `id` for a malformed identifier, `from_step` or `to_step` for a range that is reversed or past the tape, `greeks` for an unknown level, `format` for `arrow` on a build without the `arrow-export` feature. It is EMPTY when the query string fails to deserialise at all (an unknown `dataset` or `format`, a non-numeric step), because serde's message for those does not name the key; the `error` string carries the detail.", body = crate::api::rest::responses::ValidationErrorResponse),
        (status = 404, description = "Simulation not found"),
        (status = 500, description = "Internal server error")
    )
)]
#[instrument(skip(manager, snapshots, query), level = "debug")]
pub(crate) async fn export_simulation(
    req: HttpRequest,
    manager: web::Data<Arc<SimulationManager>>,
    // Absent unless the operator turned snapshot persistence on, which is why
    // this is an `Option` rather than a required dependency: a deployment
    // without ClickHouse must not have to register anything to not use it.
    snapshots: Option<web::Data<Arc<dyn SimulationSnapshotRepository>>>,
    path: web::Path<super::handlers_v2::SimulationPath>,
    query: web::Query<ExportQuery>,
) -> impl Responder {
    info!("{} {}", req.method(), req.path());

    let id = match Uuid::parse_str(&path.id) {
        Ok(id) => id,
        Err(_) => {
            return map_error(ChainError::Validation {
                field: "id".to_string(),
                reason: format!("must be a UUID, got {:?}", path.id),
            });
        }
    };

    // Resolved before the store is read, so `?greeks=bogus` on a missing
    // simulation answers 400 here exactly as it does on the snapshot endpoint,
    // rather than 404 — and long before the stream, where an error would arrive
    // mid-download with a header already sent.
    let level = match GreekLevel::parse(query.greeks.as_deref()) {
        Ok(level) => level,
        Err(error) => return map_error(error),
    };

    // The one read of shared state. From here on the export owns everything it
    // needs, so the simulation may be advanced, deleted or expired without
    // affecting the download in flight.
    let simulation = match manager.get(id).await {
        Ok(simulation) => simulation,
        Err(error) => return map_error(error),
    };
    let parameters = simulation.parameters.clone();

    let range = match StepRange::resolve(&query, parameters.steps, manager.config().max_export_rows)
    {
        Ok(range) => range,
        Err(error) => return map_error(error),
    };

    let dataset = query.dataset;
    let format = query.format;

    // Checked HERE, not in the producer: the producer runs behind a streaming
    // response whose header has already gone out, so a rejection raised there
    // arrives mid-download instead of as the 400 it is. This covers `arrow` on
    // a build without the feature, and a schedule too wide for a label mask.
    if let Err(error) = check_format_available(format, dataset, level, &parameters) {
        return map_error(error);
    }

    let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);

    // Only the chains dataset can be served from storage; the other two read the
    // factor tape and would pay a round trip for nothing. The handle is taken
    // here, on the runtime, because the producer that uses it will not be on one.
    let stored = snapshots
        .filter(|_| dataset.needs_chains())
        .map(|repository| {
            StoredSteps::new(Arc::clone(repository.get_ref()), id, Handle::current())
        });

    // Priced chains are minutes of CPU for a long horizon. Producing them on an
    // Actix worker would block every other request on that thread.
    tokio::task::spawn_blocking(move || {
        if let Err(error) = produce(&parameters, dataset, format, level, range, stored, &sender) {
            // A send failure means the client went away, which is not an error
            // worth reporting to anyone.
            let _ = sender.blocking_send(Err(error));
        }
    });

    let filename = format!(
        "{}-{}-{}.{}",
        simulation.id,
        dataset.as_str(),
        range.from,
        format.extension()
    );

    HttpResponse::Ok()
        .content_type(format.content_type())
        .insert_header((
            actix_web::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        ))
        .streaming(RowStream { receiver })
}

/// Whether this build, and this simulation, can serve a format.
///
/// The two ways a recognised format can still be unserveable: the `arrow`
/// encoding needs the `arrow-export` feature, and both binary encodings need a
/// schedule whose rule ids fit a label mask.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] naming `format`.
fn check_format_available(
    format: Format,
    dataset: Dataset,
    level: GreekLevel,
    parameters: &SimulationParametersV2,
) -> Result<(), ChainError> {
    if !format.is_binary() {
        return Ok(());
    }

    #[cfg(not(feature = "arrow-export"))]
    if matches!(format, Format::Arrow) {
        return Err(arrow_unavailable());
    }

    let schema = BinarySchema::new(dataset, level, parameters);
    // Only a dataset that actually carries labels can exceed the mask.
    if schema.types.contains(&CellType::LabelMask) {
        ensure_label_capacity(&schema)?;
    }
    Ok(())
}

/// The rejection a build without the `arrow-export` feature answers with.
///
/// One place, because it is served from two: the handler's up-front check and
/// the writer's own arm, which is unreachable behind it and kept as defence.
#[cfg(not(feature = "arrow-export"))]
fn arrow_unavailable() -> ChainError {
    ChainError::Validation {
        field: "format".to_string(),
        reason: "arrow is unavailable: this build does not have the `arrow-export` feature; \
                 use packed, json or csv"
            .to_string(),
    }
}

/// Whether a stored record can answer a request for greeks.
///
/// True when every quote it carries has both snapshots — which is what a record
/// filed by this version looks like. A record written before the greek columns
/// existed has none, and one filed by a build whose chain could not price a
/// degenerate strike may have some: either way, only a whole record can be
/// served for a level above the default, because a half-covered one would put
/// two different coverages in one export.
#[must_use]
fn record_carries_greeks(record: &SnapshotRecord) -> bool {
    record
        .expirations
        .iter()
        .flat_map(|expiration| expiration.quotes.iter())
        .all(|quote| quote.greeks_call.is_some() && quote.greeks_put.is_some())
}

/// Produces the range and sends every chunk.
///
/// Runs on a blocking thread. Returns as soon as a send fails, which is how a
/// disconnected client stops the work.
///
/// Each step takes its chains from the warehouse when `stored` has them and
/// replays them otherwise, so a partially persisted tape costs exactly the
/// pricing of its gaps.
fn produce(
    parameters: &SimulationParametersV2,
    dataset: Dataset,
    format: Format,
    level: GreekLevel,
    range: StepRange,
    mut stored: Option<StoredSteps>,
    sender: &mpsc::Sender<Result<Vec<u8>, ChainError>>,
) -> Result<(), ChainError> {
    let tape = FactorTape::build(parameters, &parameters.method)?;
    let builder = if dataset.needs_chains() {
        // Priced WITH the greek snapshots exactly when the level asks for them.
        // This is what makes a replayed step and a stored one carry the same
        // columns: a warehouse fills its rows from a chain built the same way,
        // so the two conversion paths cannot disagree about what exists.
        Some(SeriesBuilder::new(parameters, &tape)?.with_greek_snapshots(level.wants_greeks()))
    } else {
        None
    };

    // The block width is read once, here, rather than inside the writer: it is
    // the export's memory floor, and passing it in is what lets a test drive
    // several blocks without a process-wide environment variable.
    let mut writer = Writer::new(format, dataset, level, parameters, *EXPORT_BLOCK_ROWS)?;
    if let Some(chunk) = writer.prologue()?
        && sender.blocking_send(Ok(chunk)).is_err()
    {
        return Ok(());
    }

    let mut served_from_storage: usize = 0;
    for step in range.steps() {
        let row = tape
            .row(step)
            .ok_or_else(|| ChainError::Internal(format!("the tape has no row at step {step}")))?;

        // A persisted step is the same snapshot, already priced: prefer it, and
        // price only what the warehouse does not have.
        //
        // Unless it cannot answer the question asked. A row written before the
        // greek columns existed reconstructs with no snapshots, so preferring it
        // at `greeks=all` would emit empty greek columns for the persisted
        // prefix of a range and real ones for the replayed rest — one file, two
        // coverages, no signal. Such a step is replayed instead, which costs
        // pricing and keeps the promise that the source is invisible.
        let record = match &mut stored {
            Some(stored) => stored
                .take(step, range.to)
                .filter(|record| !level.wants_greeks() || record_carries_greeks(record)),
            None => None,
        };
        let replayed = match (&record, &builder) {
            (None, Some(builder)) => Some(builder.snapshot(step)?),
            _ => None,
        };
        let chains = match (&record, &replayed) {
            (Some(record), _) => Some(StepChains::Stored(record)),
            (None, Some(snapshot)) => Some(StepChains::Replayed(snapshot)),
            (None, None) => None,
        };
        if record.is_some() {
            served_from_storage = served_from_storage
                .checked_add(1)
                .ok_or_else(|| ChainError::Internal("the step counter overflowed".to_string()))?;
        }

        let delivery = writer.rows(parameters, row.step, row, chains, &mut |chunk| {
            if chunk.is_empty() {
                return Delivery::Sent;
            }
            match sender.blocking_send(Ok(chunk)) {
                Ok(()) => Delivery::Sent,
                Err(_) => Delivery::ClientGone,
            }
        })?;
        if delivery == Delivery::ClientGone {
            return Ok(());
        }
    }

    if let Some(chunk) = writer.epilogue()?
        && sender.blocking_send(Ok(chunk)).is_err()
    {
        return Ok(());
    }

    if stored.is_some() {
        debug!(
            from_step = range.from,
            to_step = range.to,
            served_from_storage,
            "Finished a v2 export"
        );
    }
    Ok(())
}

/// Encodes rows in the requested format.
enum Writer {
    /// A streamed JSON array. Tracks whether a comma is due.
    Json {
        dataset: Dataset,
        level: GreekLevel,
        first: bool,
    },
    /// RFC 4180 CSV. A writer is built per chunk rather than kept: `csv::Writer`
    /// only surrenders its buffer by consuming itself, and constructing one is
    /// cheap next to pricing a chain.
    Csv { dataset: Dataset, level: GreekLevel },
    /// The `packed` columnar block format. Buffers at most one block, which is
    /// what keeps a columnar encoding streaming.
    Packed {
        dataset: Dataset,
        level: GreekLevel,
        writer: Box<PackedWriter>,
    },
    /// Arrow IPC stream, one record batch per block.
    #[cfg(feature = "arrow-export")]
    Arrow {
        dataset: Dataset,
        level: GreekLevel,
        writer: Box<crate::api::rest::arrow_export::ArrowWriter>,
    },
}

impl Writer {
    /// Creates a writer for a dataset and format.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming `format` when the request asks
    /// for `arrow` on a build without the `arrow-export` feature, or for a
    /// binary format on a simulation whose schedule carries more rules than a
    /// label mask holds. Returns [`ChainError::Internal`] when an Arrow stream
    /// cannot be opened.
    fn new(
        format: Format,
        dataset: Dataset,
        level: GreekLevel,
        parameters: &SimulationParametersV2,
        block_rows: usize,
    ) -> Result<Self, ChainError> {
        let binary_schema = if format.is_binary() {
            let schema = BinarySchema::new(dataset, level, parameters);
            ensure_label_capacity(&schema)?;
            Some(schema)
        } else {
            None
        };

        Ok(match format {
            Format::Json => Writer::Json {
                dataset,
                level,
                first: true,
            },
            Format::Csv => Writer::Csv { dataset, level },
            Format::Packed => {
                let schema = match binary_schema {
                    Some(schema) => schema,
                    None => BinarySchema::new(dataset, level, parameters),
                };
                Writer::Packed {
                    dataset,
                    level,
                    writer: Box::new(PackedWriter::new(schema, block_rows)),
                }
            }
            #[cfg(feature = "arrow-export")]
            Format::Arrow => {
                let schema = match binary_schema {
                    Some(schema) => schema,
                    None => BinarySchema::new(dataset, level, parameters),
                };
                Writer::Arrow {
                    dataset,
                    level,
                    writer: Box::new(crate::api::rest::arrow_export::ArrowWriter::new(
                        schema, block_rows,
                    )?),
                }
            }
            // Recognised, and refused: the value is a valid format this BUILD
            // cannot serve, which is a different answer from "no such format"
            // and must never be a 500 or a silent fallback to another encoding.
            #[cfg(not(feature = "arrow-export"))]
            Format::Arrow => return Err(arrow_unavailable()),
        })
    }

    /// The bytes that open the document, if any.
    fn prologue(&mut self) -> Result<Option<Vec<u8>>, ChainError> {
        match self {
            Writer::Json { .. } => Ok(Some(b"[".to_vec())),
            Writer::Csv { dataset, level } => {
                let header: Vec<String> = dataset
                    .header(*level)
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                Ok(Some(encode_csv(&[header])?))
            }
            Writer::Packed { writer, .. } => Ok(Some(writer.header()?)),
            #[cfg(feature = "arrow-export")]
            Writer::Arrow { writer, .. } => Ok(Some(writer.header()?)),
        }
    }

    /// The bytes that close the document, if any.
    ///
    /// For the binary formats this is where the last, partial block goes: it
    /// exists precisely because a columnar encoding cannot emit a block until
    /// it is full or the rows run out.
    fn epilogue(&mut self) -> Result<Option<Vec<u8>>, ChainError> {
        match self {
            Writer::Json { .. } => Ok(Some(b"]".to_vec())),
            Writer::Csv { .. } => Ok(None),
            Writer::Packed { writer, .. } => writer.flush(),
            #[cfg(feature = "arrow-export")]
            Writer::Arrow { writer, .. } => writer.finish(),
        }
    }

    /// Encodes every row one step contributes.
    ///
    /// `simulated_at` comes from the factor row whichever source produced the
    /// chains: it is the tape's instant, the same one a stored record was
    /// written from, and taking it from one place keeps the two sources
    /// rendering identically by construction.
    fn rows<E>(
        &mut self,
        parameters: &SimulationParametersV2,
        step: usize,
        row: &crate::domain::factors::FactorRow,
        chains: Option<StepChains<'_>>,
        emit: &mut E,
    ) -> Result<Delivery, ChainError>
    where
        E: FnMut(Vec<u8>) -> Delivery,
    {
        let simulated_at = render_instant(row.simulated_at);
        let symbol = parameters.symbol.as_str();

        match self {
            Writer::Json {
                dataset,
                level,
                first,
            } => {
                let values = json_rows(*dataset, *level, step, &simulated_at, symbol, row, chains);
                let mut chunk = Vec::new();
                let mut rows_buffered = 0_usize;
                for value in values {
                    if !*first {
                        chunk.push(b',');
                    }
                    *first = false;
                    let encoded = serde_json::to_vec(&value).map_err(|e| {
                        ChainError::Internal(format!("failed to encode an export row: {e}"))
                    })?;
                    chunk.extend_from_slice(&encoded);
                    rows_buffered += 1;

                    if rows_buffered >= ROWS_PER_CHUNK {
                        if emit(std::mem::take(&mut chunk)) == Delivery::ClientGone {
                            return Ok(Delivery::ClientGone);
                        }
                        rows_buffered = 0;
                    }
                }
                if chunk.is_empty() {
                    return Ok(Delivery::Sent);
                }
                Ok(emit(chunk))
            }
            Writer::Csv { dataset, level } => {
                let records = csv_rows(*dataset, *level, step, &simulated_at, symbol, row, chains);
                for batch in records.chunks(ROWS_PER_CHUNK) {
                    if emit(encode_csv(batch)?) == Delivery::ClientGone {
                        return Ok(Delivery::ClientGone);
                    }
                }
                Ok(Delivery::Sent)
            }
            // Both binary arms feed the writer ONE ROW AT A TIME and hand each
            // finished block straight to `emit`. Collecting a step's rows first
            // would make the footprint a function of the chain size rather than
            // of the block width, and would delay noticing a disconnected
            // client until a whole step had been encoded — at the snapshot
            // contract cap, that is the difference the block width is supposed
            // to buy.
            Writer::Packed {
                dataset,
                level,
                writer,
            } => {
                // Cloned once per step so the visitor can hold the writer
                // mutably: the schema is a handful of small vectors, next to
                // nothing beside pricing a chain.
                let schema = writer.schema().clone();
                let context = RowContext {
                    schema: &schema,
                    dataset: *dataset,
                    level: *level,
                    step,
                    simulated_at: row.simulated_at,
                    row,
                    chains,
                };
                let flow = visit_typed_rows(&context, &mut |cells| {
                    let Some(block) = writer.push_row(cells)? else {
                        return Ok(RowFlow::Continue);
                    };
                    if emit(block) == Delivery::ClientGone {
                        return Ok(RowFlow::Stop);
                    }
                    Ok(RowFlow::Continue)
                })?;
                Ok(match flow {
                    RowFlow::Continue => Delivery::Sent,
                    RowFlow::Stop => Delivery::ClientGone,
                })
            }
            #[cfg(feature = "arrow-export")]
            Writer::Arrow {
                dataset,
                level,
                writer,
            } => {
                // Cloned once per step so the visitor can hold the writer
                // mutably: the schema is a handful of small vectors, next to
                // nothing beside pricing a chain.
                let schema = writer.schema().clone();
                let context = RowContext {
                    schema: &schema,
                    dataset: *dataset,
                    level: *level,
                    step,
                    simulated_at: row.simulated_at,
                    row,
                    chains,
                };
                let flow = visit_typed_rows(&context, &mut |cells| {
                    let Some(batch) = writer.push_row(cells)? else {
                        return Ok(RowFlow::Continue);
                    };
                    if emit(batch) == Delivery::ClientGone {
                        return Ok(RowFlow::Stop);
                    }
                    Ok(RowFlow::Continue)
                })?;
                Ok(match flow {
                    RowFlow::Continue => Delivery::Sent,
                    RowFlow::Stop => Delivery::ClientGone,
                })
            }
        }
    }
}

/// Encodes records as RFC 4180 CSV.
///
/// CRLF terminators, as the RFC specifies, and quoting left entirely to the
/// crate — handling commas, quotes and newlines by hand is the class of bug the
/// dependency exists to remove.
fn encode_csv(records: &[Vec<String>]) -> Result<Vec<u8>, ChainError> {
    let mut writer = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(Vec::new());
    for record in records {
        writer.write_record(record).map_err(csv_error)?;
    }
    writer
        .into_inner()
        .map_err(|e| ChainError::Internal(format!("failed to flush the export buffer: {e}")))
}

/// Maps a CSV encoding failure into the error boundary.
#[cold]
fn csv_error(error: csv::Error) -> ChainError {
    ChainError::Internal(format!("failed to encode an export row: {error}"))
}

/// The JSON objects one step contributes.
fn json_rows(
    dataset: Dataset,
    level: GreekLevel,
    step: usize,
    simulated_at: &str,
    symbol: &str,
    row: &crate::domain::factors::FactorRow,
    chains: Option<StepChains<'_>>,
) -> Vec<serde_json::Value> {
    match dataset {
        Dataset::Underlying => vec![serde_json::json!({
            "step": step,
            "simulated_at": simulated_at,
            "symbol": symbol,
            "price": positive_to_f64(row.spot),
        })],
        Dataset::Volatility => vec![serde_json::json!({
            "step": step,
            "simulated_at": simulated_at,
            "symbol": symbol,
            "base_volatility": positive_to_f64(row.base_volatility),
        })],
        Dataset::OptionChains => {
            let Some(chains) = chains else {
                return Vec::new();
            };
            let mut rows = Vec::new();
            for expiration in chains.expirations() {
                let expires_at = render_instant(expiration.expires_at);
                let labels = expiration.labels.join("|");
                for quote in expiration.quotes.quotes() {
                    let mut value = serde_json::json!({
                        "step": step,
                        "simulated_at": simulated_at,
                        "symbol": symbol,
                        "expires_at": expires_at,
                        "labels": labels,
                        "days_to_expiration": expiration.days_to_expiration,
                        "strike": quote.strike,
                        "implied_volatility": quote.implied_volatility,
                        "call_bid": quote.call_bid,
                        "call_ask": quote.call_ask,
                        "call_mid": quote.call_mid,
                        "call_delta": quote.call_delta,
                        "put_bid": quote.put_bid,
                        "put_ask": quote.put_ask,
                        "put_mid": quote.put_mid,
                        "put_delta": quote.put_delta,
                        "gamma": quote.gamma,
                    });

                    // The same keys the CSV header carries at this level, and
                    // always all of them: a strike with no snapshot emits
                    // `null`, never a missing key, so a JSON row and a CSV row
                    // describe the same shape.
                    let mut extra: Vec<(&str, Option<f64>)> = Vec::new();
                    if level.wants_greeks() {
                        extra.extend([
                            ("call_theta", quote.call_greeks.theta),
                            ("put_theta", quote.put_greeks.theta),
                            ("call_vega", quote.call_greeks.vega),
                            ("put_vega", quote.put_greeks.vega),
                            ("call_rho", quote.call_greeks.rho),
                            ("put_rho", quote.put_greeks.rho),
                            ("call_rho_d", quote.call_greeks.rho_d),
                            ("put_rho_d", quote.put_greeks.rho_d),
                        ]);
                    }
                    if matches!(level, GreekLevel::All) {
                        extra.extend([
                            ("call_gamma", quote.call_greeks.gamma),
                            ("put_gamma", quote.put_greeks.gamma),
                            ("call_alpha", quote.call_greeks.alpha),
                            ("put_alpha", quote.put_greeks.alpha),
                            ("call_vanna", quote.call_greeks.vanna),
                            ("put_vanna", quote.put_greeks.vanna),
                            ("call_vomma", quote.call_greeks.vomma),
                            ("put_vomma", quote.put_greeks.vomma),
                            ("call_veta", quote.call_greeks.veta),
                            ("put_veta", quote.put_greeks.veta),
                            ("call_charm", quote.call_greeks.charm),
                            ("put_charm", quote.put_greeks.charm),
                            ("call_color", quote.call_greeks.color),
                            ("put_color", quote.put_greeks.color),
                        ]);
                    }
                    if let Some(object) = value.as_object_mut() {
                        for (key, greek) in extra {
                            object.insert(
                                key.to_string(),
                                match greek {
                                    Some(greek) => serde_json::json!(greek),
                                    None => serde_json::Value::Null,
                                },
                            );
                        }
                    }
                    rows.push(value);
                }
            }
            rows
        }
    }
}

/// How many rows one chunk carries.
///
/// The export streams, but a chunk is indivisible: the channel bounds how many
/// are in flight, not how large they are, so a whole step in one chunk means a
/// slow client can hold the entire step in memory no matter how small the
/// channel is. At the per-snapshot cap of 200 000 contracts a `greeks=all` step
/// is hundreds of megabytes, and sixteen of those in flight is gigabytes.
///
/// Rows rather than bytes because a row's width is fixed by the dataset and the
/// greek level, so a row count IS a byte bound: 512 rows of the widest shape the
/// `option_chains` dataset can produce is a few hundred kilobytes, and the
/// narrower datasets are far less. Backpressure then applies within a step
/// rather than only between steps.
const ROWS_PER_CHUNK: usize = 512;

/// Whether a chunk reached the client.
///
/// A `bool` would read as "success", and the interesting case here is the one
/// that is neither success nor failure: the client hung up, which is not an
/// error to report to anyone, just a reason to stop working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// The chunk was handed to the response stream.
    Sent,
    /// The receiver is gone. Stop producing; nothing is owed to anyone.
    ClientGone,
}

/// The CSV records one step contributes, in the header's order.
fn csv_rows(
    dataset: Dataset,
    level: GreekLevel,
    step: usize,
    simulated_at: &str,
    symbol: &str,
    row: &crate::domain::factors::FactorRow,
    chains: Option<StepChains<'_>>,
) -> Vec<Vec<String>> {
    match dataset {
        Dataset::Underlying => vec![vec![
            step.to_string(),
            simulated_at.to_string(),
            symbol.to_string(),
            positive_to_f64(row.spot).to_string(),
        ]],
        Dataset::Volatility => vec![vec![
            step.to_string(),
            simulated_at.to_string(),
            symbol.to_string(),
            positive_to_f64(row.base_volatility).to_string(),
        ]],
        Dataset::OptionChains => {
            let Some(chains) = chains else {
                return Vec::new();
            };
            let mut records = Vec::new();
            for expiration in chains.expirations() {
                let expires_at = render_instant(expiration.expires_at);
                // Joined with `|` rather than `,` so a multi-label chain stays
                // one column without depending on quoting to do it.
                let labels = expiration.labels.join("|");
                for quote in expiration.quotes.quotes() {
                    let mut record = vec![
                        step.to_string(),
                        simulated_at.to_string(),
                        symbol.to_string(),
                        expires_at.clone(),
                        labels.clone(),
                        expiration.days_to_expiration.to_string(),
                        quote.strike.to_string(),
                        quote.implied_volatility.to_string(),
                        render_optional(quote.call_bid),
                        render_optional(quote.call_ask),
                        render_optional(quote.call_mid),
                        render_optional(quote.call_delta),
                        render_optional(quote.put_bid),
                        render_optional(quote.put_ask),
                        render_optional(quote.put_mid),
                        render_optional(quote.put_delta),
                        render_optional(quote.gamma),
                    ];
                    // Appended in the header's order, and ALWAYS the same count
                    // for a level: a strike with no snapshot writes empty
                    // fields, never fewer of them.
                    if level.wants_greeks() {
                        record.push(render_optional(quote.call_greeks.theta));
                        record.push(render_optional(quote.put_greeks.theta));
                        record.push(render_optional(quote.call_greeks.vega));
                        record.push(render_optional(quote.put_greeks.vega));
                        record.push(render_optional(quote.call_greeks.rho));
                        record.push(render_optional(quote.put_greeks.rho));
                        record.push(render_optional(quote.call_greeks.rho_d));
                        record.push(render_optional(quote.put_greeks.rho_d));
                    }
                    if matches!(level, GreekLevel::All) {
                        record.push(render_optional(quote.call_greeks.gamma));
                        record.push(render_optional(quote.put_greeks.gamma));
                        record.push(render_optional(quote.call_greeks.alpha));
                        record.push(render_optional(quote.put_greeks.alpha));
                        record.push(render_optional(quote.call_greeks.vanna));
                        record.push(render_optional(quote.put_greeks.vanna));
                        record.push(render_optional(quote.call_greeks.vomma));
                        record.push(render_optional(quote.put_greeks.vomma));
                        record.push(render_optional(quote.call_greeks.veta));
                        record.push(render_optional(quote.put_greeks.veta));
                        record.push(render_optional(quote.call_greeks.charm));
                        record.push(render_optional(quote.put_greeks.charm));
                        record.push(render_optional(quote.call_greeks.color));
                        record.push(render_optional(quote.put_greeks.color));
                    }
                    records.push(record);
                }
            }
            records
        }
    }
}

/// Converts a decimal to the wire's `f64`, by VALUE and not by representation.
///
/// The normalisation is the whole point. A `Decimal` is a mantissa and a scale,
/// so one value has many forms — `0.5415620196854147` and the same number
/// padded to twenty-eight decimals are equal and hold different mantissas — and
/// `to_f64` reads the mantissa, so the two can land on adjacent floats. The
/// warehouse round trip changes exactly that: a value is scaled out to the
/// column's twenty-eight decimals on the way in and has its trailing zeros
/// stripped on the way back.
///
/// Without this, the same tape exported after its rows were filed rendered a
/// digit differently from the same tape replayed, and two exports of one
/// simulation stopped being byte-identical (issue #152). Normalising first
/// makes the rendering depend on the number, which is what a client compares.
#[must_use]
#[inline]
fn decimal_to_f64(value: rust_decimal::Decimal) -> Option<f64> {
    use rust_decimal::prelude::ToPrimitive;
    value.normalize().to_f64()
}

/// Converts a positive value to the wire's `f64`, through [`decimal_to_f64`].
///
/// `Positive::to_f64` goes straight to the float and carries the scale
/// sensitivity with it, so every conversion in this module goes through here
/// instead.
#[must_use]
#[inline]
fn positive_to_f64(value: positive::Positive) -> f64 {
    // A `Positive` is finite and in range by construction, so the fallback is
    // unreachable; it is the value `to_f64` itself would have produced.
    decimal_to_f64(value.to_dec()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::routes::configure_v2_routes;
    use crate::infrastructure::SimulationV2Config;
    use crate::session::InMemorySimulationStore;
    use actix_web::App;
    use actix_web::http::StatusCode;
    use actix_web::test as actix_test;
    use serde_json::{Value, json};

    /// A value renders the same however it was written down.
    ///
    /// The warehouse round trip preserves the number and not its form: a value
    /// is scaled out to the column's twenty-eight decimals on the way in and
    /// has its trailing zeros stripped on the way back, so the same quote
    /// arrives with a different mantissa than the one that was priced.
    /// `Decimal::to_f64` reads the mantissa, so without normalising, the two
    /// land on adjacent floats and the export of a stored tape differs from
    /// the export of a replayed one (issue #152).
    #[test]
    fn test_a_value_renders_the_same_whatever_scale_it_carries() {
        use rust_decimal::Decimal;

        let mut checked = 0_usize;
        for numerator in 1..2_000_i128 {
            // A long expansion, which is what a mid price computed from a
            // quote actually looks like.
            let priced = (Decimal::from(numerator) / Decimal::from(7_919)).normalize();
            let shift = match 28_u32.checked_sub(priced.scale()) {
                Some(shift) => shift,
                None => continue,
            };
            let factor = match 10_i128.checked_pow(shift) {
                Some(factor) => factor,
                None => continue,
            };
            let mantissa = match priced.mantissa().checked_mul(factor) {
                Some(mantissa) => mantissa,
                None => continue,
            };
            // The same number as the storage column holds it.
            let Ok(padded) = Decimal::try_from_i128_with_scale(mantissa, 28) else {
                continue;
            };
            assert_eq!(priced, padded, "the two forms must be the same number");

            let priced_rendered = decimal_to_f64(priced);
            let padded_rendered = decimal_to_f64(padded);
            assert_eq!(
                priced_rendered.map(f64::to_bits),
                padded_rendered.map(f64::to_bits),
                "{priced} renders as {priced_rendered:?} priced and {padded_rendered:?} stored, \
                 so an export of a filed tape would differ from the same tape replayed"
            );
            checked += 1;
        }

        assert!(
            checked > 1_000,
            "the case is only covered if the values actually exercised it, {checked} did"
        );
    }

    /// A quote read back from the warehouse views exactly as the priced one.
    ///
    /// The end of the same defect, one layer up: both sources are reduced to a
    /// `QuoteView`, and it is that reduction which has to be blind to how the
    /// value was written. The round trip is reproduced here rather than called
    /// into: what the column does to a value is scale it out and strip the
    /// zeros back off, and this layer must survive that whoever performs it.
    #[test]
    fn test_a_stored_quote_views_as_the_priced_one_does() {
        use crate::infrastructure::QuoteRow;
        use positive::Positive;
        use rust_decimal::Decimal;

        /// A `Positive` that keeps the decimal's own form, which is the point.
        fn exactly(value: Decimal) -> Positive {
            match Positive::new_decimal(value) {
                Ok(value) => value,
                Err(error) => panic!("the reference value must be positive: {error}"),
            }
        }

        let Some(priced_mid) = Decimal::from(4_291_i64).checked_div(Decimal::from(7_919_i64))
        else {
            panic!("the reference mid must divide");
        };
        let priced_mid = priced_mid.normalize();

        // Through the column and back: scaled to twenty-eight decimals, then
        // returned with its trailing zeros stripped.
        let stored_mid = {
            let Some(shift) = 28_u32.checked_sub(priced_mid.scale()) else {
                panic!("the reference mid must fit the column's scale");
            };
            let Some(factor) = 10_i128.checked_pow(shift) else {
                panic!("the scale factor must exist");
            };
            let Some(mantissa) = priced_mid.mantissa().checked_mul(factor) else {
                panic!("the reference mid must fit the column");
            };
            let mut mantissa = mantissa;
            let mut scale = 28_u32;
            while scale > 0 && mantissa % 10 == 0 {
                mantissa /= 10;
                scale -= 1;
            }
            match Decimal::try_from_i128_with_scale(mantissa, scale) {
                Ok(value) => value,
                Err(error) => panic!("the stored mid must read back: {error}"),
            }
        };
        assert_eq!(
            stored_mid, priced_mid,
            "the round trip must preserve the value, or this test is about something else"
        );

        let strike = exactly(Decimal::from(5_000_i64));
        let priced_view = QuoteView::stored(&QuoteRow::new(strike, exactly(priced_mid)).with_call(
            None,
            None,
            Some(exactly(priced_mid)),
            None,
        ));
        let stored_view = QuoteView::stored(&QuoteRow::new(strike, exactly(stored_mid)).with_call(
            None,
            None,
            Some(exactly(stored_mid)),
            None,
        ));

        assert_eq!(
            priced_view.call_mid.map(f64::to_bits),
            stored_view.call_mid.map(f64::to_bits),
            "a mid that went through storage views as {:?} against {:?} priced",
            stored_view.call_mid,
            priced_view.call_mid
        );
        assert_eq!(
            priced_view.implied_volatility.to_bits(),
            stored_view.implied_volatility.to_bits(),
            "an implied volatility that went through storage views as {} against {} priced",
            stored_view.implied_volatility,
            priced_view.implied_volatility
        );
    }

    /// The reference configuration of ADR 0001 §14, trimmed to a few steps and
    /// a narrow ladder so a full option-chains export stays quick.
    fn reference_body() -> Value {
        json!({
            "symbol": "SPX",
            "steps": 3,
            "start_at": "2026-01-05T14:30:00Z",
            "step_interval_seconds": 86400,
            "timezone": "America/New_York",
            "expiration_time": "17:00",
            "schedules": [
                { "rule_id": "zero_dte", "kind": "daily", "target_count": 1 },
                { "rule_id": "weeklies", "kind": "weekly", "target_count": 3,
                  "weekdays": ["Mon", "Wed", "Fri"] }
            ],
            "initial_price": 5000.0,
            "volatility": 0.18,
            "risk_free_rate": 0.04,
            "dividend_yield": 0.0,
            "method": { "Brownian": { "dt": 0.004, "drift": 0.0, "volatility": 0.18 } },
            "time_frame": "Day",
            "chain_size": 3,
            "strike_interval": 25.0,
            "spread": 0.02,
            "seed": 42
        })
    }

    /// Mounts the real v2 routes over an in-memory store, with no warehouse.
    ///
    /// The argument form mounts one, which is how the tests below exercise the
    /// stored path over the very same route registration production uses.
    macro_rules! v2_service {
        () => {
            v2_service!(None)
        };
        ($snapshots:expr) => {{
            let manager = Arc::new(crate::session::SimulationManager::new(
                Arc::new(InMemorySimulationStore::new()),
                SimulationV2Config::default(),
            ));
            let snapshots: Option<Arc<dyn SimulationSnapshotRepository>> = $snapshots;
            actix_test::init_service(
                App::new()
                    .configure(|cfg| configure_v2_routes(cfg, manager.clone(), snapshots.clone())),
            )
            .await
        }};
    }

    macro_rules! create {
        ($app:expr) => {{
            let request = actix_test::TestRequest::post()
                .uri("/api/v2/simulations")
                .set_json(reference_body())
                .to_request();
            let response = actix_test::call_service(&$app, request).await;
            assert_eq!(response.status(), StatusCode::CREATED);
            let body: Value = actix_test::read_body_json(response).await;
            match body.get("id").and_then(Value::as_str) {
                Some(id) => id.to_string(),
                None => panic!("the response must carry an id: {body}"),
            }
        }};
    }

    macro_rules! export {
        ($app:expr, $id:expr, $query:expr) => {{
            let uri = format!("/api/v2/simulations/{}/export?{}", $id, $query);
            let response = actix_test::call_service(
                &$app,
                actix_test::TestRequest::get().uri(&uri).to_request(),
            )
            .await;
            let status = response.status();
            let body = actix_test::read_body(response).await;
            (status, String::from_utf8_lossy(&body).to_string())
        }};
    }

    /// Downloads an export as raw bytes, for the binary encodings.
    macro_rules! export_bytes {
        ($app:expr, $id:expr, $query:expr) => {{
            let uri = format!("/api/v2/simulations/{}/export?{}", $id, $query);
            let response = actix_test::call_service(
                &$app,
                actix_test::TestRequest::get().uri(&uri).to_request(),
            )
            .await;
            let status = response.status();
            let body = actix_test::read_body(response).await;
            (status, body.to_vec())
        }};
    }

    /// The CSV body as rows of fields, header dropped.
    fn csv_records_of(body: &str) -> Vec<Vec<String>> {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_reader(body.as_bytes());
        reader
            .records()
            .map(|record| match record {
                Ok(record) => record.iter().map(ToString::to_string).collect(),
                Err(error) => panic!("the csv must parse: {error}"),
            })
            .collect()
    }

    /// Counts CSV data rows, excluding the header and the trailing terminator.
    fn csv_rows_of(body: &str) -> usize {
        body.split("\r\n").filter(|line| !line.is_empty()).count() - 1
    }

    fn json_rows_of(body: &str) -> Vec<Value> {
        match serde_json::from_str::<Value>(body) {
            Ok(Value::Array(rows)) => rows,
            other => panic!("a JSON export must be an array, got {other:?}"),
        }
    }

    // ---- shape -----------------------------------------------------------

    /// Every dataset is downloadable in both formats, and the two agree on how
    /// many rows they carry.
    #[actix_web::test]
    async fn test_every_dataset_exports_in_both_formats_with_equal_row_counts() {
        let app = v2_service!();
        let id = create!(app);

        for dataset in ["underlying", "volatility", "option_chains"] {
            let (json_status, json_body) =
                export!(app, id, format!("dataset={dataset}&format=json"));
            let (csv_status, csv_body) = export!(app, id, format!("dataset={dataset}&format=csv"));

            assert_eq!(json_status, StatusCode::OK, "{dataset} json");
            assert_eq!(csv_status, StatusCode::OK, "{dataset} csv");
            assert_eq!(
                json_rows_of(&json_body).len(),
                csv_rows_of(&csv_body),
                "{dataset}: the two encodings must carry the same rows"
            );
        }
    }

    /// The per-step datasets carry exactly one row per step, and read straight
    /// off the factor tape.
    #[actix_web::test]
    async fn test_the_per_step_datasets_carry_one_row_per_step() {
        let app = v2_service!();
        let id = create!(app);

        for dataset in ["underlying", "volatility"] {
            let (_, body) = export!(app, id, format!("dataset={dataset}&format=json"));
            let rows = json_rows_of(&body);

            assert_eq!(rows.len(), 3, "{dataset}");
            for (index, row) in rows.iter().enumerate() {
                assert_eq!(row.get("step"), Some(&json!(index)));
                assert_eq!(row.get("symbol"), Some(&json!("SPX")));
            }
        }
    }

    /// The option-chains dataset carries one row per (step × expiration ×
    /// strike), with every documented column.
    #[actix_web::test]
    async fn test_the_option_chains_dataset_carries_every_documented_column() {
        let app = v2_service!();
        let id = create!(app);

        let (_, body) = export!(app, id, "dataset=option_chains&format=json");
        let rows = json_rows_of(&body);
        assert!(!rows.is_empty());

        let first = match rows.first() {
            Some(first) => first,
            None => panic!("the export must carry rows"),
        };
        for column in Dataset::OptionChains.header(GreekLevel::None) {
            assert!(
                first.get(column).is_some(),
                "the export must carry {column}: {first}"
            );
        }
    }

    /// Rows are ordered by step, then expiration, then strike.
    #[actix_web::test]
    async fn test_rows_are_ordered_by_step_then_expiration_then_strike() {
        let app = v2_service!();
        let id = create!(app);

        let (_, body) = export!(app, id, "dataset=option_chains&format=json");
        let rows = json_rows_of(&body);

        let key = |row: &Value| {
            (
                row.get("step").and_then(Value::as_u64).unwrap_or_default(),
                row.get("expires_at")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                row.get("strike")
                    .and_then(Value::as_f64)
                    .unwrap_or_default()
                    .to_string(),
            )
        };
        let keys: Vec<_> = rows.iter().map(key).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "the export must be in its documented order");
    }

    /// A coincident expiration keeps every label, joined with a pipe so it
    /// stays one CSV column.
    #[actix_web::test]
    async fn test_overlapping_labels_stay_one_column() {
        let app = v2_service!();
        let id = create!(app);

        let (_, body) = export!(app, id, "dataset=option_chains&format=csv");
        assert!(
            body.contains("weeklies|zero_dte"),
            "a shared expiration must carry both labels in one column"
        );
        assert!(
            !body.contains("\"weeklies"),
            "the pipe join means the labels never need quoting"
        );
    }

    /// The CSV is RFC 4180: a header row and CRLF terminators.
    #[actix_web::test]
    async fn test_the_csv_is_rfc_4180() {
        let app = v2_service!();
        let id = create!(app);

        let (_, body) = export!(app, id, "dataset=underlying&format=csv");

        assert!(body.starts_with("step,simulated_at,symbol,price\r\n"));
        assert!(body.ends_with("\r\n"));
    }

    // ---- the binary encodings (issue #78) --------------------------------

    /// A `packed` export decodes to exactly the values the CSV renders.
    ///
    /// The cross-format equality this issue is about, asserted per dataset and
    /// per row: the binary encodings are a faster route to the same numbers,
    /// not a different answer.
    #[actix_web::test]
    async fn test_a_packed_export_decodes_to_the_csv_values() {
        let app = v2_service!();
        let id = create!(app);

        for dataset in ["underlying", "volatility", "option_chains"] {
            let (status, text) = export!(app, id, format!("dataset={dataset}&format=csv"));
            assert_eq!(status, StatusCode::OK);
            let (binary_status, bytes) =
                export_bytes!(app, id, format!("dataset={dataset}&format=packed"));
            assert_eq!(binary_status, StatusCode::OK);

            let expected = csv_records_of(&text);
            match crate::api::rest::binary::decode_packed(&bytes) {
                Ok((names, rows)) => {
                    assert_eq!(
                        names,
                        text.lines()
                            .next()
                            .unwrap_or_default()
                            .split(',')
                            .collect::<Vec<_>>(),
                        "the packed columns must be the csv header, in order, for {dataset}"
                    );
                    assert_eq!(rows.len(), expected.len(), "row count for {dataset}");
                    assert_eq!(rows, expected, "values for {dataset}");
                }
                Err(error) => panic!("the {dataset} export must decode: {error}"),
            }
        }
    }

    /// Exporting the same simulation twice produces byte-identical output, in
    /// every format.
    #[actix_web::test]
    async fn test_every_format_is_byte_identical_on_repeat() {
        let app = v2_service!();
        let id = create!(app);

        let mut formats = vec!["json", "csv", "packed"];
        if cfg!(feature = "arrow-export") {
            formats.push("arrow");
        }
        for format in formats {
            let (_, first) =
                export_bytes!(app, id, format!("dataset=option_chains&format={format}"));
            let (_, second) =
                export_bytes!(app, id, format!("dataset=option_chains&format={format}"));
            assert_eq!(first, second, "{format} must be reproducible byte for byte");
        }
    }

    /// `from_step` and `to_step` behave identically in the Arrow encoding.
    #[cfg(feature = "arrow-export")]
    #[actix_web::test]
    async fn test_a_step_range_behaves_the_same_in_arrow() {
        let app = v2_service!();
        let id = create!(app);

        let (_, text) = export!(
            app,
            id,
            "dataset=underlying&format=csv&from_step=1&to_step=2"
        );
        let (status, bytes) = export_bytes!(
            app,
            id,
            "dataset=underlying&format=arrow&from_step=1&to_step=2"
        );
        assert_eq!(status, StatusCode::OK);

        match crate::api::rest::arrow_export::decode_stream(&bytes) {
            Ok(rows) => assert_eq!(rows, csv_records_of(&text)),
            Err(error) => panic!("the ranged export must decode: {error}"),
        }

        let (status, body) = export!(
            app,
            id,
            "dataset=underlying&format=arrow&from_step=3&to_step=1"
        );
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    /// `from_step` and `to_step` behave identically in the binary formats.
    #[actix_web::test]
    async fn test_a_step_range_behaves_the_same_in_packed() {
        let app = v2_service!();
        let id = create!(app);

        let (_, text) = export!(
            app,
            id,
            "dataset=underlying&format=csv&from_step=1&to_step=2"
        );
        let (status, bytes) = export_bytes!(
            app,
            id,
            "dataset=underlying&format=packed&from_step=1&to_step=2"
        );
        assert_eq!(status, StatusCode::OK);

        match crate::api::rest::binary::decode_packed(&bytes) {
            Ok((_, rows)) => assert_eq!(rows, csv_records_of(&text)),
            Err(error) => panic!("the ranged export must decode: {error}"),
        }

        // And an invalid range is refused the same way, before any bytes.
        let (status, body) = export!(
            app,
            id,
            "dataset=underlying&format=packed&from_step=3&to_step=1"
        );
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    /// An absent value stays absent through the encoding.
    ///
    /// End to end, against whatever the fixture produces. The stronger
    /// assertion — that a null is a cleared validity bit rather than a zero or
    /// a NaN, which are both values a chain can legitimately hold — is pinned
    /// at the encoder, in `binary::tests`.
    #[actix_web::test]
    async fn test_a_null_survives_the_packed_encoding() {
        let app = v2_service!();
        let id = create!(app);

        let (_, text) = export!(app, id, "dataset=option_chains&format=csv");
        let expected = csv_records_of(&text);
        let empties: usize = expected
            .iter()
            .map(|record| record.iter().filter(|field| field.is_empty()).count())
            .sum();

        let (_, bytes) = export_bytes!(app, id, "dataset=option_chains&format=packed");
        match crate::api::rest::binary::decode_packed(&bytes) {
            Ok((_, rows)) => {
                let decoded: usize = rows
                    .iter()
                    .map(|record| record.iter().filter(|field| field.is_empty()).count())
                    .sum();
                assert_eq!(decoded, empties, "every null must survive as a null");
            }
            Err(error) => panic!("the export must decode: {error}"),
        }
    }

    /// The response advertises the format it actually sent.
    #[actix_web::test]
    async fn test_the_binary_formats_advertise_their_content_type() {
        let app = v2_service!();
        let id = create!(app);

        let uri = format!("/api/v2/simulations/{id}/export?dataset=underlying&format=packed");
        let response =
            actix_test::call_service(&app, actix_test::TestRequest::get().uri(&uri).to_request())
                .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(actix_web::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/octet-stream")
        );
        let disposition = response
            .headers()
            .get(actix_web::http::header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(disposition.ends_with(".ocsp\""), "{disposition}");
    }

    /// Asking for `arrow` on a build without the feature is a typed 400 naming
    /// the format, never a 500 and never a silent fallback to another encoding.
    #[cfg(not(feature = "arrow-export"))]
    #[actix_web::test]
    async fn test_arrow_without_the_feature_is_a_typed_400() {
        let app = v2_service!();
        let id = create!(app);

        let (status, body) = export!(app, id, "dataset=underlying&format=arrow");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: Value = match serde_json::from_str(&body) {
            Ok(error) => error,
            Err(parse_error) => panic!("the rejection must be JSON: {parse_error}, body {body}"),
        };
        assert_eq!(
            error.get("field"),
            Some(&Value::String("format".to_string()))
        );
    }

    /// An `arrow` export decodes to the same values the CSV renders.
    #[cfg(feature = "arrow-export")]
    #[actix_web::test]
    async fn test_an_arrow_export_decodes_to_the_csv_values() {
        let app = v2_service!();
        let id = create!(app);

        for dataset in ["underlying", "volatility", "option_chains"] {
            let (_, text) = export!(app, id, format!("dataset={dataset}&format=csv"));
            let (status, bytes) = export_bytes!(app, id, format!("dataset={dataset}&format=arrow"));
            assert_eq!(status, StatusCode::OK);

            match crate::api::rest::arrow_export::decode_stream(&bytes) {
                Ok(rows) => {
                    // Value by value, not merely shape: the decoder renders
                    // timestamps the way the CSV does, so the two compare
                    // directly and the equality claim means something.
                    assert_eq!(rows, csv_records_of(&text), "values for {dataset}");
                }
                Err(error) => panic!("the {dataset} export must decode: {error}"),
            }
        }
    }

    /// One step of the reference simulation, priced.
    fn snapshot_of(
        parameters: &SimulationParametersV2,
        tape: &FactorTape,
        step: usize,
    ) -> Option<SeriesSnapshot> {
        let builder = match SeriesBuilder::new(parameters, tape) {
            Ok(builder) => builder.with_greek_snapshots(true),
            Err(error) => panic!("the builder must accept the parameters: {error}"),
        };
        match builder.snapshot(step) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => panic!("the snapshot must build: {error}"),
        }
    }

    /// A packed export split across several blocks carries the same rows.
    ///
    /// Every other export in this suite fits one block, so without this the
    /// blocking path — the whole reason a columnar encoding can stream — is
    /// exercised only by the encoder's own unit tests.
    #[actix_web::test]
    async fn test_a_multi_block_packed_export_carries_every_row() {
        let parameters = reference_parameters();
        let tape = match FactorTape::build(&parameters, &parameters.method) {
            Ok(tape) => tape,
            Err(error) => panic!("the tape must build: {error}"),
        };
        let row = match tape.row(0) {
            Some(row) => row,
            None => panic!("the tape must have a first row"),
        };

        let mut narrow = Vec::new();
        let mut wide = Vec::new();
        for (block_rows, sink) in [(2_usize, &mut narrow), (4_096_usize, &mut wide)] {
            let mut writer = match Writer::new(
                Format::Packed,
                Dataset::OptionChains,
                GreekLevel::None,
                &parameters,
                block_rows,
            ) {
                Ok(writer) => writer,
                Err(error) => panic!("the writer must build: {error}"),
            };
            if let Some(prologue) = match writer.prologue() {
                Ok(prologue) => prologue,
                Err(error) => panic!("the prologue must encode: {error}"),
            } {
                sink.extend(prologue);
            }

            let chains = match snapshot_of(&parameters, &tape, 0) {
                Some(snapshot) => snapshot,
                None => panic!("step zero must produce chains"),
            };
            match writer.rows(
                &parameters,
                0,
                row,
                Some(StepChains::Replayed(&chains)),
                &mut |chunk| {
                    sink.extend(chunk);
                    Delivery::Sent
                },
            ) {
                Ok(Delivery::Sent) => {}
                other => panic!("the rows must be delivered, got {other:?}"),
            }
            if let Some(epilogue) = match writer.epilogue() {
                Ok(epilogue) => epilogue,
                Err(error) => panic!("the epilogue must encode: {error}"),
            } {
                sink.extend(epilogue);
            }
        }

        let narrow_rows = match crate::api::rest::binary::decode_packed(&narrow) {
            Ok((_, rows)) => rows,
            Err(error) => panic!("the narrow document must decode: {error}"),
        };
        let wide_rows = match crate::api::rest::binary::decode_packed(&wide) {
            Ok((_, rows)) => rows,
            Err(error) => panic!("the wide document must decode: {error}"),
        };

        assert!(
            narrow_rows.len() > 2,
            "the fixture must span several blocks"
        );
        assert_eq!(
            narrow_rows, wide_rows,
            "the block width must not change a single value"
        );
    }

    /// A binary step emits blocks AS THEY FILL, not after the step.
    ///
    /// The claim the block width exists to make: at the snapshot contract cap a
    /// step can carry two hundred thousand rows, so collecting them before the
    /// first block reached the client would make memory a function of the chain
    /// size and would delay noticing a disconnect until the whole step had been
    /// encoded. Asserted by counting how many blocks arrive before the step
    /// finishes, at a block width of two.
    #[actix_web::test]
    async fn test_a_binary_step_emits_blocks_as_they_fill() {
        let parameters = reference_parameters();
        let tape = match FactorTape::build(&parameters, &parameters.method) {
            Ok(tape) => tape,
            Err(error) => panic!("the tape must build: {error}"),
        };
        let row = match tape.row(0) {
            Some(row) => row,
            None => panic!("the tape must have a first row"),
        };
        let chains = match snapshot_of(&parameters, &tape, 0) {
            Some(snapshot) => snapshot,
            None => panic!("step zero must produce chains"),
        };

        let mut writer = match Writer::new(
            Format::Packed,
            Dataset::OptionChains,
            GreekLevel::None,
            &parameters,
            2,
        ) {
            Ok(writer) => writer,
            Err(error) => panic!("the writer must build: {error}"),
        };

        let mut blocks = 0_usize;
        match writer.rows(
            &parameters,
            0,
            row,
            Some(StepChains::Replayed(&chains)),
            &mut |_chunk| {
                blocks += 1;
                Delivery::Sent
            },
        ) {
            Ok(Delivery::Sent) => {}
            other => panic!("the rows must be delivered, got {other:?}"),
        }

        assert!(
            blocks > 1,
            "one step must produce several blocks at a width of two, got {blocks}"
        );
    }

    /// A disconnect stops the step at the block it happened on.
    #[actix_web::test]
    async fn test_a_disconnect_stops_a_binary_step_immediately() {
        let parameters = reference_parameters();
        let tape = match FactorTape::build(&parameters, &parameters.method) {
            Ok(tape) => tape,
            Err(error) => panic!("the tape must build: {error}"),
        };
        let row = match tape.row(0) {
            Some(row) => row,
            None => panic!("the tape must have a first row"),
        };
        let chains = match snapshot_of(&parameters, &tape, 0) {
            Some(snapshot) => snapshot,
            None => panic!("step zero must produce chains"),
        };

        let mut writer = match Writer::new(
            Format::Packed,
            Dataset::OptionChains,
            GreekLevel::None,
            &parameters,
            2,
        ) {
            Ok(writer) => writer,
            Err(error) => panic!("the writer must build: {error}"),
        };

        let mut delivered = 0_usize;
        let outcome = writer.rows(
            &parameters,
            0,
            row,
            Some(StepChains::Replayed(&chains)),
            &mut |_chunk| {
                delivered += 1;
                Delivery::ClientGone
            },
        );

        match outcome {
            Ok(Delivery::ClientGone) => assert_eq!(
                delivered, 1,
                "the step must stop at the first refused block, not encode the rest"
            ),
            other => panic!("a gone client must be reported, got {other:?}"),
        }
    }

    /// A row's cell types are the ones the schema declares, for every dataset
    /// and every greek level.
    ///
    /// `BinarySchema::new` assigns types by matching column NAMES with an
    /// `f64` fallback, so a column added to the header lists — which the greek
    /// levels do, and which issue #75 did — lands on `f64` by default. A
    /// mismatch would not panic: the encoder writes a value's own width while a
    /// decoder reads the column's, and `Dictionary` is the one four-byte type,
    /// so one wrong cell desynchronises the rest of the block.
    #[actix_web::test]
    async fn test_every_row_matches_its_declared_schema() {
        let parameters = reference_parameters();
        let tape = match FactorTape::build(&parameters, &parameters.method) {
            Ok(tape) => tape,
            Err(error) => panic!("the tape must build: {error}"),
        };
        let row = match tape.row(0) {
            Some(row) => row,
            None => panic!("the tape must have a first row"),
        };
        let chains = match snapshot_of(&parameters, &tape, 0) {
            Some(snapshot) => snapshot,
            None => panic!("step zero must produce chains"),
        };

        for dataset in [
            Dataset::Underlying,
            Dataset::Volatility,
            Dataset::OptionChains,
        ] {
            for level in [GreekLevel::None, GreekLevel::First, GreekLevel::All] {
                let schema = BinarySchema::new(dataset, level, &parameters);
                let context = crate::api::rest::binary::RowContext {
                    schema: &schema,
                    dataset,
                    level,
                    step: 0,
                    simulated_at: row.simulated_at,
                    row,
                    chains: Some(StepChains::Replayed(&chains)),
                };
                let rows = match crate::api::rest::binary::typed_rows(&context) {
                    Ok(rows) => rows,
                    Err(error) => panic!("{dataset:?} at {level:?} must encode: {error}"),
                };

                assert!(!rows.is_empty(), "{dataset:?} at {level:?} produced no row");
                for cells in &rows {
                    assert_eq!(
                        cells.len(),
                        schema.types.len(),
                        "{dataset:?} at {level:?}: a row must carry every declared column"
                    );
                    for (position, (cell, declared)) in cells.iter().zip(&schema.types).enumerate()
                    {
                        assert_eq!(
                            cell.cell_type(),
                            *declared,
                            "{dataset:?} at {level:?}: column {} ({}) carries the wrong type",
                            position,
                            schema.names.get(position).copied().unwrap_or("?")
                        );
                    }
                }
            }
        }
    }

    /// Records the size and wall time of every format, for the docs.
    ///
    /// Ignored by default: it is a measurement, not an assertion, and its
    /// numbers only mean something from a release build. Run it with
    /// `cargo test --release --all-features -- --ignored --nocapture`.
    #[actix_web::test]
    #[ignore = "a measurement, not an assertion; run it from a release build"]
    async fn test_measure_every_format() {
        let app = v2_service!();
        let id = create!(app);

        for format in ["json", "csv", "packed", "arrow"] {
            let started = std::time::Instant::now();
            let (status, bytes) = export_bytes!(
                app,
                id,
                format!("dataset=option_chains&format={format}&greeks=all")
            );
            let elapsed = started.elapsed();
            if status != StatusCode::OK {
                println!("{format:>7}: unavailable in this build");
                continue;
            }
            println!(
                "{format:>7}: {:>9} bytes  {:>7.1} ms",
                bytes.len(),
                elapsed.as_secs_f64() * 1000.0
            );
        }
    }

    /// The JSON is a single valid array, even though it is streamed in chunks.
    #[actix_web::test]
    async fn test_the_json_is_a_single_valid_array() {
        let app = v2_service!();
        let id = create!(app);

        let (_, body) = export!(app, id, "dataset=option_chains&format=json");

        assert!(body.starts_with('['));
        assert!(body.ends_with(']'));
        assert!(!json_rows_of(&body).is_empty());
    }

    // ---- determinism and isolation ---------------------------------------

    /// Repeating an export yields byte-identical output.
    #[actix_web::test]
    async fn test_a_repeated_export_is_byte_identical() {
        let app = v2_service!();
        let id = create!(app);

        for query in [
            "dataset=underlying&format=csv",
            "dataset=option_chains&format=json",
        ] {
            let (_, first) = export!(app, id, query);
            let (_, second) = export!(app, id, query);
            assert_eq!(first, second, "{query} must be byte-identical on a repeat");
        }
    }

    /// Two simulations with the same seed export identical tapes.
    #[actix_web::test]
    async fn test_the_same_seed_exports_an_identical_tape() {
        let app = v2_service!();
        let first = create!(app);
        let second = create!(app);
        assert_ne!(first, second);

        let (_, left) = export!(app, first, "dataset=option_chains&format=csv");
        let (_, right) = export!(app, second, "dataset=option_chains&format=csv");

        assert_eq!(left, right);
    }

    /// A different seed exports a different tape, while the schedule-driven
    /// expirations stay the same.
    #[actix_web::test]
    async fn test_a_different_seed_exports_a_different_tape() {
        let app = v2_service!();
        let baseline = create!(app);

        let mut body = reference_body();
        body["seed"] = json!(43);
        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/api/v2/simulations")
                .set_json(body)
                .to_request(),
        )
        .await;
        let created: Value = actix_test::read_body_json(response).await;
        let other = match created.get("id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => panic!("the response must carry an id"),
        };

        let (_, left) = export!(app, baseline, "dataset=underlying&format=json");
        let (_, right) = export!(app, other, "dataset=underlying&format=json");
        assert_ne!(left, right, "a different seed must move the market path");

        let expiries = |body: &str| -> Vec<String> {
            json_rows_of(body)
                .iter()
                .filter_map(|row| row.get("expires_at").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect()
        };
        let (_, left_chains) = export!(app, baseline, "dataset=option_chains&format=json");
        let (_, right_chains) = export!(app, other, "dataset=option_chains&format=json");
        assert_eq!(
            expiries(&left_chains),
            expiries(&right_chains),
            "expirations come from the schedule, not the seed"
        );
    }

    /// An export changes nothing: not the cursor, not the state, not the
    /// revision, and not what the next peek returns.
    #[actix_web::test]
    async fn test_an_export_changes_nothing() {
        let app = v2_service!();
        let id = create!(app);

        let before: Value = {
            let response = actix_test::call_service(
                &app,
                actix_test::TestRequest::get()
                    .uri(&format!("/api/v2/simulations/{id}/snapshot"))
                    .to_request(),
            )
            .await;
            actix_test::read_body_json(response).await
        };

        let (status, _) = export!(app, id, "dataset=option_chains&format=json");
        assert_eq!(status, StatusCode::OK);

        let after: Value = {
            let response = actix_test::call_service(
                &app,
                actix_test::TestRequest::get()
                    .uri(&format!("/api/v2/simulations/{id}/snapshot"))
                    .to_request(),
            )
            .await;
            actix_test::read_body_json(response).await
        };

        assert_eq!(before, after, "an export must not disturb the simulation");
    }

    /// A simulation that has been walked to completion still exports its whole
    /// tape: the export replays from the parameters, not from the cursor.
    #[actix_web::test]
    async fn test_a_completed_simulation_still_exports_its_whole_tape() {
        let app = v2_service!();
        let id = create!(app);

        for _ in 0..3 {
            let response = actix_test::call_service(
                &app,
                actix_test::TestRequest::post()
                    .uri(&format!("/api/v2/simulations/{id}/step"))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        // The serving paths are exhausted...
        let peeked = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri(&format!("/api/v2/simulations/{id}/snapshot"))
                .to_request(),
        )
        .await;
        assert_eq!(peeked.status(), StatusCode::GONE);

        // ...but the export is not a serving path.
        let (status, body) = export!(app, id, "dataset=underlying&format=json");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_rows_of(&body).len(), 3);
    }

    /// An export of a simulation that was never walked covers the whole tape.
    #[actix_web::test]
    async fn test_an_unwalked_simulation_exports_from_step_zero() {
        let app = v2_service!();
        let id = create!(app);

        let (_, body) = export!(app, id, "dataset=underlying&format=json");
        let rows = json_rows_of(&body);

        assert_eq!(
            rows.first().and_then(|row| row.get("step")),
            Some(&json!(0))
        );
        assert_eq!(rows.len(), 3);
    }

    // ---- ranges ----------------------------------------------------------

    /// A range is inclusive on both ends.
    #[actix_web::test]
    async fn test_a_range_is_inclusive() {
        let app = v2_service!();
        let id = create!(app);

        let (_, body) = export!(
            app,
            id,
            "dataset=underlying&format=json&from_step=1&to_step=2"
        );
        let rows = json_rows_of(&body);

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.first().and_then(|row| row.get("step")),
            Some(&json!(1))
        );
        assert_eq!(rows.last().and_then(|row| row.get("step")), Some(&json!(2)));
    }

    /// A single-step range is one row, not zero and not an error.
    #[actix_web::test]
    async fn test_a_single_step_range_is_one_row() {
        let app = v2_service!();
        let id = create!(app);

        let (status, body) = export!(
            app,
            id,
            "dataset=underlying&format=json&from_step=1&to_step=1"
        );

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_rows_of(&body).len(), 1);
    }

    /// A reversed range is a 400 naming the field.
    #[actix_web::test]
    async fn test_a_reversed_range_is_rejected() {
        let app = v2_service!();
        let id = create!(app);

        let (status, body) = export!(
            app,
            id,
            "dataset=underlying&format=json&from_step=2&to_step=1"
        );

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("from_step"), "{body}");
    }

    /// A bound past the tape is a 400 naming which one.
    #[actix_web::test]
    async fn test_a_bound_past_the_tape_is_rejected() {
        let app = v2_service!();
        let id = create!(app);

        let (status, body) = export!(app, id, "dataset=underlying&format=json&to_step=99");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("to_step"), "{body}");

        let (status, body) = export!(app, id, "dataset=underlying&format=json&from_step=99");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("from_step"), "{body}");
    }

    // ---- errors ----------------------------------------------------------

    /// An unknown dataset or format is a 400 rather than an empty download.
    #[actix_web::test]
    async fn test_an_unknown_dataset_or_format_is_rejected() {
        let app = v2_service!();
        let id = create!(app);

        let (status, _) = export!(app, id, "dataset=greeks&format=json");
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = export!(app, id, "dataset=underlying&format=parquet");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// An unknown simulation is a 404.
    #[actix_web::test]
    async fn test_an_unknown_simulation_is_not_found() {
        let app = v2_service!();

        let (status, _) = export!(app, Uuid::new_v4(), "dataset=underlying&format=json");

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A malformed id is a 400 naming the field.
    #[actix_web::test]
    async fn test_a_malformed_id_is_rejected() {
        let app = v2_service!();

        let (status, body) = export!(app, "not-a-uuid", "dataset=underlying&format=json");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("\"field\":\"id\""), "{body}");
    }

    /// The response advertises its type and offers a filename.
    #[actix_web::test]
    async fn test_the_response_carries_its_content_type_and_filename() {
        let app = v2_service!();
        let id = create!(app);

        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri(&format!(
                    "/api/v2/simulations/{id}/export?dataset=option_chains&format=csv"
                ))
                .to_request(),
        )
        .await;

        let headers = response.headers();
        assert_eq!(
            headers
                .get(actix_web::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/csv; charset=utf-8")
        );
        let disposition = headers
            .get(actix_web::http::header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(disposition.contains("attachment"), "{disposition}");
        assert!(disposition.contains("option_chains"), "{disposition}");
        assert!(disposition.ends_with(".csv\""), "{disposition}");
    }

    // ---- CSV safety ------------------------------------------------------

    /// A symbol can never carry a CSV separator, because the request boundary
    /// rejects one.
    ///
    /// This is the corruption the `labels` pipe-join and the `rule_id` charset
    /// were designed against; the symbol is covered by the same identifier
    /// validation v1 uses, so the export inherits it rather than re-checking.
    #[actix_web::test]
    async fn test_a_symbol_cannot_carry_a_csv_separator() {
        let app = v2_service!();

        for symbol in ["SP,X", "SP\"X", "SP|X", "SP\nX"] {
            let mut body = reference_body();
            body["symbol"] = json!(symbol);

            let response = actix_test::call_service(
                &app,
                actix_test::TestRequest::post()
                    .uri("/api/v2/simulations")
                    .set_json(body)
                    .to_request(),
            )
            .await;

            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{symbol:?} must be rejected at the boundary"
            );
        }
    }

    // ---- the persisted source --------------------------------------------

    /// A warehouse that answers from memory, standing in for ClickHouse.
    ///
    /// Hermetic on purpose: what these tests are about is the *export's*
    /// decision between the two sources and the adapter that renders them, not
    /// the SQL — a live warehouse would test the driver and hide the branch.
    #[derive(Default)]
    struct FakeWarehouse {
        stored: std::sync::Mutex<std::collections::BTreeMap<usize, SnapshotRecord>>,
        /// When set, every read fails the way an unreachable warehouse does.
        failing: bool,
        /// How many range reads the export asked for — the windowing evidence.
        reads: std::sync::atomic::AtomicUsize,
    }

    impl FakeWarehouse {
        /// A warehouse that is down.
        fn failing() -> Self {
            Self {
                failing: true,
                ..Self::default()
            }
        }

        /// How many range reads it has answered.
        fn reads(&self) -> usize {
            self.reads.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// Files records after the fact.
        ///
        /// Lets a test create the simulation first and persist its real id
        /// afterwards, which is the order production works in.
        fn fill(&self, records: Vec<SnapshotRecord>) {
            let mut stored = match self.stored.lock() {
                Ok(stored) => stored,
                Err(poisoned) => poisoned.into_inner(),
            };
            for record in records {
                stored.insert(record.step, record);
            }
        }

        /// Forgets everything it holds, so a test can compare the replayed
        /// and the stored path more than once in a row.
        fn clear(&self) {
            match self.stored.lock() {
                Ok(mut stored) => stored.clear(),
                Err(poisoned) => poisoned.into_inner().clear(),
            }
        }

        /// The records it holds, ascending by step.
        fn range(&self, from: usize, to: usize) -> Vec<SnapshotRecord> {
            match self.stored.lock() {
                Ok(stored) => stored
                    .range(from..=to)
                    .map(|(_, record)| record.clone())
                    .collect(),
                Err(poisoned) => poisoned
                    .into_inner()
                    .range(from..=to)
                    .map(|(_, record)| record.clone())
                    .collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl SimulationSnapshotRepository for FakeWarehouse {
        /// No server behind it; reachable exactly as long as the process is.
        async fn ping(&self) -> Result<(), ChainError> {
            Ok(())
        }

        async fn persist(&self, record: SnapshotRecord) -> Result<(), ChainError> {
            match self.stored.lock() {
                Ok(mut stored) => {
                    stored.insert(record.step, record);
                }
                Err(poisoned) => {
                    poisoned.into_inner().insert(record.step, record);
                }
            }
            Ok(())
        }

        async fn get(
            &self,
            _simulation: Uuid,
            _generation: u64,
            step: usize,
        ) -> Result<Option<SnapshotRecord>, ChainError> {
            Ok(self.range(step, step).into_iter().next())
        }

        async fn read_range(
            &self,
            _simulation: Uuid,
            _generation: u64,
            from_step: usize,
            to_step: usize,
        ) -> Result<Vec<SnapshotRecord>, ChainError> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.failing {
                return Err(ChainError::ClickHouseError(
                    "the warehouse is unreachable".to_string(),
                ));
            }
            Ok(self.range(from_step, to_step))
        }

        async fn contract_series(
            &self,
            _query: crate::infrastructure::ContractSeriesQuery,
        ) -> Result<Vec<crate::infrastructure::ContractQuote>, ChainError> {
            // The export never projects a single contract; an empty history is
            // an honest answer rather than a panic waiting to be reached.
            Ok(Vec::new())
        }
    }

    /// The effective parameters of [`reference_body`].
    ///
    /// Parsed from the very same JSON the tests create with, so the replayed
    /// tape here cannot drift from the simulation under test.
    fn reference_parameters() -> SimulationParametersV2 {
        let request: crate::api::rest::requests_v2::CreateSimulationRequest =
            match serde_json::from_value(reference_body()) {
                Ok(request) => request,
                Err(error) => panic!("the reference body must deserialize: {error}"),
            };
        match SimulationParametersV2::try_from(request) {
            Ok(parameters) => parameters,
            Err(error) => panic!("the reference body must convert: {error}"),
        }
    }

    /// The snapshot record of `step`, as the session layer would have filed it.
    ///
    /// The conversion is re-stated here rather than reused because
    /// `session::snapshot_record` is a private module: the api layer cannot name
    /// it. That is a feature for this test — the fixture is built from the
    /// domain snapshot independently of the writer, so the assertion below is
    /// about the reader, not about a shared helper agreeing with itself.
    fn stored_record(simulation: Uuid, step: usize) -> SnapshotRecord {
        let parameters = reference_parameters();
        let tape = match FactorTape::build(&parameters, &parameters.method) {
            Ok(tape) => tape,
            Err(error) => panic!("the tape must build: {error}"),
        };
        // With the greek snapshots, exactly as the manager files them when a
        // warehouse is registered: a stored tape that lacked them would make
        // the level-aware comparisons below vacuous.
        let builder = match SeriesBuilder::new(&parameters, &tape) {
            Ok(builder) => builder.with_greek_snapshots(true),
            Err(error) => panic!("the builder must accept the parameters: {error}"),
        };
        let snapshot = match builder.snapshot(step) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("the snapshot must build: {error}"),
        };

        SnapshotRecord::new(
            simulation,
            CURRENT_SNAPSHOT_GENERATION,
            snapshot.step,
            snapshot.simulated_at,
            parameters.symbol.clone(),
            snapshot.spot,
            snapshot.base_volatility,
            snapshot
                .chains
                .iter()
                .map(|chain| {
                    crate::infrastructure::ExpirationRecord::new(
                        chain.expires_at,
                        chain.days_to_expiration,
                        chain.labels.clone(),
                        chain
                            .chain
                            .iter()
                            .map(|data| QuoteRow {
                                strike: data.strike_price,
                                implied_volatility: data.implied_volatility,
                                call_bid: data.call_bid,
                                call_ask: data.call_ask,
                                call_mid: data.call_middle,
                                put_bid: data.put_bid,
                                put_ask: data.put_ask,
                                put_mid: data.put_middle,
                                delta_call: data.delta_call,
                                delta_put: data.delta_put,
                                gamma: data.gamma,
                                greeks_call: data.greeks_call.clone(),
                                greeks_put: data.greeks_put.clone(),
                            })
                            .collect(),
                    )
                })
                .collect(),
        )
    }

    /// Every step of the reference simulation, as persisted records.
    fn stored_tape(simulation: Uuid) -> Vec<SnapshotRecord> {
        (0..3).map(|step| stored_record(simulation, step)).collect()
    }

    /// The id the export will use, parsed back from the create response.
    fn parse_id(id: &str) -> Uuid {
        match Uuid::parse_str(id) {
            Ok(id) => id,
            Err(error) => panic!("the created id must be a UUID: {error}"),
        }
    }

    /// A step served from the warehouse renders exactly like one replayed —
    /// byte for byte, in both encodings.
    ///
    /// This is what makes preferring the warehouse safe at all: the two sources
    /// go through one adapter, so a client cannot tell which produced its rows.
    #[actix_web::test]
    async fn test_a_persisted_step_renders_exactly_like_a_replayed_one() {
        let warehouse = Arc::new(FakeWarehouse::default());
        let app = v2_service!(Some(
            Arc::clone(&warehouse) as Arc<dyn SimulationSnapshotRepository>
        ));
        let id = create!(app);

        // The same app, the same simulation, exported either side of the moment
        // the warehouse learns the tape: the only thing that changes is which
        // source answers.
        let mut replayed = Vec::new();
        for format in ["json", "csv"] {
            let (status, body) = export!(app, id, format!("dataset=option_chains&format={format}"));
            assert_eq!(status, StatusCode::OK, "{format}");
            replayed.push(body);
        }

        warehouse.fill(stored_tape(parse_id(&id)));

        for (format, replayed) in ["json", "csv"].iter().zip(replayed) {
            let (status, stored) =
                export!(app, id, format!("dataset=option_chains&format={format}"));

            assert_eq!(status, StatusCode::OK, "{format}");
            assert_eq!(
                replayed, stored,
                "{format}: a persisted step must render identically to a replayed one"
            );
        }
        assert!(
            warehouse.reads() >= 4,
            "every chains export must have consulted the warehouse"
        );
    }

    /// One step is delivered as several bounded chunks, not one.
    ///
    /// The channel bounds how many chunks are in flight, not how large they
    /// are, so a whole step in one chunk let a slow client hold the entire step
    /// in memory — hundreds of megabytes at the per-snapshot cap with
    /// `greeks=all`. Backpressure has to apply WITHIN a step, which it only
    /// does if a step is more than one chunk.
    ///
    /// Checked on both encodings, since they buffer differently: JSON
    /// accumulates encoded rows, CSV encodes batches of records.
    #[test]
    fn test_a_wide_step_is_delivered_in_bounded_chunks() {
        let mut parameters = reference_parameters();
        // Wide enough that one step is several chunks.
        parameters.chain_size = Some(400);
        parameters.strike_interval =
            match positive::Positive::new_decimal(rust_decimal::Decimal::ONE) {
                Ok(interval) => Some(interval),
                Err(error) => panic!("the fixture interval must be positive: {error}"),
            };

        let tape = match FactorTape::build(&parameters, &parameters.method) {
            Ok(tape) => tape,
            Err(error) => panic!("the tape must build: {error}"),
        };
        let builder = match SeriesBuilder::new(&parameters, &tape) {
            Ok(builder) => builder,
            Err(error) => panic!("the builder must accept the parameters: {error}"),
        };
        let snapshot = match builder.snapshot(0) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("the snapshot must build: {error}"),
        };
        let row = match tape.row(0) {
            Some(row) => row,
            None => panic!("the tape must have a first row"),
        };

        let rows: usize = snapshot
            .chains
            .iter()
            .map(|chain| chain.chain.iter().count())
            .sum();
        assert!(
            rows > ROWS_PER_CHUNK,
            "the fixture must be wider than one chunk, got {rows} rows"
        );

        for format in [Format::Json, Format::Csv] {
            let mut writer = match Writer::new(
                format,
                Dataset::OptionChains,
                GreekLevel::All,
                &parameters,
                *EXPORT_BLOCK_ROWS,
            ) {
                Ok(writer) => writer,
                Err(error) => panic!("the writer must build: {error}"),
            };
            let mut chunks: Vec<usize> = Vec::new();
            let delivery = writer.rows(
                &parameters,
                0,
                row,
                Some(StepChains::Replayed(&snapshot)),
                &mut |chunk| {
                    chunks.push(chunk.len());
                    Delivery::Sent
                },
            );
            match delivery {
                Ok(Delivery::Sent) => {}
                other => panic!("{format:?}: the step must be delivered, got {other:?}"),
            }

            assert!(
                chunks.len() > 1,
                "{format:?}: a step of {rows} rows must be more than one chunk"
            );
            // The bound is rows, and a row's width is fixed by the dataset and
            // the level, so this is a byte bound in practice.
            let widest = chunks.iter().copied().max().unwrap_or_default();
            assert!(
                widest < 4 * 1024 * 1024,
                "{format:?}: a chunk of {widest} bytes is not bounded"
            );
        }
    }

    /// A client that hangs up stops the work at the next chunk boundary.
    ///
    /// The reason `rows` reports delivery rather than returning bytes: it now
    /// emits several chunks per step, so it has to learn about the disconnect
    /// itself instead of the caller discovering it after the whole step was
    /// built.
    #[test]
    fn test_a_disconnect_stops_the_step_at_the_next_chunk() {
        let mut parameters = reference_parameters();
        parameters.chain_size = Some(400);
        parameters.strike_interval =
            match positive::Positive::new_decimal(rust_decimal::Decimal::ONE) {
                Ok(interval) => Some(interval),
                Err(error) => panic!("the fixture interval must be positive: {error}"),
            };

        let tape = match FactorTape::build(&parameters, &parameters.method) {
            Ok(tape) => tape,
            Err(error) => panic!("the tape must build: {error}"),
        };
        let builder = match SeriesBuilder::new(&parameters, &tape) {
            Ok(builder) => builder,
            Err(error) => panic!("the builder must accept the parameters: {error}"),
        };
        let snapshot = match builder.snapshot(0) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("the snapshot must build: {error}"),
        };
        let row = match tape.row(0) {
            Some(row) => row,
            None => panic!("the tape must have a first row"),
        };

        let mut writer = match Writer::new(
            Format::Csv,
            Dataset::OptionChains,
            GreekLevel::All,
            &parameters,
            *EXPORT_BLOCK_ROWS,
        ) {
            Ok(writer) => writer,
            Err(error) => panic!("the writer must build: {error}"),
        };
        let mut delivered = 0_usize;
        let outcome = writer.rows(
            &parameters,
            0,
            row,
            Some(StepChains::Replayed(&snapshot)),
            &mut |_| {
                delivered += 1;
                Delivery::ClientGone
            },
        );

        match outcome {
            Ok(Delivery::ClientGone) => {}
            other => panic!("a disconnect must be reported, got {other:?}"),
        }
        assert_eq!(delivered, 1, "the work must stop at the first refusal");
    }

    /// Every way of getting a 400 out of this endpoint returns the documented
    /// shape.
    ///
    /// Four different paths reach it — the handler's own validation, the id
    /// parse, the range check and actix's query extractor — and only the first
    /// three ever named a field. The extractor's rejection used to be untyped
    /// plaintext; it now renders the same object with an empty `field`, because
    /// serde's message for a bad key does not say which one. Pinned so the
    /// documented schema stays true of all four.
    #[actix_web::test]
    async fn test_every_export_rejection_carries_the_documented_shape() {
        let app = v2_service!();
        let id = create!(app);

        let cases = [
            (
                "/api/v2/simulations/not-a-uuid/export?dataset=option_chains&format=csv"
                    .to_string(),
                Some("id"),
            ),
            (
                format!(
                    "/api/v2/simulations/{id}/export?dataset=option_chains&format=csv&from_step=2&to_step=1"
                ),
                Some("from_step"),
            ),
            (
                format!(
                    "/api/v2/simulations/{id}/export?dataset=option_chains&format=csv&greeks=second"
                ),
                Some("greeks"),
            ),
            // The extractor's own rejections: typed body, unnamed field.
            (
                format!("/api/v2/simulations/{id}/export?dataset=nope&format=csv"),
                Some(""),
            ),
            (
                format!(
                    "/api/v2/simulations/{id}/export?dataset=option_chains&format=csv&from_step=abc"
                ),
                Some(""),
            ),
        ];

        for (uri, field) in cases {
            let response = actix_test::call_service(
                &app,
                actix_test::TestRequest::get().uri(&uri).to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "for {uri}");
            let body: Value = actix_test::read_body_json(response).await;
            assert_eq!(
                body.get("field").and_then(Value::as_str),
                field,
                "for {uri}: {body}"
            );
            assert!(
                body.get("error")
                    .and_then(Value::as_str)
                    .is_some_and(|error| !error.is_empty()),
                "every rejection must explain itself: {body}"
            );
        }
    }

    /// The two conversion paths agree at every greek level.
    ///
    /// The acceptance criterion of issue #75, and the one this stack's design
    /// exists to satisfy: `QuoteView::replayed` and `QuoteView::stored` are
    /// different code reading different sources, and the export prefers the
    /// warehouse whenever it has a step. If they disagreed about a greek, the
    /// same simulation would export differently depending on whether
    /// persistence happened to be registered — the asymmetry #74 and #75
    /// together remove.
    ///
    /// Checked in both encodings, because they are also two separate
    /// renderings of the same view.
    #[actix_web::test]
    async fn test_the_two_sources_agree_at_every_greek_level() {
        let warehouse = Arc::new(FakeWarehouse::default());
        let app = v2_service!(Some(
            Arc::clone(&warehouse) as Arc<dyn SimulationSnapshotRepository>
        ));
        let id = create!(app);

        for level in ["none", "first", "all"] {
            let mut replayed = Vec::new();
            for format in ["json", "csv"] {
                let (status, body) = export!(
                    app,
                    id,
                    format!("dataset=option_chains&format={format}&greeks={level}")
                );
                assert_eq!(status, StatusCode::OK, "{format} at {level}");
                replayed.push(body);
            }

            warehouse.fill(stored_tape(parse_id(&id)));

            for (format, replayed) in ["json", "csv"].iter().zip(replayed) {
                let (status, stored) = export!(
                    app,
                    id,
                    format!("dataset=option_chains&format={format}&greeks={level}")
                );
                assert_eq!(status, StatusCode::OK, "{format} at {level}");
                assert_eq!(
                    replayed, stored,
                    "{format} at greeks={level}: the stored and replayed paths must agree"
                );
            }

            warehouse.clear();
        }
    }

    /// The default header is frozen, literally.
    ///
    /// `test_the_default_export_is_unchanged` compares the default against
    /// `greeks=none`, which cannot fail if BOTH moved. This spells the
    /// pre-change header out by hand, so an edit to `CHAIN_COLUMNS` has to
    /// break a test rather than a consumer parsing by position.
    #[test]
    fn test_the_default_chain_header_is_frozen() {
        assert_eq!(
            Dataset::OptionChains.header(GreekLevel::None),
            vec![
                "step",
                "simulated_at",
                "symbol",
                "expires_at",
                "labels",
                "days_to_expiration",
                "strike",
                "implied_volatility",
                "call_bid",
                "call_ask",
                "call_mid",
                "call_delta",
                "put_bid",
                "put_ask",
                "put_mid",
                "put_delta",
                "gamma",
            ],
            "the default export's columns are frozen; levels may only append"
        );
    }

    /// A warehouse holding pre-#74 rows does not produce a half-covered export.
    ///
    /// The failure this prevents: a range whose early steps were filed before
    /// the greek columns existed and whose later steps were not would emit
    /// empty greek columns for the prefix and real ones for the rest, in one
    /// file, with nothing to tell them apart. Such a step is replayed instead.
    ///
    /// At the default level the same record is still served from storage,
    /// because there it answers the question completely.
    #[actix_web::test]
    async fn test_a_stored_step_without_greeks_is_replayed_rather_than_served_short() {
        let warehouse = Arc::new(FakeWarehouse::default());
        let app = v2_service!(Some(
            Arc::clone(&warehouse) as Arc<dyn SimulationSnapshotRepository>
        ));
        let id = create!(app);

        // A tape as a pre-#74 binary filed it: every value but the greeks.
        let mut old_shape = stored_tape(parse_id(&id));
        for record in &mut old_shape {
            for expiration in &mut record.expirations {
                for quote in &mut expiration.quotes {
                    quote.greeks_call = None;
                    quote.greeks_put = None;
                }
            }
        }
        warehouse.fill(old_shape);

        let (status, all) = export!(
            app,
            id,
            "dataset=option_chains&format=csv&greeks=all".to_string()
        );
        assert_eq!(status, StatusCode::OK);

        // Replayed, so every row carries its greeks rather than empty columns.
        let header = Dataset::OptionChains.header(GreekLevel::All);
        let charm = match header.iter().position(|column| *column == "call_charm") {
            Some(index) => index,
            None => panic!("the all-level header must carry call_charm"),
        };
        let lines: Vec<&str> = all.split("\r\n").filter(|line| !line.is_empty()).collect();
        assert!(lines.len() > 1, "the export must carry rows");
        for line in &lines[1..] {
            let fields: Vec<&str> = line.split(',').collect();
            assert!(
                fields.get(charm).is_some_and(|value| !value.is_empty()),
                "a replayed row must carry its greeks: {line}"
            );
        }

        // And the same record still answers the default level from storage.
        let (_, default) = export!(
            app,
            id,
            "dataset=option_chains&format=csv&greeks=none".to_string()
        );
        assert_eq!(status, StatusCode::OK);
        assert!(default.lines().count() > 1);
    }

    /// The default export is byte-identical to `greeks=none`, and carries no
    /// greek column at all.
    ///
    /// The regression that protects every existing consumer of a tape: a
    /// backtester parsing by column position must read exactly what it read
    /// before the parameter existed.
    #[actix_web::test]
    async fn test_the_default_export_is_unchanged() {
        let app = v2_service!();
        let id = create!(app);

        for format in ["json", "csv"] {
            let (status, default) =
                export!(app, id, format!("dataset=option_chains&format={format}"));
            assert_eq!(status, StatusCode::OK);
            let (_, explicit) = export!(
                app,
                id,
                format!("dataset=option_chains&format={format}&greeks=none")
            );
            assert_eq!(default, explicit, "{format}");

            for absent in ["call_theta", "put_charm", "call_gamma", "put_color"] {
                assert!(
                    !default.contains(absent),
                    "{format}: the default export must not carry {absent}"
                );
            }
        }
    }

    /// Exporting twice at the same level is byte-identical.
    ///
    /// The export's standing guarantee, extended to the new parameter: a level
    /// must not introduce anything order-dependent or non-deterministic.
    #[actix_web::test]
    async fn test_an_export_repeats_byte_for_byte_at_every_level() {
        let app = v2_service!();
        let id = create!(app);

        for level in ["none", "first", "all"] {
            for format in ["json", "csv"] {
                let query = format!("dataset=option_chains&format={format}&greeks={level}");
                let (status, first) = export!(app, id, query.clone());
                let (_, second) = export!(app, id, query);
                assert_eq!(status, StatusCode::OK);
                assert_eq!(first, second, "{format} at greeks={level}");
            }
        }
    }

    /// The JSON export carries the same values as the CSV export, at every
    /// level.
    ///
    /// Compared as RAW TEXT on both sides, never parsed. `serde_json`'s float
    /// parser is not bit-exact for every value this export produces — parsing
    /// `0.009547174464993615` and re-rendering it yields
    /// `0.009547174464993617` — so a test that parsed the JSON would be
    /// comparing its own round-trip error against the service and reporting a
    /// divergence that is not there. The bytes the client receives are
    /// identical, and those are what this checks.
    #[actix_web::test]
    async fn test_the_json_export_matches_the_csv_export_at_every_level() {
        /// The raw text of one `"key":value` token, exactly as serialised.
        fn raw_token(row: &str, key: &str) -> String {
            let needle = format!("\"{key}\":");
            let start = match row.find(&needle) {
                Some(start) => start + needle.len(),
                None => panic!("the JSON row must carry {key}: {row}"),
            };
            let rest = &row[start..];
            let end = rest.find([',', '}']).unwrap_or(rest.len());
            rest[..end].trim_matches('"').to_string()
        }

        let app = v2_service!();
        let id = create!(app);

        for (level, greek_level) in [
            ("none", GreekLevel::None),
            ("first", GreekLevel::First),
            ("all", GreekLevel::All),
        ] {
            let (_, csv) = export!(
                app,
                id,
                format!("dataset=option_chains&format=csv&greeks={level}")
            );
            let (_, json) = export!(
                app,
                id,
                format!("dataset=option_chains&format=json&greeks={level}")
            );

            let header = Dataset::OptionChains.header(greek_level);
            let lines: Vec<&str> = csv.split("\r\n").filter(|line| !line.is_empty()).collect();
            // Split the array into its objects without parsing them.
            let objects: Vec<&str> = json
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split("},{")
                .collect();

            assert_eq!(
                lines.len() - 1,
                objects.len(),
                "row counts differ at {level}"
            );
            assert!(!objects.is_empty(), "the export must carry rows at {level}");

            for (line, object) in lines[1..].iter().zip(objects.iter()) {
                let fields: Vec<&str> = line.split(',').collect();
                assert_eq!(fields.len(), header.len(), "width differs at {level}");
                for (column, field) in header.iter().zip(fields.iter()) {
                    let mut from_json = raw_token(object, column);
                    // A CSV blank and a JSON null are the same absence.
                    if from_json == "null" {
                        from_json = String::new();
                    }
                    let expected = field.trim_matches('"');

                    // Numbers are compared as NUMBERS, through one parser. The
                    // two encoders render the same `f64` differently — JSON
                    // takes exponent notation below `1e-5` where the CSV spells
                    // the zeros out, and `4975` is written `4975.0` — so a text
                    // comparison would report a difference in notation as a
                    // difference in value. Parsing both sides with the same
                    // correctly-rounded parser compares what the criterion is
                    // about, and still catches a genuine divergence.
                    match (from_json.parse::<f64>(), expected.parse::<f64>()) {
                        (Ok(json_value), Ok(csv_value)) => assert_eq!(
                            json_value, csv_value,
                            "{column} differs between the encodings at {level}"
                        ),
                        _ => assert_eq!(
                            from_json, expected,
                            "{column} differs between the encodings at {level}"
                        ),
                    }
                }
            }
        }
    }

    /// An unknown level is a typed 400, before a single byte is streamed.
    ///
    /// A stream that has already sent its header cannot take an error back, so
    /// the rejection has to happen on the runtime rather than in the producer.
    #[actix_web::test]
    async fn test_an_unknown_greek_level_is_rejected_before_streaming() {
        let app = v2_service!();
        let id = create!(app);

        let (status, body) = export!(
            app,
            id,
            "dataset=option_chains&format=csv&greeks=second".to_string()
        );

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("greeks"),
            "the 400 must name the field, got {body}"
        );
        assert!(
            !body.contains("call_bid"),
            "no header may have been streamed, got {body}"
        );
    }

    /// The persisted snapshot is what the export serves, not a replay of it.
    ///
    /// Without this the test above would pass on an export that ignored the
    /// warehouse entirely, since both sources agree by construction. A stored
    /// row carrying a value replay could never produce is the only way to see
    /// which side answered.
    #[actix_web::test]
    async fn test_the_export_prefers_the_persisted_snapshot() {
        let warehouse = Arc::new(FakeWarehouse::default());
        let app = v2_service!(Some(
            Arc::clone(&warehouse) as Arc<dyn SimulationSnapshotRepository>
        ));
        let id = create!(app);

        let (_, replayed) = export!(app, id, "dataset=option_chains&format=json");
        assert!(
            !replayed.contains("1234.5"),
            "the marker must be impossible to reach by replay"
        );

        let mut records = stored_tape(parse_id(&id));
        for record in &mut records {
            for expiration in &mut record.expirations {
                for quote in &mut expiration.quotes {
                    quote.call_bid = Some(positive::pos_or_panic!(1_234.5));
                }
            }
        }
        warehouse.fill(records);

        let (status, stored) = export!(app, id, "dataset=option_chains&format=json");

        assert_eq!(status, StatusCode::OK);
        let rows = json_rows_of(&stored);
        assert!(!rows.is_empty());
        for row in &rows {
            assert_eq!(
                row.get("call_bid"),
                Some(&json!(1234.5)),
                "every chains row must come from the warehouse: {row}"
            );
        }
    }

    /// A step the warehouse does not hold is replayed, and the export is
    /// indistinguishable from a full replay.
    #[actix_web::test]
    async fn test_a_missing_step_falls_back_to_replay() {
        let warehouse = Arc::new(FakeWarehouse::default());
        let app = v2_service!(Some(
            Arc::clone(&warehouse) as Arc<dyn SimulationSnapshotRepository>
        ));
        let id = create!(app);

        let mut replayed = Vec::new();
        for format in ["json", "csv"] {
            let (_, body) = export!(app, id, format!("dataset=option_chains&format={format}"));
            replayed.push(body);
        }

        // Only the middle step is persisted; the ends are gaps.
        warehouse.fill(vec![stored_record(parse_id(&id), 1)]);

        for (format, replayed) in ["json", "csv"].iter().zip(replayed) {
            let (status, mixed) =
                export!(app, id, format!("dataset=option_chains&format={format}"));

            assert_eq!(status, StatusCode::OK, "{format}");
            assert_eq!(
                replayed, mixed,
                "{format}: a partially persisted tape must export the whole range"
            );
        }
    }

    /// A warehouse that is down does not fail the export, and does not change
    /// what it produces.
    #[actix_web::test]
    async fn test_a_failing_warehouse_does_not_fail_the_export() {
        let replaying = v2_service!();
        let id = create!(replaying);

        // A different app, hence a different simulation id — but the same
        // parameters and the same seed, which is all a tape depends on.
        let warehouse = Arc::new(FakeWarehouse::failing());
        let storing = v2_service!(Some(
            Arc::clone(&warehouse) as Arc<dyn SimulationSnapshotRepository>
        ));
        let stored_id = create!(storing);

        let (_, replayed) = export!(replaying, id, "dataset=option_chains&format=csv");
        let (status, degraded) = export!(storing, stored_id, "dataset=option_chains&format=csv");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(replayed, degraded, "a failed read must fall back to replay");
        assert_eq!(
            warehouse.reads(),
            1,
            "a warehouse that failed once must not be asked again for this export"
        );
    }

    /// Only the chains dataset consults the warehouse, and it consults it once
    /// per window rather than once per step.
    #[actix_web::test]
    async fn test_only_the_chains_dataset_reads_the_warehouse() {
        let warehouse = Arc::new(FakeWarehouse::default());
        let app = v2_service!(Some(
            Arc::clone(&warehouse) as Arc<dyn SimulationSnapshotRepository>
        ));
        let id = create!(app);

        for dataset in ["underlying", "volatility"] {
            let (status, _) = export!(app, id, format!("dataset={dataset}&format=json"));
            assert_eq!(status, StatusCode::OK);
        }
        assert_eq!(
            warehouse.reads(),
            0,
            "the tape-only datasets must not pay for a warehouse lookup"
        );

        let (status, _) = export!(app, id, "dataset=option_chains&format=json");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            warehouse.reads(),
            1,
            "three steps fit in one window, so one read serves them all"
        );
    }

    /// A window covers at most its width, and never reaches past the range.
    #[test]
    fn test_a_window_is_bounded_by_its_width_and_by_the_range() {
        assert_eq!(window_end(0, SNAPSHOT_WINDOW_STEPS, 1_000), 63);
        assert_eq!(window_end(64, SNAPSHOT_WINDOW_STEPS, 1_000), 127);
        assert_eq!(
            window_end(0, SNAPSHOT_WINDOW_STEPS, 10),
            10,
            "a short range must not be read past its end"
        );
        assert_eq!(window_end(7, 1, 1_000), 7, "a narrowed window is one step");
        assert_eq!(
            window_end(usize::MAX, SNAPSHOT_WINDOW_STEPS, usize::MAX),
            usize::MAX,
            "the arithmetic must not wrap into a reversed range"
        );
    }

    // ---- ranges and bounds, unit level -----------------------------------

    fn query(from: Option<usize>, to: Option<usize>) -> ExportQuery {
        ExportQuery {
            dataset: Dataset::Underlying,
            format: Format::Json,
            from_step: from,
            to_step: to,
            greeks: None,
        }
    }

    /// Omitted bounds cover the whole tape.
    #[test]
    fn test_omitted_bounds_cover_the_whole_tape() {
        match StepRange::resolve(&query(None, None), 10, 1_000) {
            Ok(range) => {
                assert_eq!(range.from, 0);
                assert_eq!(range.to, 9);
                assert_eq!(range.steps().count(), 10);
            }
            Err(error) => panic!("the default range must resolve: {error}"),
        }
    }

    /// A range larger than the configured cap is refused, so an unbounded
    /// request cannot turn into minutes of pricing.
    #[test]
    fn test_a_range_beyond_the_cap_is_refused() {
        match StepRange::resolve(&query(None, None), 10_000, 100) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "to_step");
                assert!(reason.contains("the service will export"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A range exactly at the cap is allowed.
    #[test]
    fn test_a_range_exactly_at_the_cap_is_allowed() {
        assert!(StepRange::resolve(&query(None, None), 100, 100).is_ok());
    }

    /// An absent optional renders as an empty CSV field, not `null` and not
    /// zero.
    #[test]
    fn test_an_absent_optional_renders_empty() {
        assert_eq!(render_optional(None), "");
        assert_eq!(render_optional(Some(0.0)), "0");
        assert_eq!(render_optional(Some(1.5)), "1.5");
    }

    /// Every dataset's header matches the columns its rows carry, at every
    /// greek level.
    #[test]
    fn test_every_header_matches_its_row_width() {
        for level in [GreekLevel::None, GreekLevel::First, GreekLevel::All] {
            for (dataset, width) in [
                (Dataset::Underlying, 4),
                (Dataset::Volatility, 4),
                (
                    Dataset::OptionChains,
                    match level {
                        // 17 today, plus four first-order greeks per style,
                        // plus seven more per style at `all`.
                        GreekLevel::None => 17,
                        GreekLevel::First => 17 + 8,
                        GreekLevel::All => 17 + 8 + 14,
                    },
                ),
            ] {
                assert_eq!(
                    dataset.header(level).len(),
                    width,
                    "{dataset:?} at {level:?}"
                );
            }
        }
    }

    /// Each level's header is a PREFIX of the next.
    ///
    /// What lets a consumer parse by position: raising the level appends and
    /// never moves a column, so a parser written against `none` keeps reading
    /// the same fields out of an `all` export.
    #[test]
    fn test_each_greek_level_extends_the_previous_header() {
        let none = Dataset::OptionChains.header(GreekLevel::None);
        let first = Dataset::OptionChains.header(GreekLevel::First);
        let all = Dataset::OptionChains.header(GreekLevel::All);

        assert_eq!(first[..none.len()], none[..]);
        assert_eq!(all[..first.len()], first[..]);

        // And the appended names are the documented ones.
        assert_eq!(
            &first[none.len()..],
            &[
                "call_theta",
                "put_theta",
                "call_vega",
                "put_vega",
                "call_rho",
                "put_rho",
                "call_rho_d",
                "put_rho_d"
            ]
        );
        assert_eq!(all[first.len()], "call_gamma");
        assert_eq!(all[all.len() - 1], "put_color");
    }

    /// Only the option-chains dataset needs chains priced, which is why a
    /// multi-year underlying export is nearly free.
    #[test]
    fn test_only_the_chains_dataset_prices_anything() {
        assert!(!Dataset::Underlying.needs_chains());
        assert!(!Dataset::Volatility.needs_chains());
        assert!(Dataset::OptionChains.needs_chains());
    }
}
