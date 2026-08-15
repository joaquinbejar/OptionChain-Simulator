//! Lazy rolling multi-expiration snapshots.
//!
//! A **snapshot** is the whole simulated market at one step: one clock, one
//! spot, one base volatility, and the ordered set of option chains that are
//! alive at that instant. It is assembled from two inputs that already exist —
//! a row of the [factor tape](crate::domain::factors) and the
//! [planner](crate::domain::expiry)'s active expirations — and it is built
//! **on demand**, never stored for every step.
//!
//! # Why lazily
//!
//! v1 materialises the whole chain tape up front, which costs
//! `O(steps × strikes)` and stops when its single expiry reaches zero. A
//! rolling simulation multiplies that by the number of live expirations and
//! runs for years, so materialising it is not an option: the reference
//! configuration alone is fifteen chains a step. Building from a factor row
//! keeps the resident cost at `O(steps)` small rows plus a bounded cache of
//! recently-served snapshots.
//!
//! # What this module does not do
//!
//! It does not price anything. Every chain comes from upstream
//! `OptionChain::build_chain`, through the one wiring point the factor tape
//! already uses, so premiums, Greeks, spreads, skew and smile are optionstratlib's
//! and there is no second implementation to drift. It also does not use
//! `generator_optionseries`: that ages expirations and drops them, but never
//! replenishes them, and its `OptionSeries::build_series` path does not
//! preserve the requested strike interval.
//!
//! # The one subtle decision
//!
//! Chains are built with `ExpirationDate::Days(fractional_dte)`, computed here
//! from the same `(simulated_at, expires_at)` pair the planner used — never
//! with `ExpirationDate::DateTime`. Upstream's `DateTime` variant computes the
//! remaining time against `Utc::now()` directly, which would make every premium
//! depend on when the request happened to arrive. Passing `Days` keeps chain
//! building a pure function of the tape and the planner, which is what the
//! replay guarantee rests on.
//!
//! # The one value upstream stamps from the host clock
//!
//! `OptionChain::build_chain` writes a `YYYY-MM-DD` `expiration_date` string
//! derived from `Utc::now()`, and the field is private with no setter, so it
//! cannot be normalised here. Nothing priced depends on it — `get_days()` and
//! `get_years()` read the `Days` value directly — and the authoritative
//! expiration is the absolute `expires_at` the planner produced, which is what
//! [`ExpiryChain`] carries and what #47 must put on the wire. The consequence
//! to keep in mind is narrow but real: two builds of the same snapshot either
//! side of UTC midnight carry different stamps, so the raw upstream chain must
//! not be serialised verbatim into a response.

use crate::domain::expiry::{ActiveExpiry, RollingPlanner};
use crate::domain::factors::{FactorRow, FactorTape, build_chain};
use crate::infrastructure::DEFAULT_MAX_CACHED_SNAPSHOTS;
use crate::session::SimulationParametersV2;
use crate::utils::ChainError;
use chrono::{DateTime, Utc};
use optionstratlib::ExpirationDate;
use optionstratlib::chains::chain::OptionChain;
use positive::Positive;
use std::collections::HashMap;
use std::time::Instant;
use tracing::{debug, instrument};
use uuid::Uuid;

/// One live expiration at one step: when it expires, how far away that is,
/// which rules asked for it, and its priced chain.
///
/// `PartialEq` is hand-written rather than derived, because upstream's
/// `OptionChain: PartialEq` compares only the expiration string and the symbol
/// — not the strikes, the premiums or the underlying price. Deriving here would
/// give every reproducibility test a comparison that passes with completely
/// different chains.
#[derive(Debug, Clone)]
pub(crate) struct ExpiryChain {
    /// The absolute expiration instant, in UTC.
    pub(crate) expires_at: DateTime<Utc>,
    /// Fractional days remaining, from the same `(simulated_at, expires_at)`
    /// pair the planner used. Always strictly positive: an expired chain is
    /// never emitted.
    pub(crate) days_to_expiration: Positive,
    /// The ids of every rule this expiration satisfies, sorted. A date claimed
    /// by both a weekly and a monthly rule appears **once**, with both labels —
    /// it is priced once, not twice.
    pub(crate) labels: Vec<String>,
    /// The priced chain, built entirely by upstream.
    pub(crate) chain: OptionChain,
}

impl PartialEq for ExpiryChain {
    /// Compares what the simulation produced, contract by contract.
    ///
    /// The chain's `expiration_date` string is deliberately excluded: upstream
    /// stamps it from the host calendar via `Utc::now()`, so including it would
    /// make two otherwise identical snapshots differ across UTC midnight. Every
    /// value that reaches a client — the strikes, the premiums, the Greeks and
    /// the underlying price — is compared.
    fn eq(&self, other: &Self) -> bool {
        self.expires_at == other.expires_at
            && self.days_to_expiration == other.days_to_expiration
            && self.labels == other.labels
            && self.chain.underlying_price == other.chain.underlying_price
            && self.chain.symbol == other.chain.symbol
            && self.chain.options == other.chain.options
    }
}

