//! The storage-agnostic contract for the v2 snapshot tape.
//!
//! One trait with four operations, in the shape the two real query patterns
//! need: reconstruct a whole snapshot (or a range of them) for #49's exports,
//! and follow one `(expiration, strike, side)` across simulated time for a
//! chart or a backtest.
//!
//! # What a reader is promised
//!
//! * **Never a partial snapshot.** A snapshot becomes visible only once every
//!   one of its quote rows has been accepted *and* a completion marker carrying
//!   the expected row count has been written after them. A read that finds the
//!   marker but not the rows it promises reports the snapshot as absent, so a
//!   half-written step reads exactly like a step that was never written.
//! * **Never a duplicate.** Persisting the same `(simulation, generation, step)`
//!   twice is idempotent from every read path here, including immediately after
//!   the second write and before any background merge has run.
//! * **A stable order.** Snapshots come back by step ascending, expirations by
//!   expiry ascending, strikes ascending; a contract history comes back by
//!   simulated time ascending. Two calls with the same arguments return the
//!   same rows in the same order.
//! * **A bounded answer.** Every read is capped by `OCS_SNAPSHOT_MAX_READ_ROWS`.
//!   A request that would exceed it is a typed error naming the knob, never a
//!   silently truncated tape — a short answer is indistinguishable from a gap
//!   in the data, and #49's exports page rather than guess.

use super::record::{ContractQuote, ContractSide, SnapshotRecord};
use crate::utils::ChainError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use positive::Positive;
use uuid::Uuid;

/// Which contract's history to read, and over which steps.
///
/// A struct rather than seven positional arguments: the three `u64`-ish fields
/// would otherwise be trivially transposable at a call site, and transposing
/// `from_step` and `to_step` is a silent empty result.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractSeriesQuery {
    /// The simulation to read.
    pub simulation: Uuid,
    /// The generation the rows were written under —
    /// [`super::record::CURRENT_SNAPSHOT_GENERATION`] for anything this build
    /// materialised. Not the session's `version`.
    pub generation: u64,
    /// The contract's absolute expiration, exactly as persisted.
    pub expires_at: DateTime<Utc>,
    /// The contract's strike, exactly as persisted.
    pub strike: Positive,
    /// Which side to project.
    pub side: ContractSide,
    /// First step to consider, inclusive.
    pub from_step: usize,
    /// Last step to consider, inclusive.
    pub to_step: usize,
}

impl ContractSeriesQuery {
    /// Creates a contract-history query.
    #[must_use]
    pub fn new(
        simulation: Uuid,
        generation: u64,
        expires_at: DateTime<Utc>,
        strike: Positive,
        side: ContractSide,
        from_step: usize,
        to_step: usize,
    ) -> Self {
        Self {
            simulation,
            generation,
            expires_at,
            strike,
            side,
            from_step,
            to_step,
        }
    }
}

/// Persists and reads the v2 snapshot tape.
///
/// Object-safe: the service holds an `Arc<dyn SimulationSnapshotRepository>`
/// when persistence is configured and nothing at all when it is not, so an
/// absent warehouse costs a branch rather than a failing call.
#[async_trait]
pub trait SimulationSnapshotRepository: Send + Sync {
    /// Persists one complete snapshot.
    ///
    /// Writes every quote row as **one** bounded batch and only then the
    /// completion marker, so a reader can never observe the marker without the
    /// rows it promises. Retrying the same `(simulation, generation, step)` is
    /// safe: the rows carry a deterministic identity and replace rather than
    /// accumulate.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] when the record is malformed or
    /// larger than `OCS_SNAPSHOT_BATCH_ROWS`, and
    /// [`ChainError::ClickHouseError`] when the warehouse rejects or times out
    /// the write. Callers on the advance path treat a failure as non-fatal: the
    /// snapshot is deterministic, so a later replay can persist the same step.
    async fn persist(&self, record: SnapshotRecord) -> Result<(), ChainError>;

