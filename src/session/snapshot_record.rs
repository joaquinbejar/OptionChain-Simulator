//! The domain snapshot, in the shape the warehouse stores.
//!
//! The conversion lives here rather than in `infrastructure` because
//! `SeriesSnapshot` is `pub(crate)` inside the private `domain` module: the
//! persistence layer cannot name it, and must not, or the layering would run
//! backwards. The session layer sees both sides, so it owns the translation —
//! the same place the f64 ↔ typed conversion already happens for the REST
//! boundary.
//!
//! What crosses is a flat, owned record. Nothing here interprets the numbers:
//! a quote that upstream did not price stays `None` rather than becoming a
//! zero, because a zero premium is a statement and a missing one is not.

use crate::domain::series::SeriesSnapshot;
use crate::infrastructure::{
    CURRENT_SNAPSHOT_GENERATION, ExpirationRecord, QuoteRow, SnapshotRecord,
};

/// The tape generation persisted snapshots are filed under.
///
/// Defers to [`CURRENT_SNAPSHOT_GENERATION`], which the persistence layer owns
/// and publishes: an external reader has to pass the same number to address a
/// row, so one definition rather than two that could drift.
///
/// **Not** the session's compare-and-swap revision, which changes on every
/// advance — filing steps under a moving version would scatter one simulation
/// across as many generations as it has steps, and make a retry a new row
/// instead of the same one.
///
/// A v2 simulation is immutable after creation, so its tape has exactly one
/// generation for its lifetime. This constant is bumped only if a change to the
/// walk mathematics would make the same effective inputs produce a different
/// tape — at which point stored snapshots from the old generation are still
/// readable, still correct for the binary that wrote them, and distinguishable
/// from the new ones.
pub(crate) const SNAPSHOT_TAPE_GENERATION: u64 = CURRENT_SNAPSHOT_GENERATION;

/// How many quote rows a snapshot would become.
///
/// Counted from the domain snapshot rather than from a built record, so a
/// caller can decide whether to build one at all.
#[must_use]
pub(crate) fn snapshot_quote_count(snapshot: &SeriesSnapshot) -> usize {
    snapshot
        .chains
        .iter()
        .map(|chain| chain.chain.options.len())
        .sum()
}