/// The whole simulated market at one step.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SeriesSnapshot {
    /// The 0-based step this snapshot describes.
    pub(crate) step: usize,
    /// The simulated instant, from the factor row.
    pub(crate) simulated_at: DateTime<Utc>,
    /// The underlying price shared by every chain in the snapshot.
    pub(crate) spot: Positive,
    /// The base implied volatility shared by every chain, before skew and
    /// smile shape it per strike.
    pub(crate) base_volatility: Positive,
    /// The live chains, ordered by `expires_at` ascending.
    pub(crate) chains: Vec<ExpiryChain>,
}

impl SeriesSnapshot {
    /// The chain expiring at `expires_at`, if the snapshot carries one.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the by-expiration lookup the tests use; the DTO layer walks \
                      `chains` in order instead"
        )
    )]
    pub(crate) fn chain_at(&self, expires_at: DateTime<Utc>) -> Option<&ExpiryChain> {
        self.chains
            .iter()
            .find(|chain| chain.expires_at == expires_at)
    }

    /// Every chain a given rule contributed to.
    ///
    /// A rule's count is satisfied by the chains carrying its label, which is
    /// not the same as the number of chains: coincident expirations are shared.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the per-rule view the inventory tests assert on; nothing served \
                      needs it, because a chain carries its own labels"
        )
    )]
    pub(crate) fn chains_for(&self, rule_id: &str) -> impl Iterator<Item = &ExpiryChain> {
        self.chains
            .iter()
            .filter(move |chain| chain.labels.iter().any(|label| label == rule_id))
    }
}

/// Builds snapshots for one simulation.
///
/// Borrows its inputs, so a caller that keeps the parameters and the tape in a
/// session can build a snapshot per request without cloning either.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SeriesBuilder<'a> {
    parameters: &'a SimulationParametersV2,
    tape: &'a FactorTape,
}

impl<'a> SeriesBuilder<'a> {
    /// Creates a builder over a simulation's parameters and its factor tape.
    ///
    /// Validates the parameters, for the same reason [`FactorTape::build`]
    /// does: `SimulationParametersV2` is public with public fields, so a caller
    /// can assemble one that never passed a validating constructor. The
    /// schedule cannot be invalid — its fields are private and `Deserialize`
    /// routes through the constructor — but `chain_size` is a bare `Option`
    /// whose cap lives only in `validate`, and it drives how many contracts
    /// every snapshot prices. Once per builder, not once per step.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] when the parameters are invalid.
    pub(crate) fn new(
        parameters: &'a SimulationParametersV2,
        tape: &'a FactorTape,
    ) -> Result<Self, ChainError> {
        parameters.validate()?;
        Ok(Self { parameters, tape })
    }

    /// Builds the snapshot at `step`.
    ///
    /// Pure: the same `(parameters, tape, step)` always produces the same
    /// snapshot, because the factor row is fixed, the planner is deterministic,
    /// and the chains are built from the two of them with no randomness and no
    /// clock read that reaches a priced value — see the module docs for the one
    /// stamp upstream writes from the host calendar.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::NotFound`] when `step` is past the end of the
    /// tape, [`ChainError::Validation`] when the schedule cannot be projected
    /// at that instant, and [`ChainError::Internal`] when a fractional
    /// days-to-expiration is not representable or upstream cannot build a
    /// chain.
    #[instrument(skip(self), level = "debug")]
    pub(crate) fn snapshot(&self, step: usize) -> Result<SeriesSnapshot, ChainError> {
        let row = self.tape.row(step).ok_or_else(|| {
            ChainError::NotFound(format!(
                "step {step} is past the end of a {}-step simulation",
                self.tape.len()
            ))
        })?;

        let planner = RollingPlanner::new(&self.parameters.schedule);
        let active = planner.active_at(row.simulated_at)?;

        let mut chains = Vec::with_capacity(active.len());
        for expiry in &active {
            chains.push(self.build_expiry_chain(row, expiry)?);
        }

        debug!(
            step,
            chains = chains.len(),
            "Built a rolling multi-expiration snapshot"
        );

        Ok(SeriesSnapshot {
            step: row.step,
            simulated_at: row.simulated_at,
            spot: row.spot,
            base_volatility: row.base_volatility,
            // The planner already returns its expirations chronologically and
            // deduplicated, so the snapshot inherits both properties.
            chains,
        })
    }

    /// Prices one expiration at one factor row.
    fn build_expiry_chain(
        &self,
        row: &FactorRow,
        expiry: &ActiveExpiry,
    ) -> Result<ExpiryChain, ChainError> {
        let days = expiry.days_to_expiration(row.simulated_at)?;
        let days_to_expiration = Positive::new_decimal(days).map_err(|e| {
            ChainError::Internal(format!(
                "days to expiration {days} for {} is not a valid Positive: {e}",
                expiry.expires_at
            ))
        })?;

        // `Days`, not `DateTime`: see the module docs. This is the value that
        // makes a premium a function of the simulated clock rather than of when
        // the request arrived.
        let chain = build_chain(
            self.parameters,
            row.spot,
            row.base_volatility,
            ExpirationDate::Days(days_to_expiration),
        )?;

        Ok(ExpiryChain {
            expires_at: expiry.expires_at,
            days_to_expiration,
            labels: expiry.labels.clone(),
            chain,
        })
    }
}

/// One cached snapshot together with the last time it was served.
struct CacheEntry {
    snapshot: SeriesSnapshot,
    last_access: Instant,
}