    /// Reads back one snapshot, or `None` when it was never completed.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] when the snapshot is larger than one
    /// read may return, and [`ChainError::ClickHouseError`] when the warehouse
    /// is unreachable or a stored value is unreadable.
    async fn get(
        &self,
        simulation: Uuid,
        generation: u64,
        step: usize,
    ) -> Result<Option<SnapshotRecord>, ChainError>;

    /// Reads an inclusive range of steps, ascending.
    ///
    /// Steps that were never completed are **skipped**, not faked: the caller
    /// sees which steps are present and can replay the rest deterministically.
    ///
    /// The whole range is materialised, bounded by `OCS_SNAPSHOT_MAX_READ_ROWS`
    /// — hence `read_`, not `stream_`. A caller exporting a multi-year
    /// simulation pages the range rather than asking for a stream this does not
    /// provide.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] when the range is reversed or would
    /// exceed `OCS_SNAPSHOT_MAX_READ_ROWS`, and
    /// [`ChainError::ClickHouseError`] when the warehouse is unreachable or a
    /// stored value is unreadable.
    async fn read_range(
        &self,
        simulation: Uuid,
        generation: u64,
        from_step: usize,
        to_step: usize,
    ) -> Result<Vec<SnapshotRecord>, ChainError>;

    /// Reads one contract's history, ordered by simulated time.
    ///
    /// Only steps whose snapshot is complete contribute, so a chart never shows
    /// a point that came from a half-written step.
    ///
    /// # Errors
    ///
    /// As [`SimulationSnapshotRepository::read_range`].
    async fn contract_series(
        &self,
        query: ContractSeriesQuery,
    ) -> Result<Vec<ContractQuote>, ChainError>;
}

#[cfg(test)]
mod tests {
    use super::super::record::{CURRENT_SNAPSHOT_GENERATION, ExpirationRecord, QuoteRow};
    use super::*;
    use chrono::TimeZone;
    use positive::pos_or_panic;
    use rust_decimal_macros::dec;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// A hermetic implementation of the contract, standing in for the
    /// warehouse.
    ///
    /// Exists to prove two things without a server: that the trait is
    /// object-safe behind an `Arc<dyn _>` — which is how the service holds it —
    /// and that the documented ordering and idempotence are expressible.
    #[derive(Default)]
    struct InMemorySnapshotRepository {
        stored: Mutex<BTreeMap<(Uuid, u64, usize), SnapshotRecord>>,
    }

    #[async_trait]
    impl SimulationSnapshotRepository for InMemorySnapshotRepository {
        async fn persist(&self, record: SnapshotRecord) -> Result<(), ChainError> {
            record.validate()?;
            let key = (record.simulation, record.generation, record.step);
            self.stored.lock().await.insert(key, record);
            Ok(())
        }

        async fn get(
            &self,
            simulation: Uuid,
            generation: u64,
            step: usize,
        ) -> Result<Option<SnapshotRecord>, ChainError> {
            Ok(self
                .stored
                .lock()
                .await
                .get(&(simulation, generation, step))
                .cloned())
        }

        async fn read_range(
            &self,
            simulation: Uuid,
            generation: u64,
            from_step: usize,
            to_step: usize,
        ) -> Result<Vec<SnapshotRecord>, ChainError> {
            Ok(self
                .stored
                .lock()
                .await
                .range((simulation, generation, from_step)..=(simulation, generation, to_step))
                .map(|(_, record)| record.clone())
                .collect())
        }

