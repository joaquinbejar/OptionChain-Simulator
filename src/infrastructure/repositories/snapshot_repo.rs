//! The ClickHouse-backed store for the v2 snapshot tape (issue #56).
//!
//! Two tables — the metadata of a step and the flattened quotes of that step —
//! written in a fixed order that makes a half-written snapshot invisible, and
//! read with deduplication applied at query time so a retry can never surface
//! twice.
//!
//! # The completion protocol
//!
//! A snapshot is persisted in two writes, in this order:
//!
//! 1. **Every quote row, as one batch.** One `INSERT`, never one per contract.
//!    Either the server accepts the whole batch or the write fails and nothing
//!    is marked.
//! 2. **The completion marker**, carrying the number of quote rows the batch
//!    contained.
//!
//! A reader therefore sees one of three states. No marker: the snapshot is
//! absent, which is exactly how an unpersisted step reads, and deterministic
//! replay can produce it. A marker whose `quote_count` matches the rows on
//! disk: the snapshot is whole. A marker whose count does *not* match: the
//! snapshot is reported absent and logged at `WARN`, because a snapshot missing
//! strikes is worse than a snapshot missing entirely — a backtest would price
//! against a chain with holes in it.
//!
//! # Why every read deduplicates
//!
//! `ReplacingMergeTree` collapses duplicate coordinates *eventually*, on a
//! background merge. Idempotence has to hold immediately — the second write of
//! a retried step is exactly when a duplicate would be read — so the row reads
//! use `FINAL` and the count checks use `uniqExact` over the identity columns.
//! Both are bounded by the `(simulation_id, simulation_generation)` prefix of the
//! sorting key, so they never degenerate into a full scan.
//!
//! # When ClickHouse is down
//!
//! Every driver error becomes a [`ChainError`] here; no `clickhouse::error`
//! type appears in a public signature, and neither the connection string nor
//! the credentials are ever logged. The advance path treats a failed persist as
//! non-fatal: the snapshot is deterministic, so the step can be written later
//! by a replay without the client ever noticing.

use crate::infrastructure::clickhouse::snapshots::interface::{
    ContractSeriesQuery, SimulationSnapshotRepository,
};
use crate::infrastructure::clickhouse::snapshots::model::{
    ContractReadRow, DECIMAL_SCALE, OptionQuoteRow, QUOTES_TABLE, QuoteReadRow, SNAPSHOTS_TABLE,
    SnapshotMetaReadRow, SnapshotMetaRow, contract_quote_from_row, meta_row, quote_rows,
    record_from_rows, to_storage_instant, to_storage_positive,
};
use crate::infrastructure::clickhouse::snapshots::record::{
    ContractQuote, ContractSide, SnapshotRecord,
};
use crate::infrastructure::config::snapshot::SnapshotPersistenceConfig;
use crate::infrastructure::{ClickHouseClient, ClickHouseConfig};
use crate::utils::ChainError;
use async_trait::async_trait;
use chrono::Utc;
use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

/// The DDL of the metadata table, shipped with the crate.
///
/// Checked in under `src/infrastructure/clickhouse/schema/` rather than beside
/// the compose files because it is compiled into the binary: the code that
/// inserts these rows and the schema they must match travel together, and a
/// deployment asset could drift from the row types without the compiler
/// noticing.
const SNAPSHOTS_DDL: &str = include_str!("../clickhouse/schema/simulation_snapshots.sql");

/// The DDL of the quotes table. See [`SNAPSHOTS_DDL`].
const QUOTES_DDL: &str = include_str!("../clickhouse/schema/simulation_option_quotes.sql");

/// The placeholder the retention knob replaces in the DDL.
const RETENTION_PLACEHOLDER: &str = "{{RETENTION_DAYS}}";

/// Selects the metadata of every completed step in a range.
///
/// `FINAL` deduplicates a retried write immediately. `complete = true` is the
/// marker; the `quote_count` it carries is verified against the rows actually
/// read, in [`assemble`].
const META_RANGE_QUERY: &str = "SELECT \
        step, \
        snapshot_id, \
        toUnixTimestamp64Nano(simulated_at) AS simulated_at, \
        symbol, \
        underlying_price, \
        base_volatility, \
        quote_count \
    FROM simulation_snapshots FINAL \
    WHERE simulation_id = {simulation:String} \
      AND simulation_generation = {generation:UInt64} \
      AND step >= {from_step:UInt64} \
      AND step <= {to_step:UInt64} \
      AND complete = true \
    ORDER BY step ASC";

/// Selects the quotes of every step in a range, in storage order.
///
/// The ordering is the table's own sorting key, so this is a range read rather
/// than a sort, and it is the order [`assemble`] relies on to group rows into
/// expirations in one pass.
const QUOTES_RANGE_QUERY: &str = "SELECT \
        step, \
        toUnixTimestamp64Nano(expires_at) AS expires_at, \
        days_to_expiration, \
        labels, \
        strike, \
        implied_volatility, \
        call_bid, \
        call_ask, \
        call_mid, \
        put_bid, \
        put_ask, \
        put_mid, \
        delta_call, \
        delta_put, \
        gamma \
    FROM simulation_option_quotes FINAL \
    WHERE simulation_id = {simulation:String} \
      AND simulation_generation = {generation:UInt64} \
      AND step >= {from_step:UInt64} \
      AND step <= {to_step:UInt64} \
    ORDER BY step ASC, expires_at ASC, strike ASC";

/// The steps in a range whose stored quotes match their marker's count.
///
/// The completeness check for a query that does not materialise a whole
/// snapshot. `uniqExact` over the identity columns counts *deduplicated* rows
/// without paying for `FINAL`, which is what makes this affordable inside a
/// contract history.
const COMPLETE_STEPS_SUBQUERY: &str = "SELECT marker.step \
    FROM ( \
        SELECT step, quote_count \
        FROM simulation_snapshots FINAL \
        WHERE simulation_id = {simulation:String} \
          AND simulation_generation = {generation:UInt64} \
          AND step >= {from_step:UInt64} \
          AND step <= {to_step:UInt64} \
          AND complete = true \
    ) AS marker \
    INNER JOIN ( \
        SELECT step, uniqExact((expires_at, strike)) AS stored \
        FROM simulation_option_quotes \
        WHERE simulation_id = {simulation:String} \
          AND simulation_generation = {generation:UInt64} \
          AND step >= {from_step:UInt64} \
          AND step <= {to_step:UInt64} \
        GROUP BY step \
    ) AS counted ON marker.step = counted.step \
    WHERE marker.quote_count = counted.stored";