/// Builds the persistence record for a snapshot the manager just served.
///
/// Ordering is inherited, not imposed: the planner already yields expirations
/// chronologically and upstream holds strikes in a `BTreeSet`, so the record
/// reproduces the canonical order without sorting anything. That is what lets a
/// reconstruction from the warehouse compare equal to the in-memory snapshot.
pub(crate) fn snapshot_record(
    simulation: uuid::Uuid,
    symbol: &str,
    snapshot: &SeriesSnapshot,
) -> SnapshotRecord {
    let expirations = snapshot
        .chains
        .iter()
        .map(|chain| ExpirationRecord {
            expires_at: chain.expires_at,
            days_to_expiration: chain.days_to_expiration,
            labels: chain.labels.clone(),
            quotes: chain
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
                    // Read, never computed: `build_chain` asks upstream for the
                    // snapshots, so filing a step costs a clone and no
                    // arithmetic. A strike whose snapshot upstream could not
                    // build stays `None`, and is stored as NULL rather than as
                    // a zero somebody would trade on.
                    greeks_call: data.greeks_call.clone(),
                    greeks_put: data.greeks_put.clone(),
                })
                .collect(),
        })
        .collect();

    SnapshotRecord {
        simulation,
        generation: SNAPSHOT_TAPE_GENERATION,
        step: snapshot.step,
        simulated_at: snapshot.simulated_at,
        symbol: symbol.to_string(),
        spot: snapshot.spot,
        base_volatility: snapshot.base_volatility,
        expirations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
    use crate::api::rest::requests_v2::CreateSimulationRequest;
    use crate::domain::factors::FactorTape;
    use crate::domain::series::SeriesBuilder;
    use crate::session::{ExpiryRule, ExpiryRuleKind, SimulationParametersV2};
    use chrono::{TimeZone, Utc, Weekday};
    use uuid::Uuid;

    fn parameters() -> SimulationParametersV2 {
        let start_at = match Utc.with_ymd_and_hms(2026, 1, 5, 14, 30, 0).single() {
            Some(instant) => instant,
            None => panic!("the test instant must be valid"),
        };
        let rule =
            |id: &str, kind: ExpiryRuleKind, count: usize| match ExpiryRule::new(id, kind, count) {
                Ok(rule) => rule,
                Err(error) => panic!("the test rule must be valid: {error}"),
            };

        let request = CreateSimulationRequest {
            symbol: "SPX".to_string(),
            steps: 2,
            start_at: Some(start_at),
            step_interval_seconds: Some(86_400),
            timezone: "America/New_York".to_string(),
            calendar: Some("weekdays_v1".to_string()),
            expiration_time: "17:00".to_string(),
            schedules: vec![
                rule("zero_dte", ExpiryRuleKind::Daily, 1),
                rule(
                    "weeklies",
                    ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Wed, Weekday::Fri]),
                    2,
                ),
            ],
            initial_price: 5000.0,
            volatility: 0.18,
            risk_free_rate: 0.04,
            dividend_yield: 0.012,
            method: ApiWalkType::Brownian {
                dt: 0.004,
                drift: 0.0,
                volatility: 0.18,
            },
            time_frame: ApiTimeFrame::Day,
            chain_size: Some(5),
            strike_interval: Some(25.0),
            skew_slope: None,
            smile_curve: None,
            spread: Some(0.02),
            seed: Some(42),
        };

        match SimulationParametersV2::try_from(request) {
            Ok(parameters) => parameters,
            Err(error) => panic!("the request must convert: {error}"),
        }
    }

    /// The record carries every value the snapshot did, in the same order.
    ///
    /// This is the reconstruction guarantee at its source: whatever the
    /// warehouse can return is bounded above by what leaves here, so a field
    /// dropped in this conversion is a field no query can bring back.
    #[test]
    fn test_the_record_carries_the_whole_snapshot() {
        let parameters = parameters();
        let tape = match FactorTape::build(&parameters, &parameters.method) {
            Ok(tape) => tape,
            Err(error) => panic!("the tape must build: {error}"),
        };
        let builder = match SeriesBuilder::new(&parameters, &tape) {
            Ok(builder) => builder,
            Err(error) => panic!("the builder must accept the parameters: {error}"),
        };
        let snapshot = match builder.snapshot(1) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("the snapshot must build: {error}"),
        };

        let simulation = Uuid::new_v4();
        let record = snapshot_record(simulation, &parameters.symbol, &snapshot);

        assert_eq!(record.simulation, simulation);
        assert_eq!(record.generation, SNAPSHOT_TAPE_GENERATION);
        assert_eq!(record.step, snapshot.step);
        assert_eq!(record.simulated_at, snapshot.simulated_at);
        assert_eq!(record.symbol, parameters.symbol);
        assert_eq!(record.spot, snapshot.spot);
        assert_eq!(record.base_volatility, snapshot.base_volatility);
        assert_eq!(record.expirations.len(), snapshot.chains.len());
        assert!(
            !record.expirations.is_empty(),
            "the fixture must price something"
        );

        for (stored, live) in record.expirations.iter().zip(&snapshot.chains) {
            assert_eq!(stored.expires_at, live.expires_at);
            assert_eq!(stored.days_to_expiration, live.days_to_expiration);
            assert_eq!(stored.labels, live.labels);

            let quotes = live.chain.iter().count();
            assert_eq!(stored.quotes.len(), quotes, "every strike must be carried");

            for (row, data) in stored.quotes.iter().zip(live.chain.iter()) {
                assert_eq!(row.strike, data.strike_price);
                assert_eq!(row.implied_volatility, data.implied_volatility);
                assert_eq!(row.call_bid, data.call_bid);
                assert_eq!(row.call_ask, data.call_ask);
                assert_eq!(row.call_mid, data.call_middle);
                assert_eq!(row.put_bid, data.put_bid);
                assert_eq!(row.put_ask, data.put_ask);
                assert_eq!(row.put_mid, data.put_middle);
                assert_eq!(row.delta_call, data.delta_call);
                assert_eq!(row.delta_put, data.delta_put);
                assert_eq!(row.gamma, data.gamma);
            }
        }

        match record.validate() {
            Ok(()) => {}
            Err(error) => panic!("a record built from a real snapshot must be valid: {error}"),
        }
    }

    /// The same step files under the same id however often it is served, which
    /// is what makes a retry replace a row instead of adding one.
    #[test]
    fn test_the_same_step_files_under_a_stable_id() {
        let parameters = parameters();
        let tape = match FactorTape::build(&parameters, &parameters.method) {
            Ok(tape) => tape,
            Err(error) => panic!("the tape must build: {error}"),
        };
        let builder = match SeriesBuilder::new(&parameters, &tape) {
            Ok(builder) => builder,
            Err(error) => panic!("the builder must accept the parameters: {error}"),
        };
        let simulation = Uuid::new_v4();

        let first = match builder.snapshot(0) {
            Ok(snapshot) => snapshot_record(simulation, &parameters.symbol, &snapshot),
            Err(error) => panic!("the snapshot must build: {error}"),
        };
        let second = match builder.snapshot(0) {
            Ok(snapshot) => snapshot_record(simulation, &parameters.symbol, &snapshot),
            Err(error) => panic!("the snapshot must build: {error}"),
        };
        let next_step = match builder.snapshot(1) {
            Ok(snapshot) => snapshot_record(simulation, &parameters.symbol, &snapshot),
            Err(error) => panic!("the snapshot must build: {error}"),
        };

        assert_eq!(first.snapshot_id(), second.snapshot_id());
        assert_ne!(
            first.snapshot_id(),
            next_step.snapshot_id(),
            "two steps of one simulation are two rows"
        );
    }
}
