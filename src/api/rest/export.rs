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
use crate::session::{SimulationManager, SimulationParametersV2};
use crate::utils::ChainError;
use actix_web::{HttpRequest, HttpResponse, Responder, web};
use chrono::{DateTime, SecondsFormat, Utc};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tracing::{info, instrument, warn};
use utoipa::ToSchema;
use uuid::Uuid;

/// How many rows are buffered between the producer and the client.
///
/// Small on purpose: it is the backpressure. A slow client fills it, the
/// producer blocks on the next send, and no unbounded queue of priced chains
/// accumulates in memory.
const CHANNEL_CAPACITY: usize = 16;

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
        A simulation that has not been walked at all exports its whole tape. JSON is a single \
        array of row objects; CSV is RFC 4180 with a header row and CRLF line endings. Repeating \
        the same export yields byte-identical output.",
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
#[instrument(skip(manager, query), level = "debug")]
pub(crate) async fn export_simulation(
    req: HttpRequest,
    manager: web::Data<Arc<SimulationManager>>,
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

    // Priced chains are minutes of CPU for a long horizon. Producing them on an
    // Actix worker would block every other request on that thread.
    tokio::task::spawn_blocking(move || {
        if let Err(error) = produce(&parameters, dataset, format, range, &sender) {
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

/// Replays the range and sends every chunk.
///
/// Runs on a blocking thread. Returns as soon as a send fails, which is how a
/// disconnected client stops the work.
fn produce(
    parameters: &SimulationParametersV2,
    dataset: Dataset,
    format: Format,
    range: StepRange,
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

    for step in range.steps() {
        let row = tape
            .row(step)
            .ok_or_else(|| ChainError::Internal(format!("the tape has no row at step {step}")))?;
        let snapshot = match &builder {
            Some(builder) => Some(builder.snapshot(step)?),
            None => None,
        };

        let chunk = writer.rows(parameters, row.step, row, snapshot.as_ref())?;
        if !chunk.is_empty() && sender.blocking_send(Ok(chunk)).is_err() {
            return Ok(());
        }
    }

    if let Some(chunk) = writer.epilogue()?
        && sender.blocking_send(Ok(chunk)).is_err()
    {
        return Ok(());
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
    fn rows(
        &mut self,
        parameters: &SimulationParametersV2,
        step: usize,
        row: &crate::domain::factors::FactorRow,
        snapshot: Option<&SeriesSnapshot>,
    ) -> Result<Vec<u8>, ChainError> {
        let simulated_at = render_instant(row.simulated_at);
        let symbol = parameters.symbol.as_str();

        match self {
            Writer::Json { dataset, first } => {
                let values = json_rows(*dataset, step, &simulated_at, symbol, row, snapshot);
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
                let records = csv_rows(*dataset, step, &simulated_at, symbol, row, snapshot);
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
    snapshot: Option<&SeriesSnapshot>,
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
            let Some(snapshot) = snapshot else {
                return Vec::new();
            };
            let mut rows = Vec::new();
            for chain in &snapshot.chains {
                let expires_at = render_instant(chain.expires_at);
                let labels = chain.labels.join("|");
                for data in chain.chain.iter() {
                    rows.push(serde_json::json!({
                        "step": step,
                        "simulated_at": simulated_at,
                        "symbol": symbol,
                        "expires_at": expires_at,
                        "labels": labels,
                        "days_to_expiration": chain.days_to_expiration.to_f64(),
                        "strike": data.strike_price.to_f64(),
                        "implied_volatility": data.implied_volatility.to_f64(),
                        "call_bid": data.call_bid.map(|v| v.to_f64()),
                        "call_ask": data.call_ask.map(|v| v.to_f64()),
                        "call_mid": data.call_middle.map(|v| v.to_f64()),
                        "call_delta": data.delta_call.and_then(decimal_to_f64),
                        "put_bid": data.put_bid.map(|v| v.to_f64()),
                        "put_ask": data.put_ask.map(|v| v.to_f64()),
                        "put_mid": data.put_middle.map(|v| v.to_f64()),
                        "put_delta": data.delta_put.and_then(decimal_to_f64),
                        "gamma": data.gamma.and_then(decimal_to_f64),
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
    snapshot: Option<&SeriesSnapshot>,
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
            let Some(snapshot) = snapshot else {
                return Vec::new();
            };
            let mut records = Vec::new();
            for chain in &snapshot.chains {
                let expires_at = render_instant(chain.expires_at);
                // Joined with `|` rather than `,` so a multi-label chain stays
                // one column without depending on quoting to do it.
                let labels = chain.labels.join("|");
                for data in chain.chain.iter() {
                    records.push(vec![
                        step.to_string(),
                        simulated_at.to_string(),
                        symbol.to_string(),
                        expires_at.clone(),
                        labels.clone(),
                        chain.days_to_expiration.to_f64().to_string(),
                        data.strike_price.to_f64().to_string(),
                        data.implied_volatility.to_f64().to_string(),
                        render_optional(data.call_bid.map(|v| v.to_f64())),
                        render_optional(data.call_ask.map(|v| v.to_f64())),
                        render_optional(data.call_middle.map(|v| v.to_f64())),
                        render_optional(data.delta_call.and_then(decimal_to_f64)),
                        render_optional(data.put_bid.map(|v| v.to_f64())),
                        render_optional(data.put_ask.map(|v| v.to_f64())),
                        render_optional(data.put_middle.map(|v| v.to_f64())),
                        render_optional(data.delta_put.and_then(decimal_to_f64)),
                        render_optional(data.gamma.and_then(decimal_to_f64)),
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

    macro_rules! v2_service {
        () => {{
            let manager = Arc::new(crate::session::SimulationManager::new(
                Arc::new(InMemorySimulationStore::new()),
                SimulationV2Config::default(),
            ));
            actix_test::init_service(
                App::new().configure(|cfg| configure_v2_routes(cfg, manager.clone())),
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
