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
//! per-quote view, so a row is byte-identical whichever side produced it. The
//! factor row still comes from the tape either way: the underlying and
//! volatility datasets are built from it, and it is cheap next to a chain.
//!
//! # Determinism
//!
//! Two exports of the same simulation are byte-identical. Every value is a
//! function of the effective parameters and the cursor, timestamps render as
//! whole-second RFC 3339, and numbers use Rust's shortest round-trip
//! formatting — no locale, no thousands separators. That is what lets a
//! backtest harness cache a download and know it is still current.

use crate::api::rest::error::map_error;
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
}

impl Format {
    /// The `Content-Type` the response advertises.
    #[must_use]
    fn content_type(self) -> &'static str {
        match self {
            Format::Json => "application/json",
            Format::Csv => "text/csv; charset=utf-8",
        }
    }

    /// The extension of the suggested download filename.
    #[must_use]
    fn extension(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Csv => "csv",
        }
    }
}

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
    #[must_use]
    fn header(self) -> &'static [&'static str] {
        match self {
            Dataset::Underlying => &["step", "simulated_at", "symbol", "price"],
            Dataset::Volatility => &["step", "simulated_at", "symbol", "base_volatility"],
            Dataset::OptionChains => &[
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
    pub(crate) dataset: Dataset,
    /// Which encoding to return.
    pub(crate) format: Format,
    /// First step to include, inclusive. Defaults to `0`.
    #[serde(default)]
    pub(crate) from_step: Option<usize>,
    /// Last step to include, inclusive. Defaults to the final generated step.
    #[serde(default)]
    pub(crate) to_step: Option<usize>,
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

/// One strike of one expiration, flattened to exactly what a row carries.
///
/// The common view both sources are reduced to. Every conversion from a source
/// value to the wire's `f64` happens here and only here, which is what makes a
/// persisted row and a replayed row byte-identical rather than merely similar.
#[derive(Debug, Clone, Copy, PartialEq)]
struct QuoteView {
    strike: f64,
    implied_volatility: f64,
    call_bid: Option<f64>,
    call_ask: Option<f64>,
    call_mid: Option<f64>,
    call_delta: Option<f64>,
    put_bid: Option<f64>,
    put_ask: Option<f64>,
    put_mid: Option<f64>,
    put_delta: Option<f64>,
    gamma: Option<f64>,
}

impl QuoteView {
    /// Views a strike that was just priced.
    #[must_use]
    fn replayed(data: &OptionData) -> Self {
        Self {
            strike: data.strike_price.to_f64(),
            implied_volatility: data.implied_volatility.to_f64(),
            call_bid: data.call_bid.map(|value| value.to_f64()),
            call_ask: data.call_ask.map(|value| value.to_f64()),
            call_mid: data.call_middle.map(|value| value.to_f64()),
            call_delta: data.delta_call.and_then(decimal_to_f64),
            put_bid: data.put_bid.map(|value| value.to_f64()),
            put_ask: data.put_ask.map(|value| value.to_f64()),
            put_mid: data.put_middle.map(|value| value.to_f64()),
            put_delta: data.delta_put.and_then(decimal_to_f64),
            gamma: data.gamma.and_then(decimal_to_f64),
        }
    }

    /// Views a strike that was read back from the warehouse.
    #[must_use]
    fn stored(row: &QuoteRow) -> Self {
        Self {
            strike: row.strike.to_f64(),
            implied_volatility: row.implied_volatility.to_f64(),
            call_bid: row.call_bid.map(|value| value.to_f64()),
            call_ask: row.call_ask.map(|value| value.to_f64()),
            call_mid: row.call_mid.map(|value| value.to_f64()),
            call_delta: row.delta_call.and_then(decimal_to_f64),
            put_bid: row.put_bid.map(|value| value.to_f64()),
            put_ask: row.put_ask.map(|value| value.to_f64()),
            put_mid: row.put_mid.map(|value| value.to_f64()),
            put_delta: row.delta_put.and_then(decimal_to_f64),
            gamma: row.gamma.and_then(decimal_to_f64),
        }
    }
}

/// The strikes of one expiration, from whichever source produced them.
#[derive(Debug, Clone, Copy)]
enum QuoteSource<'a> {
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
    fn quotes(self) -> impl Iterator<Item = QuoteView> + 'a {
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
struct ExpirationView<'a> {
    expires_at: DateTime<Utc>,
    days_to_expiration: f64,
    labels: &'a [String],
    quotes: QuoteSource<'a>,
}

/// The chains of one step, from whichever source produced them.
///
/// The adapter the whole preference rests on: a stored snapshot and a replayed
/// one are the same simulated market, so they must render the same rows. Giving
/// the two sources one view — instead of two row builders that happen to agree
/// today — is what makes preferring the warehouse safe.
#[derive(Debug, Clone, Copy)]
enum StepChains<'a> {
    /// Priced here and now, from the effective parameters.
    Replayed(&'a SeriesSnapshot),
    /// Read back from the warehouse exactly as it was served.
    Stored(&'a SnapshotRecord),
}

impl<'a> StepChains<'a> {
    /// The live expirations, ascending — the order both sources guarantee.
    fn expirations(self) -> impl Iterator<Item = ExpirationView<'a>> {
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
                days_to_expiration: chain.days_to_expiration.to_f64(),
                labels: &chain.labels,
                quotes: QuoteSource::Replayed(&chain.chain),
            })
            .chain(
                stored
                    .into_iter()
                    .flatten()
                    .map(|expiration| ExpirationView {
                        expires_at: expiration.expires_at,
                        days_to_expiration: expiration.days_to_expiration.to_f64(),
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

#[utoipa::path(
    get,
    path = "/api/v2/simulations/{id}/export",
    description = "Export a simulation's complete tape, or a step range of it, as JSON or CSV. \
        Read-only: it replays from an immutable snapshot of the effective parameters and never \
        advances the cursor, changes the state or version, or alters what the next peek returns. \
        A simulation that has not been walked at all exports its whole tape. Where snapshot \
        persistence is enabled, an option_chains export serves the steps the warehouse holds from \
        it and replays the rest; the rows are identical either way. JSON is a single array of row \
        objects; CSV is RFC 4180 with a header row and CRLF line endings. Repeating the same \
        export yields byte-identical output.",
    params(
        ("id" = String, Path, description = "The simulation's identifier"),
        ("dataset" = String, Query, description = "underlying | volatility | option_chains"),
        ("format" = String, Query, description = "json | csv"),
        ("from_step" = Option<usize>, Query, description = "First step, inclusive; defaults to 0"),
        ("to_step" = Option<usize>, Query, description = "Last step, inclusive; defaults to the final step")
    ),
    responses(
        (status = 200, description = "The exported rows, streamed", body = String),
        (status = 400, description = "Unknown dataset or format, or an invalid range; body carries `error` and `field`"),
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
        if let Err(error) = produce(&parameters, dataset, format, range, stored, &sender) {
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
    range: StepRange,
    mut stored: Option<StoredSteps>,
    sender: &mpsc::Sender<Result<Vec<u8>, ChainError>>,
) -> Result<(), ChainError> {
    let tape = FactorTape::build(parameters, &parameters.method)?;
    let builder = if dataset.needs_chains() {
        Some(SeriesBuilder::new(parameters, &tape)?)
    } else {
        None
    };

    let mut writer = Writer::new(format, dataset)?;
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
        let record = match &mut stored {
            Some(stored) => stored.take(step, range.to),
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

        let chunk = writer.rows(parameters, row.step, row, chains)?;
        if !chunk.is_empty() && sender.blocking_send(Ok(chunk)).is_err() {
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
    Json { dataset: Dataset, first: bool },
    /// RFC 4180 CSV. A writer is built per chunk rather than kept: `csv::Writer`
    /// only surrenders its buffer by consuming itself, and constructing one is
    /// cheap next to pricing a chain.
    Csv { dataset: Dataset },
}

impl Writer {
    /// Creates a writer for a dataset and format.
    fn new(format: Format, dataset: Dataset) -> Result<Self, ChainError> {
        Ok(match format {
            Format::Json => Writer::Json {
                dataset,
                first: true,
            },
            Format::Csv => Writer::Csv { dataset },
        })
    }

    /// The bytes that open the document, if any.
    fn prologue(&mut self) -> Result<Option<Vec<u8>>, ChainError> {
        match self {
            Writer::Json { .. } => Ok(Some(b"[".to_vec())),
            Writer::Csv { dataset } => {
                let header: Vec<String> =
                    dataset.header().iter().map(ToString::to_string).collect();
                Ok(Some(encode_csv(&[header])?))
            }
        }
    }

    /// The bytes that close the document, if any.
    fn epilogue(&mut self) -> Result<Option<Vec<u8>>, ChainError> {
        match self {
            Writer::Json { .. } => Ok(Some(b"]".to_vec())),
            Writer::Csv { .. } => Ok(None),
        }
    }

    /// Encodes every row one step contributes.
    ///
    /// `simulated_at` comes from the factor row whichever source produced the
    /// chains: it is the tape's instant, the same one a stored record was
    /// written from, and taking it from one place keeps the two sources
    /// rendering identically by construction.
    fn rows(
        &mut self,
        parameters: &SimulationParametersV2,
        step: usize,
        row: &crate::domain::factors::FactorRow,
        chains: Option<StepChains<'_>>,
    ) -> Result<Vec<u8>, ChainError> {
        let simulated_at = render_instant(row.simulated_at);
        let symbol = parameters.symbol.as_str();

        match self {
            Writer::Json { dataset, first } => {
                let values = json_rows(*dataset, step, &simulated_at, symbol, row, chains);
                let mut chunk = Vec::new();
                for value in values {
                    if !*first {
                        chunk.push(b',');
                    }
                    *first = false;
                    let encoded = serde_json::to_vec(&value).map_err(|e| {
                        ChainError::Internal(format!("failed to encode an export row: {e}"))
                    })?;
                    chunk.extend_from_slice(&encoded);
                }
                Ok(chunk)
            }
            Writer::Csv { dataset } => {
                let records = csv_rows(*dataset, step, &simulated_at, symbol, row, chains);
                encode_csv(&records)
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
            "price": row.spot.to_f64(),
        })],
        Dataset::Volatility => vec![serde_json::json!({
            "step": step,
            "simulated_at": simulated_at,
            "symbol": symbol,
            "base_volatility": row.base_volatility.to_f64(),
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
                    rows.push(serde_json::json!({
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
                    }));
                }
            }
            rows
        }
    }
}

/// The CSV records one step contributes, in the header's order.
fn csv_rows(
    dataset: Dataset,
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
            row.spot.to_f64().to_string(),
        ]],
        Dataset::Volatility => vec![vec![
            step.to_string(),
            simulated_at.to_string(),
            symbol.to_string(),
            row.base_volatility.to_f64().to_string(),
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
                    records.push(vec![
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
                    ]);
                }
            }
            records
        }
    }
}

/// Converts a decimal to the wire's `f64`.
#[must_use]
#[inline]
fn decimal_to_f64(value: rust_decimal::Decimal) -> Option<f64> {
    use rust_decimal::prelude::ToPrimitive;
    value.to_f64()
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
        for column in Dataset::OptionChains.header() {
            assert!(
                first.get(*column).is_some(),
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
        let builder = match SeriesBuilder::new(&parameters, &tape) {
            Ok(builder) => builder,
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

    /// Every dataset's header matches the columns its rows carry.
    #[test]
    fn test_every_header_matches_its_row_width() {
        for (dataset, width) in [
            (Dataset::Underlying, 4),
            (Dataset::Volatility, 4),
            (Dataset::OptionChains, 17),
        ] {
            assert_eq!(dataset.header().len(), width, "{dataset:?}");
        }
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