/// Builds the contract-history query for one side.
///
/// The side chooses column names from a closed match on [`ContractSide`] —
/// there is no path from a request value into this string, so the aliases are
/// the only thing that varies and the query stays a constant in every other
/// respect.
#[must_use]
fn contract_series_query(side: ContractSide, limit: usize) -> String {
    let (bid, ask, mid, delta) = match side {
        ContractSide::Call => ("call_bid", "call_ask", "call_mid", "delta_call"),
        ContractSide::Put => ("put_bid", "put_ask", "put_mid", "delta_put"),
    };

    // `limit` is a validated `usize` from configuration, never a request value.
    // It is interpolated rather than bound because the named parameters below
    // must survive verbatim — `format!` would try to read `{simulation:String}`
    // as a format specifier — and because a `LIMIT` placeholder is the one
    // position where server-side parameters are least portable.
    format!(
        "SELECT \
            quote.step AS step, \
            toUnixTimestamp64Nano(quote.simulated_at) AS simulated_at, \
            toUnixTimestamp64Nano(quote.expires_at) AS expires_at, \
            quote.days_to_expiration AS days_to_expiration, \
            quote.strike AS strike, \
            quote.implied_volatility AS implied_volatility, \
            quote.{bid} AS bid, \
            quote.{ask} AS ask, \
            quote.{mid} AS mid, \
            quote.{delta} AS delta, \
            quote.gamma AS gamma \
        FROM simulation_option_quotes AS quote FINAL \
        WHERE quote.simulation_id = {{simulation:String}} \
          AND quote.simulation_generation = {{generation:UInt64}} \
          AND quote.step >= {{from_step:UInt64}} \
          AND quote.step <= {{to_step:UInt64}} \
          AND quote.expires_at = fromUnixTimestamp64Nano({{expires_at:Int64}}) \
          AND quote.strike = toDecimal128({{strike:String}}, {DECIMAL_SCALE}) \
          AND quote.step IN ({COMPLETE_STEPS_SUBQUERY}) \
        ORDER BY quote.simulated_at ASC, quote.step ASC \
        LIMIT {limit}"
    )
}

/// Appends a row bound to a range query.
///
/// Callers pass [`ClickHouseSnapshotRepository::probe_limit`], one more than a
/// read may return, so a range that would exceed the bound comes back detectably
/// long instead of quietly truncated.
#[must_use]
fn with_probe_limit(query: &str, limit: usize) -> String {
    format!("{query} LIMIT {limit}")
}

/// Where a snapshot's rows are written.
///
/// A seam, not an abstraction for its own sake: it is what lets the write-order
/// and one-batch rules be tested without a ClickHouse server, which is the only
/// way they can be tested in the hermetic suite.
#[async_trait]
pub(crate) trait SnapshotWriter: Send + Sync {
    /// Writes EVERY quote row of one snapshot in a single batch.
    async fn write_quote_batch(&self, rows: &[OptionQuoteRow]) -> Result<(), ChainError>;

    /// Writes the completion marker. Called only after the batch succeeded.
    async fn write_completion_marker(&self, row: &SnapshotMetaRow) -> Result<(), ChainError>;
}

/// Runs the completion protocol: quotes first, as one batch, then the marker.
///
/// The ordering is the whole guarantee. If the batch fails, the marker is never
/// written and the step reads as absent; if the marker fails, the orphan quote
/// rows are invisible to every read path and a later retry overwrites them.
///
/// # Errors
///
/// Propagates whatever the writer reports.
async fn run_completion_protocol<W>(
    writer: &W,
    quotes: &[OptionQuoteRow],
    marker: &SnapshotMetaRow,
) -> Result<(), ChainError>
where
    W: SnapshotWriter + ?Sized,
{
    // An empty snapshot is legal — a schedule can leave a step with nothing
    // live — and an empty batch is a network round trip that writes nothing.
    if !quotes.is_empty() {
        writer.write_quote_batch(quotes).await?;
    }
    writer.write_completion_marker(marker).await
}

/// Persists and reads the v2 snapshot tape in ClickHouse.
pub struct ClickHouseSnapshotRepository {
    /// The shared ClickHouse client.
    client: Arc<ClickHouseClient>,
    /// The operator's limits: batch size, read bound, insert timeout,
    /// retention.
    config: SnapshotPersistenceConfig,
}

impl ClickHouseSnapshotRepository {
    /// Creates a repository over an existing client.
    ///
    /// `config.enabled` is not consulted here: whether persistence happens at
    /// all is decided when the service chooses to build a repository, in
    /// [`ClickHouseSnapshotRepository::from_env`].
    #[must_use]
    pub fn new(client: Arc<ClickHouseClient>, config: SnapshotPersistenceConfig) -> Self {
        Self { client, config }
    }

    /// Builds the repository the environment describes, or `None` when snapshot
    /// persistence is switched off.
    ///
    /// This call itself performs no I/O — the ClickHouse client connects lazily
    /// — but that does **not** make the warehouse optional once the knob is on:
    /// the service calls [`ClickHouseSnapshotRepository::ensure_schema`] at
    /// boot, so with `OCS_SNAPSHOT_PERSISTENCE_ENABLED=true` an unreachable
    /// ClickHouse fails startup. Persistence off is the configuration that
    /// tolerates its absence. Once running, a *later* outage is survivable:
    /// the advance path treats a failed persist as non-fatal.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] when a knob is set but invalid, and
    /// [`ChainError::ClickHouseError`] when the client cannot be built.
    pub fn from_env() -> Result<Option<Self>, ChainError> {
        let config = SnapshotPersistenceConfig::from_env()?;
        if !config.enabled {
            // Not logged here: the caller that decides what to do without a
            // repository is the one that should say so, and saying it twice
            // reads like two different facts.
            return Ok(None);
        }

        let client = ClickHouseClient::new(ClickHouseConfig::default())?;
        Ok(Some(Self::new(Arc::new(client), config)))
    }

    /// The limits this repository enforces.
    #[must_use]
    pub fn config(&self) -> &SnapshotPersistenceConfig {
        &self.config
    }

    /// Creates the two tables if they are absent.
    ///
    /// Idempotent, and safe to call at every startup. It will **not** alter a
    /// table that already exists, so changing `OCS_SNAPSHOT_RETENTION_DAYS`
    /// against a live deployment needs an explicit
    /// `ALTER TABLE ... MODIFY TTL`; the DDL documents that.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::ClickHouseError`] when the warehouse rejects the
    /// DDL or is unreachable.
    #[instrument(skip(self), level = "debug")]
    pub async fn ensure_schema(&self) -> Result<(), ChainError> {
        for ddl in [SNAPSHOTS_DDL, QUOTES_DDL] {
            // The only substitution is a `u32` that
            // `SnapshotPersistenceConfig::from_env` has already bounded, so no
            // external text can reach this statement.
            let statement = ddl.replace(
                RETENTION_PLACEHOLDER,
                &self.config.retention_days.to_string(),
            );
            self.client.client.query(&statement).execute().await?;
        }

        info!(
            retention_days = self.config.retention_days,
            "Ensured the v2 snapshot schema"
        );
        Ok(())
    }

    /// Reads the metadata of every completed step in an inclusive range.
    async fn fetch_meta_rows(
        &self,
        simulation: Uuid,
        generation: u64,
        from_step: u64,
        to_step: u64,
    ) -> Result<Vec<SnapshotMetaReadRow>, ChainError> {
        let sql = with_probe_limit(META_RANGE_QUERY, self.probe_limit()?);
        let rows = self
            .client
            .client
            .query(&sql)
            .param("simulation", simulation.to_string())
            .param("generation", generation)
            .param("from_step", from_step)
            .param("to_step", to_step)
            .fetch_all::<SnapshotMetaReadRow>()
            .await?;

        self.reject_if_over_budget(rows.len(), "snapshots")?;
        Ok(rows)
    }

