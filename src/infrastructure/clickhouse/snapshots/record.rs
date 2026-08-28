//! The typed records the snapshot repository persists and returns.
//!
//! These are **infrastructure** types, not the domain snapshot. `src/domain` is
//! a private module and `SeriesSnapshot`/`ExpiryChain` are `pub(crate)` inside
//! it, so this layer cannot name them — and should not: a storage record has to
//! survive a schema migration and a driver upgrade without dragging the
//! simulation's internals along. The session layer, which can see both, owns
//! the conversion from a built snapshot into a [`SnapshotRecord`].
//!
//! Everything here stays typed — `Positive`, `Decimal`, `DateTime<Utc>`,
//! `Uuid` — up to the serialization boundary in
//! [`super::model`], which is the single place a value becomes a ClickHouse
//! column.
//!
//! # What identifies a snapshot
//!
//! `(simulation, generation, step)`. Deliberately **not** called a version: a
//! v2 session already has a `version`, the optimistic-concurrency revision that
//! changes on every advance and travels on the wire. Feeding that number in here
//! would fragment one tape across hundreds of coordinates and defeat the whole
//! retry-safe deduplication, so the field carries the name of what it actually
//! is — see [`CURRENT_SNAPSHOT_GENERATION`].
//!
//! Everything in this module treats two writes of the same triple as two
//! attempts at the *same* immutable snapshot.

use crate::utils::ChainError;
use chrono::{DateTime, Utc};
use optionstratlib::greeks::GreeksSnapshot;
use positive::Positive;
use rust_decimal::Decimal;
use std::fmt;
use uuid::Uuid;

/// The generation every snapshot this build produces is written under.
///
/// A snapshot's coordinate is `(simulation, generation, step)`, and the
/// invariant the storage layer rests on is that the coordinate determines the
/// snapshot: two writes of it are two attempts at the same immutable rows, and
/// the second silently replaces the first.
///
/// The generation is what keeps that true across time. Bump it when a change to
/// this crate would make the *same* simulation and step produce *different*
/// values — a pricing fix, a walk-kernel resync, a change to the schedule
/// projection. Rows written under the old generation then stay addressable and
/// are never mixed with the new ones, rather than being half-overwritten by a
/// replay that no longer agrees with them.
///
/// A caller materialising snapshots passes this constant; a caller reading them
/// back passes the generation the rows were written under, which for everything
/// this build wrote is this one. It lives here, next to the records, so an
/// external consumer of the `pub` read API has a name to pass instead of a
/// hardcoded literal.
///
/// # History
///
/// - `1` — the shape 0.2.0 shipped. A historical walk priced every step at the
///   constant `volatility` the request supplied.
/// - `2` — a historical walk prices each step at a causal realized-volatility
///   estimate over the observations up to it. The price path is unchanged and
///   every other walk type is unaffected, but the premiums of a historical
///   simulation are not, so its rows are a different tape under the same
///   coordinates. Generation 1 rows stay in place until their retention expires
///   and are simply not read by this build.
pub const CURRENT_SNAPSHOT_GENERATION: u64 = 2;

/// The namespace every deterministic `snapshot_id` is derived under.
///
/// The ASCII bytes of `ocs-snapshot-v1`, padded to sixteen. Fixed forever: a
/// new namespace would give every already-persisted snapshot a second identity,
/// and every retry a duplicate.
const SNAPSHOT_NAMESPACE: Uuid = Uuid::from_u128(0x6f_63_73_2d_73_6e_61_70_73_68_6f_74_2d_76_31_00);