        async fn contract_series(
            &self,
            query: ContractSeriesQuery,
        ) -> Result<Vec<ContractQuote>, ChainError> {
            let stored = self.stored.lock().await;
            let mut series = Vec::new();
            for record in stored
                .range(
                    (query.simulation, query.generation, query.from_step)
                        ..=(query.simulation, query.generation, query.to_step),
                )
                .map(|(_, record)| record)
            {
                for expiration in &record.expirations {
                    if expiration.expires_at != query.expires_at {
                        continue;
                    }
                    for quote in &expiration.quotes {
                        if quote.strike != query.strike {
                            continue;
                        }
                        // The side is a projection, exactly as it is in the SQL
                        // implementation: one stored row, two ways to read it.
                        let (bid, ask, mid, delta) = match query.side {
                            ContractSide::Call => (
                                quote.call_bid,
                                quote.call_ask,
                                quote.call_mid,
                                quote.delta_call,
                            ),
                            ContractSide::Put => {
                                (quote.put_bid, quote.put_ask, quote.put_mid, quote.delta_put)
                            }
                        };
                        series.push(ContractQuote {
                            step: record.step,
                            simulated_at: record.simulated_at,
                            expires_at: expiration.expires_at,
                            days_to_expiration: expiration.days_to_expiration,
                            strike: quote.strike,
                            side: query.side,
                            implied_volatility: quote.implied_volatility,
                            bid,
                            ask,
                            mid,
                            delta,
                            gamma: quote.gamma,
                        });
                    }
                }
            }
            Ok(series)
        }
    }

    fn instant(day: u32) -> DateTime<Utc> {
        match Utc.with_ymd_and_hms(2026, 1, day, 14, 30, 0).single() {
            Some(instant) => instant,
            None => panic!("the test instant must be valid"),
        }
    }

    /// One strike whose two sides are told apart by every value, including a
    /// missing put mid.
    ///
    /// Deliberately asymmetric: a projection that ignored the requested side
    /// would return the call's numbers, and a fixture with matching sides could
    /// not tell the difference.
    fn quote() -> QuoteRow {
        QuoteRow::new(pos_or_panic!(5000.0), pos_or_panic!(0.18))
            .with_call(
                Some(pos_or_panic!(1.0)),
                Some(pos_or_panic!(1.2)),
                Some(pos_or_panic!(1.1)),
                Some(dec!(0.51)),
            )
            .with_put(
                Some(pos_or_panic!(0.8)),
                Some(pos_or_panic!(1.0)),
                None,
                Some(dec!(-0.49)),
            )
            .with_gamma(Some(dec!(0.003)))
    }

    fn record(simulation: Uuid, step: usize) -> SnapshotRecord {
        SnapshotRecord::new(
            simulation,
            CURRENT_SNAPSHOT_GENERATION,
            step,
            instant(5),
            "SPX".to_string(),
            pos_or_panic!(5000.0),
            pos_or_panic!(0.18),
            vec![ExpirationRecord::new(
                instant(9),
                pos_or_panic!(4.0),
                vec!["weeklies".to_string()],
                vec![quote()],
            )],
        )
    }

    /// A history query for one side of the fixture's strike.
    fn series_query(simulation: Uuid, side: ContractSide) -> ContractSeriesQuery {
        ContractSeriesQuery::new(
            simulation,
            CURRENT_SNAPSHOT_GENERATION,
            instant(9),
            pos_or_panic!(5000.0),
            side,
            0,
            2,
        )
    }

    /// The trait is usable behind a trait object, which is how the service
    /// holds it — and how a deployment without ClickHouse holds nothing.
    #[tokio::test]
    async fn test_the_repository_is_object_safe() {
        let simulation = Uuid::from_u128(5);
        let repository: Arc<dyn SimulationSnapshotRepository> =
            Arc::new(InMemorySnapshotRepository::default());

        match repository.persist(record(simulation, 0)).await {
            Ok(()) => {}
            Err(error) => panic!("the snapshot must persist: {error}"),
        }
        match repository
            .get(simulation, CURRENT_SNAPSHOT_GENERATION, 0)
            .await
        {
            Ok(Some(found)) => assert_eq!(found.step, 0),
            other => panic!("the snapshot must be readable, got {other:?}"),
        }
    }

    /// A missing snapshot is `None`, not an error: an unpersisted step is a
    /// normal state that deterministic replay can fill.
    #[tokio::test]
    async fn test_a_missing_snapshot_reads_as_absent() {
        let repository = InMemorySnapshotRepository::default();

        match repository
            .get(Uuid::from_u128(5), CURRENT_SNAPSHOT_GENERATION, 3)
            .await
        {
            Ok(None) => {}
            other => panic!("expected an absent snapshot, got {other:?}"),
        }
    }