/// A bounded, least-recently-accessed cache of built snapshots.
///
/// Snapshots are expensive to build and cheap to reproduce, so the cache is a
/// pure latency optimisation: evicting an entry can never change what a client
/// sees, only how long it waits. That is what lets the bound be small and the
/// eviction policy simple.
///
/// Keyed by `(simulation, step)` so one simulation's working set cannot evict
/// another's wholesale, and so #48 can drop every entry belonging to a
/// simulation that has gone away.
pub(crate) struct SnapshotCache {
    entries: HashMap<(Uuid, usize), CacheEntry>,
    capacity: usize,
}

impl Default for SnapshotCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotCache {
    /// Creates a cache bounded by the documented default.
    ///
    /// The bound is configuration, not a constant of the domain: the service
    /// passes the operator's value through [`SnapshotCache::with_capacity`].
    /// This constructor exists for tests and for a caller with nothing to say
    /// about it.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_CACHED_SNAPSHOTS)
    }

    /// Creates a cache with an explicit bound.
    ///
    /// A capacity of zero is raised to one: a cache that can hold nothing would
    /// make every `insert` a no-op and every `get` a miss, which is a
    /// misconfiguration rather than an intent.
    #[must_use]
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            capacity: capacity.max(1),
        }
    }

    /// The number of snapshots currently held.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "clippy's len_without_is_empty requires this alongside `len`"
        )
    )]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The bound this cache enforces.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the configured bound, asserted by the tests that pin the \
                      zero-capacity floor"
        )
    )]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the cached snapshot for `(simulation, step)`, refreshing its
    /// recency so an actively-walked simulation survives the bound.
    pub(crate) fn get(&mut self, simulation: Uuid, step: usize) -> Option<&SeriesSnapshot> {
        let entry = self.entries.get_mut(&(simulation, step))?;
        entry.last_access = Instant::now();
        Some(&entry.snapshot)
    }

    /// Inserts a snapshot, evicting the least recently accessed entries first.
    ///
    /// Eviction runs down to `capacity - 1` *before* the insert, so the key
    /// being inserted can never be its own victim and the cache never exceeds
    /// its bound afterwards.
    pub(crate) fn insert(&mut self, simulation: Uuid, snapshot: SeriesSnapshot) {
        let key = (simulation, snapshot.step);
        self.entries.remove(&key);
        // `capacity` is raised to at least one in the constructor, so `- 1`
        // cannot underflow. No saturating arithmetic: the rules forbid it, and
        // here it would hide a constructor that stopped enforcing the floor.
        debug_assert!(
            self.capacity >= 1,
            "capacity is floored at one on construction"
        );
        self.evict_to(self.capacity - 1);
        self.entries.insert(
            key,
            CacheEntry {
                snapshot,
                last_access: Instant::now(),
            },
        );
    }

    /// Drops every entry belonging to `simulation`, returning how many went.
    ///
    /// This is what a deleted, completed or expired simulation triggers, so a
    /// heavyweight snapshot cannot outlive the session it belongs to. #48 wires
    /// it to the store's cleanup.
    pub(crate) fn evict_simulation(&mut self, simulation: Uuid) -> usize {
        let before = self.entries.len();
        self.entries.retain(|(id, _), _| *id != simulation);
        before - self.entries.len()
    }

    /// Evicts least-recently-accessed entries until at most `max` remain.
    fn evict_to(&mut self, max: usize) {
        while self.entries.len() > max {
            let victim = self
                .entries
                .iter()
                // The key breaks an `Instant` tie, so eviction does not depend
                // on the map's randomised iteration order. Nothing served
                // depends on which entry goes — a rebuild is identical — but a
                // deterministic victim keeps the cache's behaviour reproducible
                // under test.
                .min_by_key(|(key, entry)| (entry.last_access, **key))
                .map(|(key, _)| *key);
            match victim {
                Some(key) => {
                    self.entries.remove(&key);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
    use crate::api::rest::requests_v2::CreateSimulationRequest;
    use crate::session::{ExpiryRule, ExpiryRuleKind};
    use chrono::{TimeZone, Weekday};

    /// ADR 0001 §14's reference configuration: one rolling 0DTE, three
    /// Monday/Wednesday/Friday weeklies, twelve last-Friday monthlies, all at
    /// 17:00 New York.
    fn reference_schedules() -> Vec<ExpiryRule> {
        vec![
            rule("zero_dte", ExpiryRuleKind::Daily, 1),
            rule(
                "weeklies",
                ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Wed, Weekday::Fri]),
                3,
            ),
            rule(
                "monthlies",
                ExpiryRuleKind::Monthly {
                    weekday: Weekday::Fri,
                },
                12,
            ),
        ]
    }

    /// Twelve last-Friday monthlies, the rule the isolation test watches.
    pub(super) fn monthly_rule() -> ExpiryRule {
        rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            12,
        )
    }

    /// One rolling 0DTE — the nearest expiration, so it is priced first.
    pub(super) fn zero_dte_rule() -> ExpiryRule {
        rule("zero_dte", ExpiryRuleKind::Daily, 1)
    }

    /// The reference configuration with a chosen schedule, for tests in sibling
    /// modules.
    pub(super) fn parameters_with_rules(schedules: Vec<ExpiryRule>) -> SimulationParametersV2 {
        parameters(request(2, schedules))
    }

    fn rule(id: &str, kind: ExpiryRuleKind, count: usize) -> ExpiryRule {
        match ExpiryRule::new(id, kind, count) {
            Ok(rule) => rule,
            Err(error) => panic!("the test rule must be valid: {error}"),
        }
    }

    fn request(steps: usize, schedules: Vec<ExpiryRule>) -> CreateSimulationRequest {
        let start_at = match Utc.with_ymd_and_hms(2026, 1, 5, 14, 30, 0).single() {
            Some(instant) => instant,
            None => panic!("the test instant must be valid"),
        };

        CreateSimulationRequest {
            symbol: "SPX".to_string(),
            steps,
            start_at: Some(start_at),
            step_interval_seconds: Some(86_400),
            timezone: "America/New_York".to_string(),
            calendar: None,
            expiration_time: "17:00".to_string(),
            schedules,
            initial_price: 5000.0,
            volatility: 0.18,
            risk_free_rate: 0.04,
            dividend_yield: 0.0,
            method: ApiWalkType::Brownian {
                dt: 1.0 / 252.0,
                drift: 0.0,
                volatility: 0.18,
            },
            time_frame: ApiTimeFrame::Day,
            // A narrow ladder keeps sixteen chains a step affordable in tests.
            chain_size: Some(3),
            strike_interval: Some(25.0),
            skew_slope: None,
            smile_curve: None,
            spread: Some(0.02),
            seed: Some(42),
        }
    }

    /// The reference configuration, for tests in sibling modules.
    pub(super) fn test_parameters() -> SimulationParametersV2 {
        parameters(request(2, reference_schedules()))
    }

    /// The reference configuration's factor tape, for tests in sibling modules.
    pub(super) fn test_tape(parameters: &SimulationParametersV2) -> FactorTape {
        tape(parameters)
    }

    /// The reference configuration under a different seed.
    pub(super) fn test_parameters_with_seed(seed: u64) -> SimulationParametersV2 {
        let mut request = request(2, reference_schedules());
        request.seed = Some(seed);
        parameters(request)
    }

    fn parameters(request: CreateSimulationRequest) -> SimulationParametersV2 {
        match SimulationParametersV2::try_from(request) {
            Ok(parameters) => parameters,
            Err(error) => panic!("the request must convert: {error}"),
        }
    }

    /// Builds a [`SeriesBuilder`], panicking on a validation the tests never
    /// intend to trip. The production path returns the error instead.
    pub(super) fn builder<'a>(
        parameters: &'a SimulationParametersV2,
        tape: &'a FactorTape,
    ) -> SeriesBuilder<'a> {
        match SeriesBuilder::new(parameters, tape) {
            Ok(builder) => builder,
            Err(error) => panic!("the reference parameters must validate: {error}"),
        }
    }

    fn tape(parameters: &SimulationParametersV2) -> FactorTape {
        match FactorTape::build(parameters, &parameters.method) {
            Ok(tape) => tape,
            Err(error) => panic!("the tape must build: {error}"),
        }
    }

    fn snapshot(
        parameters: &SimulationParametersV2,
        tape: &FactorTape,
        step: usize,
    ) -> SeriesSnapshot {
        match builder(parameters, tape).snapshot(step) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("the snapshot at step {step} must build: {error}"),
        }
    }

    /// The mid price of the at-the-money call, which is what the premium
    /// assertions compare.
    fn atm_call_mid(chain: &ExpiryChain) -> Positive {
        let atm = match chain.chain.atm_option_data() {
            Ok(data) => data,
            Err(error) => panic!("the chain must have an ATM strike: {error}"),
        };
        match atm.call_middle {
            Some(mid) => mid,
            None => panic!("the ATM call must have a mid price"),
        }
    }

    // ---- inventory --------------------------------------------------------

    /// The reference configuration yields every rule's slots at every step,
    /// with coincident expirations shared rather than duplicated.
    #[test]
    fn test_reference_configuration_satisfies_every_rule_at_every_step() {
        let parameters = parameters(request(12, reference_schedules()));
        let tape = tape(&parameters);

        for step in 0..tape.len() {
            let snapshot = snapshot(&parameters, &tape, step);

            for (rule_id, expected) in [("zero_dte", 1), ("weeklies", 3), ("monthlies", 12)] {
                assert_eq!(
                    snapshot.chains_for(rule_id).count(),
                    expected,
                    "step {step} lost inventory for {rule_id}"
                );
            }
            // Sixteen rule slots, but the shared dates collapse, so a snapshot
            // never carries more chains than slots and usually fewer.
            // A bound, not a dedup check: sixteen is the fixed sum of the rule
            // slots and the planner only ever collapses shared dates, so this
            // catches a snapshot that grew chains from nowhere. The collapse
            // itself is asserted at step zero below, where the coincidence is
            // known.
            assert!(
                snapshot.chains.len() <= 16,
                "step {step} priced {} chains, more than the sixteen rule slots",
                snapshot.chains.len()
            );
        }
    }

    /// At step zero the reference configuration collapses to fifteen chains:
    /// Monday's 0DTE and the first weekly are the same physical expiration,
    /// priced once and carrying both labels.
    #[test]
    fn test_coincident_expirations_are_priced_once_with_both_labels() {
        let parameters = parameters(request(1, reference_schedules()));
        let tape = tape(&parameters);

        let snapshot = snapshot(&parameters, &tape, 0);

        assert_eq!(snapshot.chains.len(), 15);
        let first = match snapshot.chains.first() {
            Some(chain) => chain,
            None => panic!("the snapshot must carry chains"),
        };
        assert_eq!(
            first.labels,
            vec!["weeklies".to_string(), "zero_dte".to_string()]
        );
    }

    /// Chains are ordered by expiration and never repeat an instant.
    #[test]
    fn test_chains_are_chronological_and_unique() {
        let parameters = parameters(request(3, reference_schedules()));
        let tape = tape(&parameters);

        let snapshot = snapshot(&parameters, &tape, 0);

        assert!(!snapshot.chains.is_empty());
        for pair in snapshot.chains.windows(2) {
            assert!(
                pair[0].expires_at < pair[1].expires_at,
                "chains must be strictly increasing in expiration"
            );
        }
    }

    /// Every chain shares the step's spot and base volatility but has its own
    /// expiration and fractional days-to-expiration.
    #[test]
    fn test_chains_share_the_step_state_and_differ_in_expiration() {
        let parameters = parameters(request(5, reference_schedules()));
        let tape = tape(&parameters);

        let snapshot = snapshot(&parameters, &tape, 2);
        let row = match tape.row(2) {
            Some(row) => row,
            None => panic!("the tape must have a row at step 2"),
        };

        assert_eq!(snapshot.spot, row.spot);
        assert_eq!(snapshot.base_volatility, row.base_volatility);
        assert_eq!(snapshot.simulated_at, row.simulated_at);

        let mut seen: Vec<DateTime<Utc>> = Vec::new();
        for chain in &snapshot.chains {
            assert!(chain.days_to_expiration > Positive::ZERO);
            assert!(!seen.contains(&chain.expires_at));
            seen.push(chain.expires_at);
        }
    }

    /// No chain is ever emitted at or past its expiration.
    #[test]
    fn test_an_expired_chain_is_never_emitted() {
        let parameters = parameters(request(20, reference_schedules()));
        let tape = tape(&parameters);

        for step in 0..tape.len() {
            let snapshot = snapshot(&parameters, &tape, step);
            for chain in &snapshot.chains {
                assert!(
                    chain.expires_at > snapshot.simulated_at,
                    "step {step} emitted an expired chain"
                );
            }
        }
    }

    /// Crossing the 0DTE's cutoff replaces it in the same snapshot, so there is
    /// no step with a missing rolling expiration.
    #[test]
    fn test_crossing_the_cutoff_replaces_the_expiry_without_a_gap() {
        // 17:00 New York is 22:00 UTC in January; a 12-hour step crosses it.
        let mut crossing = request(4, vec![rule("zero_dte", ExpiryRuleKind::Daily, 1)]);
        crossing.step_interval_seconds = Some(43_200);
        let parameters = parameters(crossing);
        let tape = tape(&parameters);

        let mut expirations = Vec::new();
        for step in 0..tape.len() {
            let snapshot = snapshot(&parameters, &tape, step);
            assert_eq!(
                snapshot.chains.len(),
                1,
                "step {step} must always carry exactly one 0DTE"
            );
            match snapshot.chains.first() {
                Some(chain) => expirations.push(chain.expires_at),
                None => panic!("step {step} lost its rolling expiration"),
            }
        }

        // The inventory rolled at least once across the horizon.
        let distinct: std::collections::BTreeSet<DateTime<Utc>> =
            expirations.iter().copied().collect();
        assert!(
            distinct.len() > 1,
            "the 0DTE must roll at least once over two simulated days"
        );
    }

    /// A multi-month horizon completes every step despite repeated rolls.
    #[test]
    fn test_a_long_horizon_builds_every_step() {
        let parameters = parameters(request(90, reference_schedules()));
        let tape = tape(&parameters);

        // Sampled rather than exhaustive: ninety full reference snapshots is
        // fifteen chains each, and the property under test is that no step
        // fails, not that every one is inspected.
        for step in [0, 1, 29, 45, 60, 89] {
            let snapshot = snapshot(&parameters, &tape, step);
            assert_eq!(snapshot.chains_for("monthlies").count(), 12);
            assert_eq!(snapshot.chains_for("weeklies").count(), 3);
        }
    }

    /// A step past the end of the tape is a typed error, not a panic.
    #[test]
    fn test_a_step_past_the_tape_is_not_found() {
        let parameters = parameters(request(3, reference_schedules()));
        let tape = tape(&parameters);

        match builder(&parameters, &tape).snapshot(3) {
            Err(ChainError::NotFound(message)) => assert!(message.contains("past the end")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // ---- pricing behaviour ------------------------------------------------

    /// A higher base volatility raises the representative ATM premium, all else
    /// equal.
    #[test]
    fn test_higher_base_volatility_raises_the_atm_premium() {
        let low = parameters(request(1, vec![rule("zero_dte", ExpiryRuleKind::Daily, 1)]));

        let mut high_request = request(1, vec![rule("zero_dte", ExpiryRuleKind::Daily, 1)]);
        high_request.volatility = 0.40;
        high_request.method = ApiWalkType::Brownian {
            dt: 1.0 / 252.0,
            drift: 0.0,
            volatility: 0.40,
        };
        let high = parameters(high_request);

        let low_tape = tape(&low);
        let high_tape = tape(&high);
        let low_snapshot = snapshot(&low, &low_tape, 0);
        let high_snapshot = snapshot(&high, &high_tape, 0);

        let low_chain = match low_snapshot.chains.first() {
            Some(chain) => chain,
            None => panic!("the snapshot must carry a chain"),
        };
        let high_chain = match high_snapshot.chains.first() {
            Some(chain) => chain,
            None => panic!("the snapshot must carry a chain"),
        };
        assert_eq!(
            low_chain.expires_at, high_chain.expires_at,
            "the comparison is only meaningful at the same expiration"
        );
        assert!(
            atm_call_mid(high_chain) > atm_call_mid(low_chain),
            "a higher base volatility must raise the ATM premium"
        );
    }

    /// A nearer expiration carries less extrinsic value than a farther one at
    /// the same step.
    #[test]
    fn test_a_nearer_expiration_carries_less_extrinsic_value() {
        let parameters = parameters(request(1, reference_schedules()));
        let tape = tape(&parameters);

        let snapshot = snapshot(&parameters, &tape, 0);
        let nearest = match snapshot.chains.first() {
            Some(chain) => chain,
            None => panic!("the snapshot must carry chains"),
        };
        let farthest = match snapshot.chains.last() {
            Some(chain) => chain,
            None => panic!("the snapshot must carry chains"),
        };

        assert!(nearest.days_to_expiration < farthest.days_to_expiration);
        assert!(
            atm_call_mid(nearest) < atm_call_mid(farthest),
            "the nearer expiration must carry less extrinsic value"
        );
    }

    /// Per-strike implied volatility is shaped by skew and smile rather than
    /// copying the base.
    #[test]
    fn test_skew_and_smile_shape_the_per_strike_volatility() {
        let mut shaped = request(1, vec![rule("zero_dte", ExpiryRuleKind::Daily, 1)]);
        shaped.chain_size = Some(9);
        shaped.skew_slope = Some(-0.3);
        shaped.smile_curve = Some(0.5);
        let parameters = parameters(shaped);
        let tape = tape(&parameters);

        let snapshot = snapshot(&parameters, &tape, 0);
        let chain = match snapshot.chains.first() {
            Some(chain) => chain,
            None => panic!("the snapshot must carry a chain"),
        };

        let volatilities: Vec<Positive> = chain
            .chain
            .iter()
            .map(|data| data.implied_volatility)
            .collect();
        assert!(
            volatilities.len() > 2,
            "the ladder must expose per-strike volatilities"
        );
        let distinct: std::collections::BTreeSet<String> =
            volatilities.iter().map(ToString::to_string).collect();
        assert!(
            distinct.len() > 1,
            "skew and smile must vary the volatility across strikes"
        );
    }

    // ---- determinism ------------------------------------------------------

    /// The same step rebuilds to an identical snapshot, which is what makes
    /// cache eviction unobservable.
    #[test]
    fn test_rebuilding_a_step_yields_an_identical_snapshot() {
        let parameters = parameters(request(4, reference_schedules()));
        let tape = tape(&parameters);
        let builder = builder(&parameters, &tape);

        match (builder.snapshot(2), builder.snapshot(2)) {
            (Ok(first), Ok(second)) => assert_eq!(first, second),
            (first, second) => panic!("both builds must succeed: {first:?} {second:?}"),
        }
    }

    /// Two simulations with the same seed produce identical snapshot tapes,
    /// compared over every field a client can see — timestamps, expirations,
    /// labels, spot, base volatility, and the chains themselves.
    #[test]
    fn test_same_seed_produces_an_identical_snapshot_tape() {
        let first_parameters = parameters(request(6, reference_schedules()));
        let second_parameters = parameters(request(6, reference_schedules()));
        let first_tape = tape(&first_parameters);
        let second_tape = tape(&second_parameters);

        for step in 0..first_tape.len() {
            assert_eq!(
                snapshot(&first_parameters, &first_tape, step),
                snapshot(&second_parameters, &second_tape, step),
                "step {step} diverged under the same seed"
            );
        }
    }

    /// A different seed produces a different snapshot tape.
    #[test]
    fn test_a_different_seed_produces_a_different_snapshot_tape() {
        let baseline = parameters(request(6, reference_schedules()));
        let mut other_request = request(6, reference_schedules());
        other_request.seed = Some(43);
        let other = parameters(other_request);

        let baseline_tape = tape(&baseline);
        let other_tape = tape(&other);

        let baseline_last = snapshot(&baseline, &baseline_tape, 5);
        let other_last = snapshot(&other, &other_tape, 5);

        assert_ne!(baseline_last, other_last);
        // The expirations are schedule-driven and therefore shared; the market
        // state is what diverges.
        let baseline_expiries: Vec<DateTime<Utc>> =
            baseline_last.chains.iter().map(|c| c.expires_at).collect();
        let other_expiries: Vec<DateTime<Utc>> =
            other_last.chains.iter().map(|c| c.expires_at).collect();
        assert_eq!(baseline_expiries, other_expiries);
        assert_ne!(baseline_last.spot, other_last.spot);
    }

    /// Looking a chain up by its expiration finds the one the planner produced.
    #[test]
    fn test_a_chain_can_be_found_by_its_expiration() {
        let parameters = parameters(request(1, reference_schedules()));
        let tape = tape(&parameters);
        let snapshot = snapshot(&parameters, &tape, 0);

        let target = match snapshot.chains.first() {
            Some(chain) => chain.expires_at,
            None => panic!("the snapshot must carry chains"),
        };

        match snapshot.chain_at(target) {
            Some(found) => assert_eq!(found.expires_at, target),
            None => panic!("the chain must be findable by its expiration"),
        }
        assert!(
            snapshot
                .chain_at(target - chrono::Duration::days(3650))
                .is_none()
        );
    }

    // ---- the snapshot cache -----------------------------------------------

    fn cached_snapshot(step: usize) -> SeriesSnapshot {
        let simulated_at = match Utc.with_ymd_and_hms(2026, 1, 5, 14, 30, 0).single() {
            Some(instant) => instant,
            None => panic!("the test instant must be valid"),
        };
        SeriesSnapshot {
            step,
            simulated_at,
            spot: Positive::ONE,
            base_volatility: Positive::ONE,
            chains: Vec::new(),
        }
    }

    /// A cached snapshot is returned as stored.
    #[test]
    fn test_the_cache_returns_what_it_stored() {
        let mut cache = SnapshotCache::with_capacity(4);
        let simulation = Uuid::new_v4();

        assert!(cache.is_empty());
        cache.insert(simulation, cached_snapshot(0));

        assert_eq!(cache.len(), 1);
        match cache.get(simulation, 0) {
            Some(snapshot) => assert_eq!(snapshot.step, 0),
            None => panic!("the cache must return what it stored"),
        }
        assert!(cache.get(simulation, 1).is_none());
    }

    /// The cache never exceeds its bound, and evicts the least recently
    /// accessed entry first.
    #[test]
    fn test_the_cache_evicts_the_least_recently_accessed_entry() {
        let mut cache = SnapshotCache::with_capacity(2);
        let simulation = Uuid::new_v4();

        cache.insert(simulation, cached_snapshot(0));
        cache.insert(simulation, cached_snapshot(1));
        // Touching step 0 makes step 1 the least recently accessed.
        assert!(cache.get(simulation, 0).is_some());
        cache.insert(simulation, cached_snapshot(2));

        assert_eq!(cache.len(), 2, "the cache must respect its bound");
        assert!(
            cache.get(simulation, 0).is_some(),
            "the touched entry stays"
        );
        assert!(cache.get(simulation, 1).is_none(), "the idle entry goes");
        assert!(cache.get(simulation, 2).is_some());
    }

    /// One simulation's entries do not evict another's wholesale, and a
    /// simulation's entries can be dropped together.
    #[test]
    fn test_a_simulations_entries_can_be_evicted_together() {
        let mut cache = SnapshotCache::with_capacity(8);
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();

        cache.insert(first, cached_snapshot(0));
        cache.insert(first, cached_snapshot(1));
        cache.insert(second, cached_snapshot(0));

        assert_eq!(cache.evict_simulation(first), 2);
        assert_eq!(cache.len(), 1);
        assert!(cache.get(first, 0).is_none());
        assert!(cache.get(second, 0).is_some());
        // Evicting a simulation that has nothing cached is not an error.
        assert_eq!(cache.evict_simulation(first), 0);
    }

    /// Re-inserting the same key replaces rather than duplicating.
    #[test]
    fn test_reinserting_a_key_replaces_the_entry() {
        let mut cache = SnapshotCache::with_capacity(4);
        let simulation = Uuid::new_v4();

        cache.insert(simulation, cached_snapshot(7));
        cache.insert(simulation, cached_snapshot(7));

        assert_eq!(cache.len(), 1);
    }

    /// A capacity of zero is raised to one rather than making the cache inert.
    #[test]
    fn test_a_zero_capacity_is_raised_to_one() {
        let mut cache = SnapshotCache::with_capacity(0);
        let simulation = Uuid::new_v4();

        assert_eq!(cache.capacity(), 1);
        cache.insert(simulation, cached_snapshot(0));
        assert_eq!(cache.len(), 1);
    }

    /// Eviction is unobservable: a rebuilt snapshot equals the evicted one.
    ///
    /// This is what lets the bound be small — the cache is a latency
    /// optimisation, never a source of truth.
    #[test]
    fn test_eviction_is_unobservable() {
        let parameters = parameters(request(3, reference_schedules()));
        let tape = tape(&parameters);
        let simulation = Uuid::new_v4();
        let mut cache = SnapshotCache::with_capacity(1);

        let original = snapshot(&parameters, &tape, 1);
        cache.insert(simulation, original.clone());
        // Push it out.
        cache.insert(simulation, snapshot(&parameters, &tape, 2));
        assert!(cache.get(simulation, 1).is_none());

        assert_eq!(snapshot(&parameters, &tape, 1), original);
    }
}

#[cfg(test)]
mod reproducibility_tests {
    use super::tests::*;
    use super::*;

    /// Everything about a snapshot that the v2 API surfaces to a client.
    ///
    /// Deliberately explicit rather than `PartialEq` on the whole snapshot: an
    /// `OptionChain` also carries a calendar string upstream stamps from the
    /// host clock (see the module docs), which is not part of the v2 contract
    /// and would make an otherwise-sound comparison fail across midnight.
    ///
    /// What is compared here is exactly what #47 will put on the wire.
    fn surfaced_content(snapshot: &SeriesSnapshot) -> String {
        let mut rendered = format!(
            "step={} at={} spot={} iv={}",
            snapshot.step, snapshot.simulated_at, snapshot.spot, snapshot.base_volatility
        );
        for chain in &snapshot.chains {
            rendered.push_str(&format!(
                "\n  expires={} dte={} labels={:?}",
                chain.expires_at, chain.days_to_expiration, chain.labels
            ));
            for data in chain.chain.iter() {
                rendered.push_str(&format!(
                    "\n    k={} iv={} cb={:?} ca={:?} cm={:?} pb={:?} pa={:?} pm={:?} dc={:?} dp={:?} g={:?}",
                    data.strike_price,
                    data.implied_volatility,
                    data.call_bid,
                    data.call_ask,
                    data.call_middle,
                    data.put_bid,
                    data.put_ask,
                    data.put_middle,
                    data.delta_call,
                    data.delta_put,
                    data.gamma,
                ));
            }
        }
        rendered
    }

    /// Adding an expiration rule does not disturb the chains of the rules that
    /// were already there.
    ///
    /// This is ADR 0001 §8's isolation property, at the level where it could
    /// actually break: chain building draws no randomness, so a client may add,
    /// remove or reorder rules and still compare two runs. The 0DTE rule is
    /// added deliberately — chains are built in chronological order, so it is
    /// built **first**, and anything it consumed would shift every monthly
    /// chain built after it.
    #[test]
    fn test_adding_a_rule_leaves_the_other_chains_untouched() {
        let baseline = parameters_with_rules(vec![monthly_rule()]);
        let extended = parameters_with_rules(vec![monthly_rule(), zero_dte_rule()]);

        let baseline_tape = test_tape(&baseline);
        let extended_tape = test_tape(&extended);

        let baseline_snapshot = match builder(&baseline, &baseline_tape).snapshot(1) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("the baseline snapshot must build: {error}"),
        };
        let extended_snapshot = match builder(&extended, &extended_tape).snapshot(1) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("the extended snapshot must build: {error}"),
        };

        assert_eq!(
            baseline_snapshot.spot, extended_snapshot.spot,
            "the price path must not depend on the schedule"
        );

        let monthlies = |snapshot: &SeriesSnapshot| -> Vec<ExpiryChain> {
            snapshot
                .chains
                .iter()
                .filter(|chain| chain.labels.iter().any(|label| label == "monthlies"))
                .cloned()
                .collect()
        };

        let before = monthlies(&baseline_snapshot);
        let after = monthlies(&extended_snapshot);
        assert!(
            !before.is_empty(),
            "the baseline must price a monthly chain"
        );
        assert_eq!(
            before, after,
            "adding a 0DTE rule must not change the monthly chains"
        );
    }

    /// The same seed reproduces every value the v2 surface exposes: timestamps,
    /// expirations, labels, spot, base and per-strike volatility, quotes and
    /// Greeks.
    #[test]
    fn test_same_seed_reproduces_every_surfaced_value() {
        let first = test_parameters();
        let second = test_parameters();
        let first_tape = test_tape(&first);
        let second_tape = test_tape(&second);

        for step in 0..first_tape.len() {
            let left = match builder(&first, &first_tape).snapshot(step) {
                Ok(snapshot) => snapshot,
                Err(error) => panic!("the snapshot must build: {error}"),
            };
            let right = match builder(&second, &second_tape).snapshot(step) {
                Ok(snapshot) => snapshot,
                Err(error) => panic!("the snapshot must build: {error}"),
            };

            assert_eq!(
                surfaced_content(&left),
                surfaced_content(&right),
                "step {step} diverged under the same seed"
            );
        }
    }

    /// Rebuilding a step reproduces every surfaced value, which is what makes
    /// an eviction from the snapshot cache unobservable.
    #[test]
    fn test_rebuilding_reproduces_every_surfaced_value() {
        let parameters = test_parameters();
        let tape = test_tape(&parameters);
        let builder = builder(&parameters, &tape);

        match (builder.snapshot(1), builder.snapshot(1)) {
            (Ok(first), Ok(second)) => {
                assert_eq!(surfaced_content(&first), surfaced_content(&second));
            }
            (first, second) => panic!("both builds must succeed: {first:?} {second:?}"),
        }
    }

    /// A different seed changes the surfaced market state while leaving the
    /// schedule-driven expirations alone.
    #[test]
    fn test_a_different_seed_changes_the_surfaced_market_state() {
        let baseline = test_parameters();
        let other = test_parameters_with_seed(43);
        let baseline_tape = test_tape(&baseline);
        let other_tape = test_tape(&other);

        let left = match builder(&baseline, &baseline_tape).snapshot(1) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("the snapshot must build: {error}"),
        };
        let right = match builder(&other, &other_tape).snapshot(1) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("the snapshot must build: {error}"),
        };

        assert_ne!(surfaced_content(&left), surfaced_content(&right));
        let left_expiries: Vec<DateTime<Utc>> = left.chains.iter().map(|c| c.expires_at).collect();
        let right_expiries: Vec<DateTime<Utc>> =
            right.chains.iter().map(|c| c.expires_at).collect();
        assert_eq!(
            left_expiries, right_expiries,
            "expirations come from the schedule, not the seed"
        );
    }
}