/// The deterministic identity of one materialised snapshot.
///
/// A UUID v5 over `(simulation, generation, step)` — the same primitive
/// [`crate::utils::UuidGenerator`] uses (`Uuid::new_v5`), applied to a
/// coordinate instead of to a counter, because a retry has to land on the id
/// the first attempt used. `UuidGenerator` itself cannot serve this: its name
/// is an internal counter, so two processes replaying the same step would
/// disagree.
///
/// # Examples
///
/// ```
/// use optionchain_simulator::infrastructure::{CURRENT_SNAPSHOT_GENERATION, snapshot_id};
/// use uuid::Uuid;
///
/// let simulation = Uuid::nil();
/// let generation = CURRENT_SNAPSHOT_GENERATION;
/// assert_eq!(
///     snapshot_id(simulation, generation, 7),
///     snapshot_id(simulation, generation, 7)
/// );
/// assert_ne!(
///     snapshot_id(simulation, generation, 7),
///     snapshot_id(simulation, generation + 1, 7)
/// );
/// ```
#[must_use]
#[inline]
pub fn snapshot_id(simulation: Uuid, generation: u64, step: usize) -> Uuid {
    // The separators matter: without them `(1, 23)` and `(12, 3)` would hash
    // to the same name.
    let name = format!("{simulation}:{generation}:{step}");
    Uuid::new_v5(&SNAPSHOT_NAMESPACE, name.as_bytes())
}

/// Which side of a strike a query is about.
///
/// Both sides live in one persisted row — they share the strike, the implied
/// volatility and the gamma — so the side is a projection at read time, never a
/// storage dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ContractSide {
    /// The call side.
    Call,
    /// The put side.
    Put,
}

impl ContractSide {
    /// The side's lowercase name, for logs and query construction.
    #[must_use]
    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            ContractSide::Call => "call",
            ContractSide::Put => "put",
        }
    }
}

impl fmt::Display for ContractSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One strike of one expiration, both sides.
///
/// Mirrors upstream `OptionData` as far as the v2 surface goes: the quoted
/// prices, the per-side delta, the shared gamma mirror, and since issue #74 the
/// full twelve-value greek snapshot per style. A new upstream greek is a schema
/// change, deliberately — it lands as a compile error at the `GreeksSnapshot`
/// literal that rebuilds a stored row.
///
/// `#[non_exhaustive]`: the greek columns were added to a struct with public
/// fields, which broke every downstream struct literal. Marking it now means
/// the next column does not.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct QuoteRow {
    /// The strike price.
    pub strike: Positive,
    /// The per-strike implied volatility, after skew and smile.
    pub implied_volatility: Positive,
    /// The call bid, when upstream quoted one.
    pub call_bid: Option<Positive>,
    /// The call ask, when upstream quoted one.
    pub call_ask: Option<Positive>,
    /// The call mid, when upstream quoted one.
    pub call_mid: Option<Positive>,
    /// The put bid, when upstream quoted one.
    pub put_bid: Option<Positive>,
    /// The put ask, when upstream quoted one.
    pub put_ask: Option<Positive>,
    /// The put mid, when upstream quoted one.
    pub put_mid: Option<Positive>,
    /// The call delta.
    pub delta_call: Option<Decimal>,
    /// The put delta.
    pub delta_put: Option<Decimal>,
    /// Gamma, shared by the call and the put.
    ///
    /// Upstream's convenience mirror, computed independently of the snapshots
    /// below and defined at expiry and at zero volatility where they are not.
    /// Kept alongside [`Self::greeks_call`] / [`Self::greeks_put`] rather than
    /// replaced by them, and it is what a row written before issue #74 carries.
    pub gamma: Option<Decimal>,
    /// The call's full twelve-value greek snapshot, per one long contract.
    ///
    /// `None` means the strike had no computable snapshot — a degenerate strike
    /// — or that the row predates the greek columns. Never a stand-in for zero.
    pub greeks_call: Option<GreeksSnapshot>,
    /// The put's full twelve-value greek snapshot, per one long contract.
    ///
    /// Same convention as [`Self::greeks_call`].
    pub greeks_put: Option<GreeksSnapshot>,
}

impl QuoteRow {
    /// Creates a quote row with no quotes and no Greeks.
    ///
    /// The builder-style setters exist for tests and for a caller that has only
    /// part of a chain; production callers construct the struct literally from
    /// an upstream `OptionData`.
    #[must_use]
    pub fn new(strike: Positive, implied_volatility: Positive) -> Self {
        Self {
            strike,
            implied_volatility,
            call_bid: None,
            call_ask: None,
            call_mid: None,
            put_bid: None,
            put_ask: None,
            put_mid: None,
            delta_call: None,
            delta_put: None,
            gamma: None,
            greeks_call: None,
            greeks_put: None,
        }
    }