    /// Reads the quotes of every step in an inclusive range, in storage order.
    async fn fetch_quote_rows(
        &self,
        simulation: Uuid,
        generation: u64,
        from_step: u64,
        to_step: u64,
    ) -> Result<Vec<QuoteReadRow>, ChainError> {
        let sql = with_probe_limit(QUOTES_RANGE_QUERY, self.probe_limit()?);
        let rows = self
            .client
            .client
            .query(&sql)
            .param("simulation", simulation.to_string())
            .param("generation", generation)
            .param("from_step", from_step)
            .param("to_step", to_step)
            .fetch_all::<QuoteReadRow>()
            .await?;

        self.reject_if_over_budget(rows.len(), "quotes")?;
        Ok(rows)
    }

    /// One more row than a read may return, so an over-long answer is
    /// detectable rather than silently truncated.
    fn probe_limit(&self) -> Result<usize, ChainError> {
        self.config
            .max_read_rows
            .checked_add(1)
            .ok_or_else(|| ChainError::Validation {
                field: "OCS_SNAPSHOT_MAX_READ_ROWS".to_string(),
                reason: "is too large to probe for truncation".to_string(),
            })
    }

    /// Fails when a read came back at the probe limit, which means the answer
    /// was cut off.
    fn reject_if_over_budget(&self, returned: usize, what: &str) -> Result<(), ChainError> {
        if returned > self.config.max_read_rows {
            return Err(ChainError::Validation {
                field: "OCS_SNAPSHOT_MAX_READ_ROWS".to_string(),
                reason: format!(
                    "the requested range holds more {what} than the configured bound of {}; \
                     request a smaller step range",
                    self.config.max_read_rows
                ),
            });
        }
        Ok(())
    }
}

/// Rebuilds the snapshots a range read returned, verifying each against its
/// marker.
///
/// Pure, so the completion check — the rule that decides whether a step is
/// visible at all — is testable without a server.
///
/// `quotes` must arrive in the storage order the range query asks for; rows are
/// grouped by step, and within a step by expiration, in a single pass.
///
/// # Errors
///
/// Returns [`ChainError::ClickHouseError`] when a stored value is unreadable or
/// carries a foreign identity.
fn assemble(
    simulation: Uuid,
    generation: u64,
    metas: &[SnapshotMetaReadRow],
    quotes: Vec<QuoteReadRow>,
) -> Result<Vec<SnapshotRecord>, ChainError> {
    let mut by_step: BTreeMap<u64, Vec<QuoteReadRow>> = BTreeMap::new();
    for row in quotes {
        by_step.entry(row.step).or_default().push(row);
    }

    let mut records = Vec::with_capacity(metas.len());
    for meta in metas {
        let rows = by_step.remove(&meta.step).unwrap_or_default();

        // The completion check. A marker promising more rows than the table
        // holds means the batch never fully landed, or that retention has
        // started eating the snapshot; either way it is not a snapshot anyone
        // should price against.
        //
        // The saturating fallback cannot fire on any target this builds for —
        // `usize` is at most 64 bits — and a hypothetical wider one would
        // saturate to a count that disagrees with any marker, which fails
        // closed: the snapshot is skipped rather than served short.
        let stored = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        if stored != meta.quote_count {
            warn!(
                simulation = %simulation,
                generation,
                step = meta.step,
                expected = meta.quote_count,
                stored,
                "Skipping an incomplete persisted snapshot"
            );
            continue;
        }

        records.push(record_from_rows(simulation, generation, meta, &rows)?);
    }

    Ok(records)
}

/// Validates an inclusive step range.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] when the range is reversed or a bound
/// does not fit its column.
fn step_bounds(from_step: usize, to_step: usize) -> Result<(u64, u64), ChainError> {
    if from_step > to_step {
        return Err(ChainError::Validation {
            field: "from_step".to_string(),
            reason: format!("must not exceed to_step, got {from_step} > {to_step}"),
        });
    }

    let from = u64::try_from(from_step).map_err(|_| ChainError::Validation {
        field: "from_step".to_string(),
        reason: format!("{from_step} does not fit a UInt64 column"),
    })?;
    let to = u64::try_from(to_step).map_err(|_| ChainError::Validation {
        field: "to_step".to_string(),
        reason: format!("{to_step} does not fit a UInt64 column"),
    })?;

    Ok((from, to))
}

/// The current ingestion timestamp, in unix milliseconds.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] when the host clock is before 1970, which
/// would make the `ReplacingMergeTree` version meaningless.
fn ingestion_timestamp_ms() -> Result<u64, ChainError> {
    u64::try_from(Utc::now().timestamp_millis())
        .map_err(|_| ChainError::Internal("the host clock is before 1970".to_string()))
}

#[async_trait]
impl SnapshotWriter for ClickHouseSnapshotRepository {
    /// Writes the batch as ONE `INSERT`.
    ///
    /// `Insert::write` buffers and flushes in chunks over a single request, so
    /// the loop is one network conversation regardless of how many rows it
    /// carries — the opposite of an insert per contract, which is what the
    /// issue rules out.
    async fn write_quote_batch(&self, rows: &[OptionQuoteRow]) -> Result<(), ChainError> {
        let timeout = Some(self.config.insert_timeout);
        let mut insert = self
            .client
            .client
            .insert::<OptionQuoteRow>(QUOTES_TABLE)
            .await?
            .with_timeouts(timeout, timeout);

        for row in rows {
            insert.write(row).await?;
        }
        insert.end().await?;

        Ok(())
    }

    async fn write_completion_marker(&self, row: &SnapshotMetaRow) -> Result<(), ChainError> {
        let timeout = Some(self.config.insert_timeout);
        let mut insert = self
            .client
            .client
            .insert::<SnapshotMetaRow>(SNAPSHOTS_TABLE)
            .await?
            .with_timeouts(timeout, timeout);

        insert.write(row).await?;
        insert.end().await?;

        Ok(())
    }
}