    /// A range comes back ascending by step.
    #[tokio::test]
    async fn test_a_range_comes_back_ascending() {
        let simulation = Uuid::from_u128(5);
        let repository = InMemorySnapshotRepository::default();
        for step in [2, 0, 1] {
            match repository.persist(record(simulation, step)).await {
                Ok(()) => {}
                Err(error) => panic!("the snapshot must persist: {error}"),
            }
        }

        match repository
            .read_range(simulation, CURRENT_SNAPSHOT_GENERATION, 0, 2)
            .await
        {
            Ok(range) => {
                let steps: Vec<usize> = range.iter().map(|record| record.step).collect();
                assert_eq!(steps, vec![0, 1, 2]);
            }
            Err(error) => panic!("the range must read: {error}"),
        }
    }

    /// A contract history is addressed by expiration, strike and side, and the
    /// call side returns the call's numbers.
    #[tokio::test]
    async fn test_a_contract_history_selects_the_call_side() {
        let simulation = Uuid::from_u128(5);
        let repository = InMemorySnapshotRepository::default();
        for step in 0..3 {
            match repository.persist(record(simulation, step)).await {
                Ok(()) => {}
                Err(error) => panic!("the snapshot must persist: {error}"),
            }
        }

        match repository
            .contract_series(series_query(simulation, ContractSide::Call))
            .await
        {
            Ok(series) => {
                assert_eq!(series.len(), 3);
                for quote in &series {
                    assert_eq!(quote.side, ContractSide::Call);
                    assert_eq!(quote.strike, pos_or_panic!(5000.0));
                    assert_eq!(quote.bid, Some(pos_or_panic!(1.0)));
                    assert_eq!(quote.mid, Some(pos_or_panic!(1.1)));
                    assert_eq!(quote.delta, Some(dec!(0.51)));
                    // Gamma is shared by both sides.
                    assert_eq!(quote.gamma, Some(dec!(0.003)));
                }
            }
            Err(error) => panic!("the series must read: {error}"),
        }
    }

    /// The put side returns the put's numbers, including its missing mid.
    ///
    /// The other half of the projection: without it, an implementation that
    /// always read the call columns would pass every assertion above.
    #[tokio::test]
    async fn test_a_contract_history_selects_the_put_side() {
        let simulation = Uuid::from_u128(5);
        let repository = InMemorySnapshotRepository::default();
        match repository.persist(record(simulation, 0)).await {
            Ok(()) => {}
            Err(error) => panic!("the snapshot must persist: {error}"),
        }

        match repository
            .contract_series(series_query(simulation, ContractSide::Put))
            .await
        {
            Ok(series) => {
                assert_eq!(series.len(), 1);
                match series.first() {
                    Some(quote) => {
                        assert_eq!(quote.side, ContractSide::Put);
                        assert_eq!(quote.bid, Some(pos_or_panic!(0.8)));
                        assert_eq!(quote.ask, Some(pos_or_panic!(1.0)));
                        assert_eq!(quote.mid, None, "a missing quote stays missing");
                        assert_eq!(quote.delta, Some(dec!(-0.49)));
                        assert_eq!(quote.gamma, Some(dec!(0.003)));
                    }
                    None => panic!("the series must carry a point"),
                }
            }
            Err(error) => panic!("the series must read: {error}"),
        }
    }

    /// Persisting the same coordinate twice leaves one snapshot.
    #[tokio::test]
    async fn test_persisting_twice_is_idempotent() {
        let simulation = Uuid::from_u128(5);
        let repository = InMemorySnapshotRepository::default();

        for _ in 0..2 {
            match repository.persist(record(simulation, 0)).await {
                Ok(()) => {}
                Err(error) => panic!("the snapshot must persist: {error}"),
            }
        }

        match repository
            .read_range(simulation, CURRENT_SNAPSHOT_GENERATION, 0, 0)
            .await
        {
            Ok(range) => assert_eq!(range.len(), 1),
            Err(error) => panic!("the range must read: {error}"),
        }
    }
}