    /// Sets the call side.
    #[must_use = "builders do nothing unless the value is used"]
    pub fn with_call(
        mut self,
        bid: Option<Positive>,
        ask: Option<Positive>,
        mid: Option<Positive>,
        delta: Option<Decimal>,
    ) -> Self {
        self.call_bid = bid;
        self.call_ask = ask;
        self.call_mid = mid;
        self.delta_call = delta;
        self
    }

    /// Sets the put side.
    #[must_use = "builders do nothing unless the value is used"]
    pub fn with_put(
        mut self,
        bid: Option<Positive>,
        ask: Option<Positive>,
        mid: Option<Positive>,
        delta: Option<Decimal>,
    ) -> Self {
        self.put_bid = bid;
        self.put_ask = ask;
        self.put_mid = mid;
        self.delta_put = delta;
        self
    }

    /// Sets the shared gamma.
    #[must_use = "builders do nothing unless the value is used"]
    pub fn with_gamma(mut self, gamma: Option<Decimal>) -> Self {
        self.gamma = gamma;
        self
    }

    /// Sets the call's greek snapshot.
    ///
    /// One setter per style rather than one taking both: two arguments of the
    /// same type, positionally, is exactly the shape that lets a transposed
    /// call site compile and store each side's greeks under the other's name.
    #[must_use = "builders do nothing unless the value is used"]
    pub fn with_greeks_call(mut self, greeks: Option<GreeksSnapshot>) -> Self {
        self.greeks_call = greeks;
        self
    }

    /// Sets the put's greek snapshot. See [`QuoteRow::with_greeks_call`].
    #[must_use = "builders do nothing unless the value is used"]
    pub fn with_greeks_put(mut self, greeks: Option<GreeksSnapshot>) -> Self {
        self.greeks_put = greeks;
        self
    }
}

/// One live expiration at one step, with every strike it quotes.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpirationRecord {
    /// The absolute expiration instant, in UTC. The authoritative expiration:
    /// it comes from the planner, never from a chain's calendar string.
    pub expires_at: DateTime<Utc>,
    /// Fractional days remaining, from the same `(simulated_at, expires_at)`
    /// pair the planner used.
    pub days_to_expiration: Positive,
    /// Every schedule rule this expiration satisfies, sorted. A date claimed by
    /// two rules appears once, with both labels.
    pub labels: Vec<String>,
    /// The strikes, ascending.
    pub quotes: Vec<QuoteRow>,
}

impl ExpirationRecord {
    /// Creates an expiration record.
    #[must_use]
    pub fn new(
        expires_at: DateTime<Utc>,
        days_to_expiration: Positive,
        labels: Vec<String>,
        quotes: Vec<QuoteRow>,
    ) -> Self {
        Self {
            expires_at,
            days_to_expiration,
            labels,
            quotes,
        }
    }
}

/// One materialised simulation step: the whole simulated market at one instant.
///
/// This is the unit the repository persists and returns. Reconstructing it from
/// storage must yield exactly what was written — the same ordering, the same
/// decimals, the same `None`s — which is what makes the persisted tape usable
/// as the source for #49's exports instead of a replay.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotRecord {
    /// The simulation this snapshot belongs to.
    pub simulation: Uuid,
    /// The generation the snapshot was produced under —
    /// [`CURRENT_SNAPSHOT_GENERATION`] for anything this build materialises.
    /// Not the session's `version`, which is its CAS revision.
    pub generation: u64,
    /// The 0-based step.
    pub step: usize,
    /// The simulated instant, from the factor tape.
    pub simulated_at: DateTime<Utc>,
    /// The ticker symbol every chain in the snapshot prices.
    pub symbol: String,
    /// The underlying price shared by every chain.
    pub spot: Positive,
    /// The base implied volatility shared by every chain, before skew and smile
    /// shape it per strike.
    pub base_volatility: Positive,
    /// The live expirations, ordered by `expires_at` ascending.
    pub expirations: Vec<ExpirationRecord>,
}