#[async_trait]
impl SimulationSnapshotRepository for ClickHouseSnapshotRepository {
    #[instrument(
        skip(self, record),
        fields(
            simulation = %record.simulation,
            generation = record.generation,
            step = record.step,
        ),
        level = "debug"
    )]
    async fn persist(&self, record: SnapshotRecord) -> Result<(), ChainError> {
        record.validate()?;

        let quote_count = record.quote_count();
        if quote_count > self.config.batch_rows {
            return Err(ChainError::Validation {
                field: "OCS_SNAPSHOT_BATCH_ROWS".to_string(),
                reason: format!(
                    "the snapshot holds {quote_count} quote rows, above the configured bound of {}",
                    self.config.batch_rows
                ),
            });
        }

        let inserted_at_ms = ingestion_timestamp_ms()?;
        let quotes = quote_rows(&record, inserted_at_ms)?;
        let marker = meta_row(&record, inserted_at_ms)?;

        run_completion_protocol(self, &quotes, &marker).await?;

        debug!(rows = quotes.len(), "Persisted a v2 snapshot");
        Ok(())
    }

    #[instrument(skip(self), level = "debug")]
    async fn get(
        &self,
        simulation: Uuid,
        generation: u64,
        step: usize,
    ) -> Result<Option<SnapshotRecord>, ChainError> {
        let (from, to) = step_bounds(step, step)?;

        let metas = self
            .fetch_meta_rows(simulation, generation, from, to)
            .await?;
        let Some(meta) = metas.first() else {
            return Ok(None);
        };

        // Checked before reading the rows: a snapshot too large to return is a
        // configuration answer, not a silently empty one.
        let expected = usize::try_from(meta.quote_count).unwrap_or(usize::MAX);
        self.reject_if_over_budget(expected, "quotes")?;

        let quotes = self
            .fetch_quote_rows(simulation, generation, from, to)
            .await?;
        let mut records = assemble(simulation, generation, std::slice::from_ref(meta), quotes)?;

        Ok(records.pop())
    }

    #[instrument(skip(self), level = "debug")]
    async fn read_range(
        &self,
        simulation: Uuid,
        generation: u64,
        from_step: usize,
        to_step: usize,
    ) -> Result<Vec<SnapshotRecord>, ChainError> {
        let (from, to) = step_bounds(from_step, to_step)?;

        let metas = self
            .fetch_meta_rows(simulation, generation, from, to)
            .await?;
        let quotes = self
            .fetch_quote_rows(simulation, generation, from, to)
            .await?;

        let records = assemble(simulation, generation, &metas, quotes)?;
        debug!(steps = records.len(), "Read a range of persisted snapshots");

        Ok(records)
    }

    #[instrument(
        skip(self, query),
        fields(simulation = %query.simulation, side = %query.side),
        level = "debug"
    )]
    async fn contract_series(
        &self,
        query: ContractSeriesQuery,
    ) -> Result<Vec<ContractQuote>, ChainError> {
        let (from, to) = step_bounds(query.from_step, query.to_step)?;
        let expires_at = to_storage_instant(query.expires_at, "expires_at")?;
        // The strike is matched as a decimal literal, not as a scaled integer:
        // the column is `Decimal(38, 28)` and `toDecimal128` parses the same
        // text `Positive` renders, so equality is exact.
        let strike_text = query.strike.to_dec().to_string();
        // Rendered once here purely to fail early on an unrepresentable strike;
        // the comparison itself uses the text above.
        to_storage_positive(query.strike, "strike")?;

        let sql = contract_series_query(query.side, self.probe_limit()?);
        let rows = self
            .client
            .client
            .query(&sql)
            .param("simulation", query.simulation.to_string())
            .param("generation", query.generation)
            .param("from_step", from)
            .param("to_step", to)
            .param("expires_at", expires_at)
            .param("strike", strike_text)
            .fetch_all::<ContractReadRow>()
            .await?;

        self.reject_if_over_budget(rows.len(), "quotes")?;

        let mut series = Vec::with_capacity(rows.len());
        for row in &rows {
            series.push(contract_quote_from_row(row, query.side)?);
        }

        debug!(points = series.len(), "Read a contract history");
        Ok(series)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::clickhouse::snapshots::model::{DECIMAL_SCALE, to_storage_decimal};
    use crate::infrastructure::clickhouse::snapshots::record::{ExpirationRecord, QuoteRow};
    use chrono::{DateTime, TimeZone};
    use positive::{Positive, pos_or_panic};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use std::str::FromStr;
    use std::sync::Mutex;

    fn instant(day: u32) -> DateTime<Utc> {
        match Utc.with_ymd_and_hms(2026, 1, day, 14, 30, 0).single() {
            Some(instant) => instant,
            None => panic!("the test instant must be valid"),
        }
    }

    /// A quote whose call bid carries the full 29 significant digits a
    /// `Decimal` can hold.
    ///
    /// Upstream's Black-Scholes kernels produce premiums like this, and they
    /// are exactly the values a `Float64` column would quietly round — so the
    /// fixture keeps one, and every round-trip assertion below is really an
    /// assertion about precision.
    fn full_precision_premium() -> Positive {
        let value = match Decimal::from_str("1.234567890123456789012345678") {
            Ok(value) => value,
            Err(error) => panic!("the fixture decimal must parse: {error}"),
        };
        match Positive::new_decimal(value) {
            Ok(value) => value,
            Err(error) => panic!("the fixture premium must be positive: {error}"),
        }
    }

    fn quote(strike: f64) -> QuoteRow {
        QuoteRow::new(pos_or_panic!(strike), pos_or_panic!(0.185))
            .with_call(
                Some(full_precision_premium()),
                Some(pos_or_panic!(1.35)),
                Some(pos_or_panic!(1.2)),
                Some(dec!(0.5123)),
            )
            .with_put(
                Some(pos_or_panic!(0.95)),
                Some(pos_or_panic!(1.15)),
                None,
                Some(dec!(-0.4877)),
            )
            .with_gamma(Some(dec!(0.00312345)))
    }

    fn record(simulation: Uuid, step: usize) -> SnapshotRecord {
        SnapshotRecord::new(
            simulation,
            2,
            step,
            instant(5),
            "SPX".to_string(),
            pos_or_panic!(5000.25),
            pos_or_panic!(0.18),
            vec![
                ExpirationRecord::new(
                    instant(6),
                    pos_or_panic!(1.5),
                    vec!["weeklies".to_string(), "zero_dte".to_string()],
                    vec![quote(4975.0), quote(5000.0), quote(5025.0)],
                ),
                ExpirationRecord::new(
                    instant(9),
                    pos_or_panic!(4.5),
                    vec!["weeklies".to_string()],
                    vec![quote(4975.0), quote(5000.0)],
                ),
            ],
        )
    }

    /// What a writer was asked to do, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum WriteOp {
        /// A quote batch of the given size.
        Batch(usize),
        /// The completion marker, promising the given row count.
        Marker(u64),
    }

    /// A writer that records the protocol instead of performing it.
    #[derive(Default)]
    struct RecordingWriter {
        operations: Mutex<Vec<WriteOp>>,
        fail_batch: bool,
    }

    impl RecordingWriter {
        fn failing() -> Self {
            Self {
                operations: Mutex::new(Vec::new()),
                fail_batch: true,
            }
        }

        fn operations(&self) -> Vec<WriteOp> {
            match self.operations.lock() {
                Ok(operations) => operations.clone(),
                Err(error) => panic!("the recorder must not be poisoned: {error}"),
            }
        }

        fn push(&self, operation: WriteOp) {
            match self.operations.lock() {
                Ok(mut operations) => operations.push(operation),
                Err(error) => panic!("the recorder must not be poisoned: {error}"),
            }
        }
    }

    #[async_trait]
    impl SnapshotWriter for RecordingWriter {
        async fn write_quote_batch(&self, rows: &[OptionQuoteRow]) -> Result<(), ChainError> {
            self.push(WriteOp::Batch(rows.len()));
            if self.fail_batch {
                return Err(ChainError::ClickHouseError(
                    "the warehouse is down".to_string(),
                ));
            }
            Ok(())
        }

        async fn write_completion_marker(&self, row: &SnapshotMetaRow) -> Result<(), ChainError> {
            self.push(WriteOp::Marker(row.quote_count));
            Ok(())
        }
    }

    fn rows_for(record: &SnapshotRecord) -> (Vec<OptionQuoteRow>, SnapshotMetaRow) {
        let quotes = match quote_rows(record, 1) {
            Ok(rows) => rows,
            Err(error) => panic!("the record must convert: {error}"),
        };
        let marker = match meta_row(record, 1) {
            Ok(row) => row,
            Err(error) => panic!("the record must convert: {error}"),
        };
        (quotes, marker)
    }

    /// The read rows a range query would return for a record.
    fn read_rows(record: &SnapshotRecord) -> (SnapshotMetaReadRow, Vec<QuoteReadRow>) {
        let (quotes, marker) = rows_for(record);

        let meta = SnapshotMetaReadRow {
            step: marker.step,
            snapshot_id: marker.snapshot_id,
            simulated_at: marker.simulated_at,
            symbol: marker.symbol,
            underlying_price: marker.underlying_price,
            base_volatility: marker.base_volatility,
            quote_count: marker.quote_count,
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
            })
            .collect();

        (meta, quotes)
    }

    // ---- the write protocol ------------------------------------------------

    /// Every quote row of a snapshot goes out in ONE batch.
    ///
    /// The acceptance criterion that rules out an insert per contract: a
    /// five-row snapshot must produce exactly one batch carrying five rows, not
    /// five batches of one.
    #[tokio::test]
    async fn test_a_snapshot_writes_its_quotes_in_one_batch() {
        let writer = RecordingWriter::default();
        let (quotes, marker) = rows_for(&record(Uuid::from_u128(1), 0));

        match run_completion_protocol(&writer, &quotes, &marker).await {
            Ok(()) => {}
            Err(error) => panic!("the protocol must complete: {error}"),
        }

        let operations = writer.operations();
        let batches: Vec<&WriteOp> = operations
            .iter()
            .filter(|operation| matches!(operation, WriteOp::Batch(_)))
            .collect();
        assert_eq!(batches.len(), 1, "one snapshot must be one insert");
        assert_eq!(batches.first(), Some(&&WriteOp::Batch(5)));
    }

    /// The marker is written last, so a reader never meets it before the rows
    /// it promises.
    #[tokio::test]
    async fn test_the_marker_is_written_after_the_quotes() {
        let writer = RecordingWriter::default();
        let (quotes, marker) = rows_for(&record(Uuid::from_u128(1), 0));

        match run_completion_protocol(&writer, &quotes, &marker).await {
            Ok(()) => {}
            Err(error) => panic!("the protocol must complete: {error}"),
        }

        assert_eq!(
            writer.operations(),
            vec![WriteOp::Batch(5), WriteOp::Marker(5)]
        );
    }

    /// A failed batch never leaves a marker behind, which is what makes a
    /// half-written snapshot invisible instead of corrupt.
    #[tokio::test]
    async fn test_a_failed_batch_writes_no_marker() {
        let writer = RecordingWriter::failing();
        let (quotes, marker) = rows_for(&record(Uuid::from_u128(1), 0));

        match run_completion_protocol(&writer, &quotes, &marker).await {
            Err(ChainError::ClickHouseError(_)) => {}
            other => panic!("expected the batch failure to propagate, got {other:?}"),
        }

        assert_eq!(writer.operations(), vec![WriteOp::Batch(5)]);
    }

    /// An empty snapshot writes its marker and no batch at all: an empty insert
    /// is a round trip that stores nothing.
    #[tokio::test]
    async fn test_an_empty_snapshot_writes_only_its_marker() {
        let writer = RecordingWriter::default();
        let mut empty = record(Uuid::from_u128(1), 0);
        empty.expirations.clear();
        let (quotes, marker) = rows_for(&empty);

        match run_completion_protocol(&writer, &quotes, &marker).await {
            Ok(()) => {}
            Err(error) => panic!("the protocol must complete: {error}"),
        }

        assert_eq!(writer.operations(), vec![WriteOp::Marker(0)]);
    }

    // ---- the completion check ---------------------------------------------

    /// A snapshot whose rows match its marker reconstructs exactly.
    #[test]
    fn test_a_complete_snapshot_reconstructs() {
        let original = record(Uuid::from_u128(3), 4);
        let (meta, quotes) = read_rows(&original);

        match assemble(original.simulation, original.generation, &[meta], quotes) {
            Ok(records) => assert_eq!(records, vec![original]),
            Err(error) => panic!("the snapshot must reconstruct: {error}"),
        }
    }

    /// A marker promising more rows than the table holds is treated as absent.
    ///
    /// This is the case a torn write leaves behind, and the reason the count
    /// travels with the marker.
    #[test]
    fn test_a_marker_missing_quotes_reads_as_absent() {
        let original = record(Uuid::from_u128(3), 4);
        let (meta, mut quotes) = read_rows(&original);
        quotes.pop();

        match assemble(original.simulation, original.generation, &[meta], quotes) {
            Ok(records) => assert!(
                records.is_empty(),
                "an incomplete snapshot must not surface"
            ),
            Err(error) => panic!("an incomplete snapshot is not an error: {error}"),
        }
    }

    /// A marker with no rows at all is treated as absent too — the state a
    /// failed batch would leave if the marker had somehow been written.
    #[test]
    fn test_a_marker_with_no_quotes_reads_as_absent() {
        let original = record(Uuid::from_u128(3), 4);
        let (meta, _) = read_rows(&original);

        match assemble(
            original.simulation,
            original.generation,
            &[meta],
            Vec::new(),
        ) {
            Ok(records) => assert!(records.is_empty()),
            Err(error) => panic!("an incomplete snapshot is not an error: {error}"),
        }
    }

    /// A range keeps the complete steps and drops the incomplete ones, rather
    /// than failing the whole read: the caller can replay what is missing.
    #[test]
    fn test_a_range_skips_only_the_incomplete_steps() {
        let simulation = Uuid::from_u128(3);
        let (first_meta, first_quotes) = read_rows(&record(simulation, 0));
        let (second_meta, mut second_quotes) = read_rows(&record(simulation, 1));
        let (third_meta, third_quotes) = read_rows(&record(simulation, 2));
        second_quotes.truncate(1);

        let mut quotes = first_quotes;
        quotes.extend(second_quotes);
        quotes.extend(third_quotes);

        match assemble(
            simulation,
            2,
            &[first_meta, second_meta, third_meta],
            quotes,
        ) {
            Ok(records) => {
                let steps: Vec<usize> = records.iter().map(|record| record.step).collect();
                assert_eq!(steps, vec![0, 2]);
            }
            Err(error) => panic!("the range must read: {error}"),
        }
    }

    /// A range comes back ascending by step, which is what an export streams.
    #[test]
    fn test_a_range_reconstructs_in_step_order() {
        let simulation = Uuid::from_u128(3);
        let mut metas = Vec::new();
        let mut quotes = Vec::new();
        for step in 0..3 {
            let (meta, rows) = read_rows(&record(simulation, step));
            metas.push(meta);
            quotes.extend(rows);
        }

        match assemble(simulation, 2, &metas, quotes) {
            Ok(records) => {
                let steps: Vec<usize> = records.iter().map(|record| record.step).collect();
                assert_eq!(steps, vec![0, 1, 2]);
            }
            Err(error) => panic!("the range must read: {error}"),
        }
    }

    // ---- query construction ------------------------------------------------

    /// Every read deduplicates, immediately rather than eventually.
    #[test]
    fn test_every_row_read_uses_final() {
        assert!(META_RANGE_QUERY.contains("simulation_snapshots FINAL"));
        assert!(QUOTES_RANGE_QUERY.contains("simulation_option_quotes FINAL"));
        assert!(contract_series_query(ContractSide::Call, 10).contains("AS quote FINAL"));
    }

    /// The completeness subquery counts deduplicated rows without paying for
    /// `FINAL`, and compares them against the marker.
    #[test]
    fn test_the_completeness_subquery_deduplicates_its_count() {
        assert!(COMPLETE_STEPS_SUBQUERY.contains("uniqExact((expires_at, strike))"));
        assert!(COMPLETE_STEPS_SUBQUERY.contains("marker.quote_count = counted.stored"));
        assert!(COMPLETE_STEPS_SUBQUERY.contains("complete = true"));
    }

    /// A contract history only ever reads complete steps.
    #[test]
    fn test_a_contract_history_filters_on_complete_steps() {
        let sql = contract_series_query(ContractSide::Put, 100);

        assert!(sql.contains("quote.step IN (SELECT marker.step"));
        assert!(sql.contains("uniqExact"));
    }

    /// The side selects columns, and nothing else about the query changes.
    #[test]
    fn test_the_side_only_changes_the_projected_columns() {
        let call = contract_series_query(ContractSide::Call, 100);
        let put = contract_series_query(ContractSide::Put, 100);

        assert!(call.contains("quote.call_bid AS bid"));
        assert!(call.contains("quote.delta_call AS delta"));
        assert!(!call.contains("put_bid AS bid"));
        assert!(put.contains("quote.put_bid AS bid"));
        assert!(put.contains("quote.delta_put AS delta"));
        assert!(!put.contains("call_bid AS bid"));

        // The gamma is shared by both sides, so both project the same column.
        assert!(call.contains("quote.gamma AS gamma"));
        assert!(put.contains("quote.gamma AS gamma"));
    }

    /// Every value a caller controls is a named server-side parameter; none of
    /// them is ever interpolated into the SQL text.
    #[test]
    fn test_caller_values_are_bound_as_named_parameters() {
        let queries = [
            META_RANGE_QUERY.to_string(),
            QUOTES_RANGE_QUERY.to_string(),
            contract_series_query(ContractSide::Call, 100),
        ];

        for sql in &queries {
            assert!(sql.contains("{simulation:String}"), "{sql}");
            assert!(sql.contains("{generation:UInt64}"), "{sql}");
            // No quoting means no place for an injected literal to hide.
            assert!(!sql.contains('\''), "{sql}");
        }

        let series = contract_series_query(ContractSide::Call, 100);
        assert!(series.contains("fromUnixTimestamp64Nano({expires_at:Int64})"));
        assert!(series.contains("toDecimal128({strike:String}, 28)"));
    }

    /// The strike is compared at the scale the column stores, so an exact
    /// strike matches exactly.
    #[test]
    fn test_the_strike_comparison_uses_the_storage_scale() {
        let sql = contract_series_query(ContractSide::Call, 10);

        assert!(sql.contains(&format!("toDecimal128({{strike:String}}, {DECIMAL_SCALE})")));
    }

    /// A range query carries the configured bound, plus one so truncation is
    /// detectable.
    #[test]
    fn test_a_range_query_carries_its_probe_limit() {
        assert!(with_probe_limit(META_RANGE_QUERY, 501).ends_with("LIMIT 501"));
        assert!(contract_series_query(ContractSide::Put, 77).ends_with("LIMIT 77"));
    }

    /// A read at the probe limit is an error naming the knob, never a quietly
    /// short answer.
    #[test]
    fn test_an_over_long_read_names_the_knob() {
        let repository = ClickHouseSnapshotRepository::new(
            match ClickHouseClient::new(ClickHouseConfig::default()) {
                Ok(client) => Arc::new(client),
                Err(error) => panic!("the client must build: {error}"),
            },
            SnapshotPersistenceConfig {
                max_read_rows: 10,
                ..SnapshotPersistenceConfig::default()
            },
        );

        match repository.probe_limit() {
            Ok(limit) => assert_eq!(limit, 11),
            Err(error) => panic!("the probe limit must be computable: {error}"),
        }
        match repository.reject_if_over_budget(11, "quotes") {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "OCS_SNAPSHOT_MAX_READ_ROWS");
                assert!(reason.contains("smaller step range"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
        assert!(repository.reject_if_over_budget(10, "quotes").is_ok());
    }

    // ---- schema ------------------------------------------------------------

    /// The DDL is complete after substitution: no placeholder survives, and the
    /// retention lands in the TTL.
    #[test]
    fn test_the_ddl_substitutes_its_retention() {
        let statement = SNAPSHOTS_DDL.replace(RETENTION_PLACEHOLDER, "45");

        assert!(!statement.contains(RETENTION_PLACEHOLDER));
        assert!(statement.contains("INTERVAL 45 DAY DELETE"));
    }

    /// The engine, ordering and partitioning are the ones the query patterns
    /// were designed against — a change to any of them breaks either
    /// deduplication or the access path, so it must be deliberate.
    #[test]
    fn test_the_schema_keeps_its_engine_and_keys() {
        assert!(SNAPSHOTS_DDL.contains("ENGINE = ReplacingMergeTree(inserted_at_ms)"));
        assert!(SNAPSHOTS_DDL.contains("ORDER BY (simulation_id, simulation_generation, step)"));
        assert!(SNAPSHOTS_DDL.contains("PARTITION BY toYYYYMM(simulated_at)"));

        assert!(QUOTES_DDL.contains("ENGINE = ReplacingMergeTree(inserted_at_ms)"));
        assert!(
            QUOTES_DDL.contains(
                "ORDER BY (simulation_id, simulation_generation, step, expires_at, strike)"
            )
        );
        assert!(QUOTES_DDL.contains("PARTITION BY toYYYYMM(simulated_at)"));
        assert!(QUOTES_DDL.contains("INDEX idx_contract (expires_at, strike) TYPE minmax"));
    }

    /// The partition key is derived from the row's own content, not from when
    /// it was written.
    ///
    /// The subtle one: `ReplacingMergeTree` only deduplicates inside a
    /// partition, so partitioning on the ingestion time would let a backfill
    /// survive as a duplicate of the row it was meant to replace.
    #[test]
    fn test_the_partition_key_is_deterministic() {
        for ddl in [SNAPSHOTS_DDL, QUOTES_DDL] {
            assert!(ddl.contains("PARTITION BY toYYYYMM(simulated_at)"));
            assert!(
                !ddl.contains("PARTITION BY toYYYYMM(inserted"),
                "partitioning on the ingestion time would break deduplication"
            );
        }
    }

    /// Retention is anchored to ingestion time, because simulated time may sit
    /// years in the future.
    #[test]
    fn test_retention_is_anchored_to_ingestion_time() {
        for ddl in [SNAPSHOTS_DDL, QUOTES_DDL] {
            assert!(ddl.contains("TTL toDateTime(intDiv(inserted_at_ms, 1000))"));
        }
    }

    /// Every decimal column stores at the scale that never rounds a
    /// `rust_decimal`.
    #[test]
    fn test_the_decimal_columns_match_the_storage_scale() {
        let column = format!("Decimal(38, {DECIMAL_SCALE})");

        assert!(SNAPSHOTS_DDL.contains(&column));
        assert!(QUOTES_DDL.contains(&column));
        // A quote upstream never priced must survive as absent rather than as
        // a zero, so every optional column is nullable: six quotes, two deltas
        // and gamma.
        assert_eq!(
            QUOTES_DDL.matches(&format!("Nullable({column})")).count(),
            9
        );
    }

    // ---- bounds ------------------------------------------------------------

    /// A reversed range is refused before it reaches the warehouse.
    #[test]
    fn test_a_reversed_range_is_refused() {
        match step_bounds(9, 4) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "from_step");
                assert!(reason.contains("must not exceed to_step"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A single step is a legal range.
    #[test]
    fn test_a_single_step_range_is_accepted() {
        match step_bounds(7, 7) {
            Ok(bounds) => assert_eq!(bounds, (7, 7)),
            Err(error) => panic!("a single-step range must be accepted: {error}"),
        }
    }

    /// A snapshot above the batch bound never reaches the warehouse.
    #[tokio::test]
    async fn test_an_oversized_snapshot_is_refused() {
        let repository = ClickHouseSnapshotRepository::new(
            match ClickHouseClient::new(ClickHouseConfig::default()) {
                Ok(client) => Arc::new(client),
                Err(error) => panic!("the client must build: {error}"),
            },
            SnapshotPersistenceConfig {
                batch_rows: 2,
                ..SnapshotPersistenceConfig::default()
            },
        );

        // Five rows against a bound of two: refused before any I/O, which is
        // why this test needs no server.
        match repository.persist(record(Uuid::from_u128(1), 0)).await {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "OCS_SNAPSHOT_BATCH_ROWS");
                assert!(reason.contains("above the configured bound"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A malformed record is refused before any I/O too.
    #[tokio::test]
    async fn test_a_malformed_record_is_refused() {
        let repository = ClickHouseSnapshotRepository::new(
            match ClickHouseClient::new(ClickHouseConfig::default()) {
                Ok(client) => Arc::new(client),
                Err(error) => panic!("the client must build: {error}"),
            },
            SnapshotPersistenceConfig::default(),
        );
        let mut unordered = record(Uuid::from_u128(1), 0);
        unordered.expirations.reverse();

        match repository.persist(unordered).await {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "expirations"),
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// The repository reports the limits it was built with.
    #[test]
    fn test_the_repository_exposes_its_configuration() {
        let config = SnapshotPersistenceConfig {
            batch_rows: 7,
            ..SnapshotPersistenceConfig::default()
        };
        let repository = ClickHouseSnapshotRepository::new(
            match ClickHouseClient::new(ClickHouseConfig::default()) {
                Ok(client) => Arc::new(client),
                Err(error) => panic!("the client must build: {error}"),
            },
            config,
        );

        assert_eq!(repository.config().batch_rows, 7);
    }

    /// Persistence off means no repository at all, so a deployment without a
    /// warehouse never attempts a write.
    #[test]
    fn test_a_disabled_configuration_builds_no_repository() {
        // The ambient environment leaves the knob unset, and the default is
        // off — the state every existing deployment is in.
        match ClickHouseSnapshotRepository::from_env() {
            Ok(None) => {}
            Ok(Some(_)) => {
                // Only reachable when an operator enabled it in this shell.
                assert!(
                    std::env::var("OCS_SNAPSHOT_PERSISTENCE_ENABLED").is_ok(),
                    "persistence must not switch itself on"
                );
            }
            Err(error) => panic!("the ambient environment must load: {error}"),
        }
    }

    /// The scaled form of a strike agrees with the text the query compares
    /// against, so a match is exact rather than approximate.
    #[test]
    fn test_the_strike_text_and_its_storage_form_agree() {
        let strike = pos_or_panic!(5000.25);
        let text = strike.to_dec().to_string();

        assert_eq!(text, "5000.25");
        match to_storage_decimal(dec!(5000.25), "strike") {
            Ok(scaled) => assert_eq!(scaled, 50_002_500_000_000_000_000_000_000_000_000_i128),
            Err(error) => panic!("the strike must scale: {error}"),
        }
    }

    // ---- live ClickHouse ---------------------------------------------------

    /// Builds a repository against the ambient `CLICKHOUSE_*` configuration.
    fn live_repository() -> ClickHouseSnapshotRepository {
        let client = match ClickHouseClient::new(ClickHouseConfig::default()) {
            Ok(client) => Arc::new(client),
            Err(error) => panic!("the client must build: {error}"),
        };
        ClickHouseSnapshotRepository::new(client, SnapshotPersistenceConfig::default())
    }

    /// Removes a test simulation's rows, best effort.
    async fn cleanup(repository: &ClickHouseSnapshotRepository, simulation: Uuid) {
        for table in [SNAPSHOTS_TABLE, QUOTES_TABLE] {
            let _ = repository
                .client
                .client
                .query(&format!(
                    "DELETE FROM {table} WHERE simulation_id = {{simulation:String}}"
                ))
                .param("simulation", simulation.to_string())
                .execute()
                .await;
        }
    }

    /// The acceptance criterion, end to end: a snapshot written to a real
    /// ClickHouse reconstructs into exactly the record that was written.
    #[tokio::test]
    #[ignore = "requires live ClickHouse on localhost:8123 (override via CLICKHOUSE_* env)"]
    async fn test_a_snapshot_round_trips_through_live_clickhouse() {
        let repository = live_repository();
        let simulation = Uuid::new_v4();

        match repository.ensure_schema().await {
            Ok(()) => {}
            Err(error) => panic!("the schema must be creatable: {error}"),
        }

        let original = record(simulation, 0);
        match repository.persist(original.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("the snapshot must persist: {error}"),
        }

        let read = repository.get(simulation, original.generation, 0).await;
        cleanup(&repository, simulation).await;

        match read {
            Ok(Some(reconstructed)) => assert_eq!(reconstructed, original),
            other => panic!("the snapshot must reconstruct, got {other:?}"),
        }
    }

    /// Persisting the same snapshot twice is idempotent from every read path,
    /// immediately — before any background merge has had a chance to run.
    #[tokio::test]
    #[ignore = "requires live ClickHouse on localhost:8123 (override via CLICKHOUSE_* env)"]
    async fn test_persisting_twice_is_idempotent_against_live_clickhouse() {
        let repository = live_repository();
        let simulation = Uuid::new_v4();

        match repository.ensure_schema().await {
            Ok(()) => {}
            Err(error) => panic!("the schema must be creatable: {error}"),
        }

        let original = record(simulation, 0);
        for _ in 0..2 {
            match repository.persist(original.clone()).await {
                Ok(()) => {}
                Err(error) => panic!("the snapshot must persist: {error}"),
            }
        }

        let single = repository.get(simulation, original.generation, 0).await;
        let range = repository
            .read_range(simulation, original.generation, 0, 0)
            .await;
        let series = repository
            .contract_series(ContractSeriesQuery::new(
                simulation,
                original.generation,
                instant(6),
                pos_or_panic!(5000.0),
                ContractSide::Call,
                0,
                0,
            ))
            .await;
        cleanup(&repository, simulation).await;

        match single {
            Ok(Some(reconstructed)) => assert_eq!(reconstructed, original),
            other => panic!("the snapshot must reconstruct, got {other:?}"),
        }
        match range {
            Ok(records) => assert_eq!(records, vec![original]),
            other => panic!("the range must hold one snapshot, got {other:?}"),
        }
        match series {
            Ok(points) => assert_eq!(points.len(), 1, "a retry must not duplicate a quote"),
            other => panic!("the series must read, got {other:?}"),
        }
    }

    /// A marker whose quotes never landed reads as absent rather than as a
    /// snapshot with holes in it.
    #[tokio::test]
    #[ignore = "requires live ClickHouse on localhost:8123 (override via CLICKHOUSE_* env)"]
    async fn test_a_torn_write_reads_as_absent_against_live_clickhouse() {
        let repository = live_repository();
        let simulation = Uuid::new_v4();

        match repository.ensure_schema().await {
            Ok(()) => {}
            Err(error) => panic!("the schema must be creatable: {error}"),
        }

        // The state a crash between the two writes would leave: a marker
        // promising five rows, with none of them on disk.
        let original = record(simulation, 0);
        let marker = match meta_row(&original, 1) {
            Ok(row) => row,
            Err(error) => panic!("the record must convert: {error}"),
        };
        match repository.write_completion_marker(&marker).await {
            Ok(()) => {}
            Err(error) => panic!("the marker must write: {error}"),
        }

        let read = repository.get(simulation, original.generation, 0).await;
        let range = repository
            .read_range(simulation, original.generation, 0, 0)
            .await;
        cleanup(&repository, simulation).await;

        match read {
            Ok(None) => {}
            other => panic!("a torn write must read as absent, got {other:?}"),
        }
        match range {
            Ok(records) => assert!(records.is_empty()),
            other => panic!("a torn write must not appear in a range, got {other:?}"),
        }
    }

    /// A snapshot whose marker promises more rows than the table holds is
    /// invisible from every read path, including the contract history.
    ///
    /// The state a batch that landed short would leave. It also exercises the
    /// marker's own deduplication: two markers exist for the coordinate, and
    /// `FINAL` must resolve to the newer one.
    #[tokio::test]
    #[ignore = "requires live ClickHouse on localhost:8123 (override via CLICKHOUSE_* env)"]
    async fn test_a_short_batch_hides_the_snapshot_against_live_clickhouse() {
        let repository = live_repository();
        let simulation = Uuid::new_v4();

        match repository.ensure_schema().await {
            Ok(()) => {}
            Err(error) => panic!("the schema must be creatable: {error}"),
        }

        let original = record(simulation, 0);
        match repository.persist(original.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("the snapshot must persist: {error}"),
        }

        // A later marker claiming one row more than the batch wrote. Later, so
        // ReplacingMergeTree prefers it over the honest one.
        let later = match ingestion_timestamp_ms() {
            Ok(now) => now + 1_000,
            Err(error) => panic!("the clock must be readable: {error}"),
        };
        let mut inflated = match meta_row(&original, later) {
            Ok(row) => row,
            Err(error) => panic!("the record must convert: {error}"),
        };
        inflated.quote_count += 1;
        match repository.write_completion_marker(&inflated).await {
            Ok(()) => {}
            Err(error) => panic!("the marker must write: {error}"),
        }

        let read = repository.get(simulation, original.generation, 0).await;
        let range = repository
            .read_range(simulation, original.generation, 0, 0)
            .await;
        let series = repository
            .contract_series(ContractSeriesQuery::new(
                simulation,
                original.generation,
                instant(6),
                pos_or_panic!(5000.0),
                ContractSide::Call,
                0,
                0,
            ))
            .await;
        cleanup(&repository, simulation).await;

        match read {
            Ok(None) => {}
            other => panic!("a short batch must read as absent, got {other:?}"),
        }
        match range {
            Ok(records) => assert!(records.is_empty()),
            other => panic!("a short batch must not appear in a range, got {other:?}"),
        }
        match series {
            Ok(points) => assert!(
                points.is_empty(),
                "a contract history must not read from an incomplete step"
            ),
            other => panic!("the series must read, got {other:?}"),
        }
    }

    /// A contract history comes back in simulated-time order, carrying the
    /// selected side's quotes.
    #[tokio::test]
    #[ignore = "requires live ClickHouse on localhost:8123 (override via CLICKHOUSE_* env)"]
    async fn test_a_contract_history_reads_from_live_clickhouse() {
        let repository = live_repository();
        let simulation = Uuid::new_v4();

        match repository.ensure_schema().await {
            Ok(()) => {}
            Err(error) => panic!("the schema must be creatable: {error}"),
        }

        for (step, hours) in [(0_usize, 0_i64), (1, 1), (2, 2)] {
            let mut snapshot = record(simulation, step);
            // Distinct simulated instants, so the ordering is observable.
            snapshot.simulated_at = instant(5) + chrono::Duration::hours(hours);
            match repository.persist(snapshot).await {
                Ok(()) => {}
                Err(error) => panic!("the snapshot must persist: {error}"),
            }
        }

        let series = repository
            .contract_series(ContractSeriesQuery::new(
                simulation,
                2,
                instant(9),
                pos_or_panic!(5000.0),
                ContractSide::Put,
                0,
                2,
            ))
            .await;
        cleanup(&repository, simulation).await;

        match series {
            Ok(points) => {
                let steps: Vec<usize> = points.iter().map(|point| point.step).collect();
                assert_eq!(steps, vec![0, 1, 2]);
                for point in &points {
                    assert_eq!(point.side, ContractSide::Put);
                    assert_eq!(point.strike, pos_or_panic!(5000.0));
                    // The put mid is deliberately absent in the fixture, and a
                    // missing quote must survive as missing.
                    assert_eq!(point.mid, None);
                    assert_eq!(point.delta, Some(dec!(-0.4877)));
                }
            }
            other => panic!("the series must read, got {other:?}"),
        }
    }
}