impl SnapshotRecord {
    /// Creates a snapshot record.
    ///
    /// The fields are public as well, so the session layer can build one
    /// straight from a domain snapshot without an intermediate copy. Nothing is
    /// validated here — [`SnapshotRecord::validate`] runs at the moment of
    /// writing, which is the only place an invalid record could do damage.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "a storage record is wide by nature"
    )]
    pub fn new(
        simulation: Uuid,
        generation: u64,
        step: usize,
        simulated_at: DateTime<Utc>,
        symbol: String,
        spot: Positive,
        base_volatility: Positive,
        expirations: Vec<ExpirationRecord>,
    ) -> Self {
        Self {
            simulation,
            generation,
            step,
            simulated_at,
            symbol,
            spot,
            base_volatility,
            expirations,
        }
    }

    /// This snapshot's deterministic identity.
    #[must_use]
    #[inline]
    pub fn snapshot_id(&self) -> Uuid {
        snapshot_id(self.simulation, self.generation, self.step)
    }

    /// How many quote rows this snapshot writes.
    ///
    /// The number the completion marker carries and every read verifies.
    #[must_use]
    pub fn quote_count(&self) -> usize {
        self.expirations
            .iter()
            .map(|expiration| expiration.quotes.len())
            .sum()
    }

    /// Checks the invariants the storage layer depends on.
    ///
    /// The fields are public, so a caller can assemble a record that never
    /// passed through a domain snapshot. Two properties matter enough to
    /// enforce before anything is written:
    ///
    /// * **Ordering.** Expirations ascend and strikes ascend within them. Reads
    ///   return rows in exactly this order, so a record written unordered would
    ///   come back reordered and fail its own round-trip.
    /// * **Uniqueness.** `(expires_at, strike)` identifies a row inside a
    ///   snapshot, and it is the deduplication key. Two rows sharing it would
    ///   collapse into one on merge, silently losing a strike.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming the offending field.
    pub fn validate(&self) -> Result<(), ChainError> {
        if self.symbol.trim().is_empty() {
            return Err(ChainError::Validation {
                field: "symbol".to_string(),
                reason: "must not be empty".to_string(),
            });
        }

        let mut previous_expiry: Option<DateTime<Utc>> = None;
        for expiration in &self.expirations {
            if let Some(previous) = previous_expiry
                && expiration.expires_at <= previous
            {
                return Err(ChainError::Validation {
                    field: "expirations".to_string(),
                    reason: format!(
                        "must be strictly ascending by expires_at, got {} after {}",
                        expiration.expires_at, previous
                    ),
                });
            }
            previous_expiry = Some(expiration.expires_at);

            let mut previous_strike: Option<Positive> = None;
            for quote in &expiration.quotes {
                if let Some(previous) = previous_strike
                    && quote.strike <= previous
                {
                    return Err(ChainError::Validation {
                        field: "quotes".to_string(),
                        reason: format!(
                            "strikes must be strictly ascending within {}, got {} after {}",
                            expiration.expires_at, quote.strike, previous
                        ),
                    });
                }
                previous_strike = Some(quote.strike);
            }
        }

        Ok(())
    }
}

/// One `(expiration, strike, side)` at one step: a row of a contract's history.
///
/// What `contract_series` returns, ordered by simulated time — the shape a
/// frontend chart or a backtest consumes.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ContractQuote {
    /// The step this quote came from.
    pub step: usize,
    /// The simulated instant of that step.
    pub simulated_at: DateTime<Utc>,
    /// The contract's absolute expiration.
    pub expires_at: DateTime<Utc>,
    /// Fractional days remaining at this step.
    pub days_to_expiration: Positive,
    /// The strike.
    pub strike: Positive,
    /// Which side these quotes describe.
    pub side: ContractSide,
    /// The per-strike implied volatility, shared by both sides.
    pub implied_volatility: Positive,
    /// The bid on this side.
    pub bid: Option<Positive>,
    /// The ask on this side.
    pub ask: Option<Positive>,
    /// The mid on this side.
    pub mid: Option<Positive>,
    /// The delta on this side.
    pub delta: Option<Decimal>,
    /// Gamma, shared by both sides: upstream's convenience mirror.
    pub gamma: Option<Decimal>,
    /// This side's full twelve-value greek snapshot, per one long contract.
    ///
    /// `None` for a degenerate strike, and for a row written before issue #74.
    pub greeks: Option<GreeksSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use positive::pos_or_panic;
    use rust_decimal_macros::dec;

    fn instant(day: u32) -> DateTime<Utc> {
        match Utc.with_ymd_and_hms(2026, 1, day, 14, 30, 0).single() {
            Some(instant) => instant,
            None => panic!("the test instant must be valid"),
        }
    }

    fn quote(strike: f64) -> QuoteRow {
        QuoteRow::new(pos_or_panic!(strike), pos_or_panic!(0.18))
            .with_call(
                Some(pos_or_panic!(1.0)),
                Some(pos_or_panic!(1.2)),
                Some(pos_or_panic!(1.1)),
                Some(dec!(0.51)),
            )
            .with_put(
                Some(pos_or_panic!(0.9)),
                Some(pos_or_panic!(1.1)),
                Some(pos_or_panic!(1.0)),
                Some(dec!(-0.49)),
            )
            .with_gamma(Some(dec!(0.0031)))
    }

    fn expiration(day: u32, strikes: &[f64]) -> ExpirationRecord {
        ExpirationRecord::new(
            instant(day),
            pos_or_panic!(f64::from(day)),
            vec!["weeklies".to_string()],
            strikes.iter().copied().map(quote).collect(),
        )
    }

    fn record(simulation: Uuid, generation: u64, step: usize) -> SnapshotRecord {
        SnapshotRecord::new(
            simulation,
            generation,
            step,
            instant(5),
            "SPX".to_string(),
            pos_or_panic!(5000.0),
            pos_or_panic!(0.18),
            vec![
                expiration(6, &[4975.0, 5000.0, 5025.0]),
                expiration(9, &[4975.0, 5000.0]),
            ],
        )
    }

    /// The same coordinate always yields the same id — the property a retry
    /// depends on.
    #[test]
    fn test_the_snapshot_id_is_deterministic() {
        let simulation = Uuid::from_u128(7);

        assert_eq!(
            snapshot_id(simulation, 3, 11),
            snapshot_id(simulation, 3, 11)
        );
    }

    /// Every component of the coordinate changes the id, including the two that
    /// a naive concatenation would confuse.
    #[test]
    fn test_the_snapshot_id_separates_its_components() {
        let simulation = Uuid::from_u128(7);
        let other = Uuid::from_u128(8);

        assert_ne!(snapshot_id(simulation, 3, 11), snapshot_id(other, 3, 11));
        assert_ne!(
            snapshot_id(simulation, 3, 11),
            snapshot_id(simulation, 4, 11)
        );
        assert_ne!(
            snapshot_id(simulation, 3, 11),
            snapshot_id(simulation, 3, 12)
        );
        // Without the separators, (1, 23) and (12, 3) would collide.
        assert_ne!(
            snapshot_id(simulation, 1, 23),
            snapshot_id(simulation, 12, 3)
        );
    }

    /// A caller with no reason to think about generations has a name to pass,
    /// rather than a literal it would have to guess.
    #[test]
    fn test_the_current_generation_is_addressable() {
        let simulation = Uuid::from_u128(7);

        assert_eq!(CURRENT_SNAPSHOT_GENERATION, 2);
        assert_eq!(
            record(simulation, CURRENT_SNAPSHOT_GENERATION, 0).snapshot_id(),
            snapshot_id(simulation, CURRENT_SNAPSHOT_GENERATION, 0)
        );
    }

    /// One step of one simulation is a different row in each generation.
    ///
    /// This is what a generation is for. The tables collapse on
    /// `(simulation, generation, step)`, so if two builds that price a step
    /// differently shared a generation, the later one would not sit beside the
    /// earlier tape — it would replace it, and a client replaying against
    /// either would get whichever landed last.
    #[test]
    fn test_a_generation_bump_addresses_a_different_row() {
        let simulation = Uuid::from_u128(7);

        let previous = snapshot_id(simulation, CURRENT_SNAPSHOT_GENERATION - 1, 3);
        let current = snapshot_id(simulation, CURRENT_SNAPSHOT_GENERATION, 3);

        assert_ne!(
            previous, current,
            "the same step under two generations must be two rows"
        );
        assert_eq!(
            current,
            snapshot_id(simulation, CURRENT_SNAPSHOT_GENERATION, 3),
            "and the current one must still be reproducible"
        );
    }

    /// The id is a name-based v5 UUID, which is what makes it reproducible
    /// across processes and restarts.
    #[test]
    fn test_the_snapshot_id_is_a_v5_uuid() {
        assert_eq!(
            snapshot_id(Uuid::from_u128(7), 1, 0).get_version(),
            Some(uuid::Version::Sha1)
        );
    }

    /// A record's id agrees with the free function.
    #[test]
    fn test_a_record_derives_its_own_id() {
        let simulation = Uuid::from_u128(9);
        let record = record(simulation, 2, 4);

        assert_eq!(record.snapshot_id(), snapshot_id(simulation, 2, 4));
    }

    /// The quote count is what the completion marker will carry.
    #[test]
    fn test_the_quote_count_sums_every_expiration() {
        assert_eq!(record(Uuid::from_u128(9), 1, 0).quote_count(), 5);
    }

    /// A well-formed record validates.
    #[test]
    fn test_a_well_formed_record_validates() {
        match record(Uuid::from_u128(9), 1, 0).validate() {
            Ok(()) => {}
            Err(error) => panic!("the reference record must validate: {error}"),
        }
    }

    /// An empty snapshot is legal: a simulation whose schedule yields nothing
    /// live at a step still has a step.
    #[test]
    fn test_an_empty_snapshot_is_legal() {
        let mut empty = record(Uuid::from_u128(9), 1, 0);
        empty.expirations.clear();

        assert_eq!(empty.quote_count(), 0);
        assert!(empty.validate().is_ok());
    }

    /// Out-of-order expirations are rejected: reads return them ascending, so an
    /// unordered write could never round-trip.
    #[test]
    fn test_unordered_expirations_are_rejected() {
        let mut unordered = record(Uuid::from_u128(9), 1, 0);
        unordered.expirations.reverse();

        match unordered.validate() {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "expirations");
                assert!(reason.contains("ascending"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A repeated expiration is rejected: it is the deduplication key, so the
    /// second one would silently replace the first.
    #[test]
    fn test_a_repeated_expiration_is_rejected() {
        let mut repeated = record(Uuid::from_u128(9), 1, 0);
        repeated.expirations = vec![expiration(6, &[5000.0]), expiration(6, &[5000.0])];

        assert!(repeated.validate().is_err());
    }

    /// Out-of-order strikes are rejected, for the same reason as expirations.
    #[test]
    fn test_unordered_strikes_are_rejected() {
        let mut unordered = record(Uuid::from_u128(9), 1, 0);
        unordered.expirations = vec![expiration(6, &[5025.0, 5000.0])];

        match unordered.validate() {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "quotes");
                assert!(reason.contains("ascending"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A repeated strike inside one expiration is rejected: it collapses on
    /// merge, losing a contract.
    #[test]
    fn test_a_repeated_strike_is_rejected() {
        let mut repeated = record(Uuid::from_u128(9), 1, 0);
        repeated.expirations = vec![expiration(6, &[5000.0, 5000.0])];

        assert!(repeated.validate().is_err());
    }

    /// An empty symbol is rejected.
    #[test]
    fn test_an_empty_symbol_is_rejected() {
        let mut blank = record(Uuid::from_u128(9), 1, 0);
        blank.symbol = "   ".to_string();

        match blank.validate() {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "symbol"),
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// The side renders as the name the query builder and the logs use.
    #[test]
    fn test_the_contract_side_renders_its_name() {
        assert_eq!(ContractSide::Call.to_string(), "call");
        assert_eq!(ContractSide::Put.as_str(), "put");
    }
}
