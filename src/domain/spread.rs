//! What a quote costs to cross, per contract.
//!
//! A chain used to be built with ONE spread applied to every contract, which is
//! not what an option market looks like: a twelve-dollar in-the-money call and
//! a five-cent far wing do not carry the same absolute bid-ask, and the wing's
//! spread is a far larger fraction of its price. A consumer valuing a position
//! at the touch gets its entire cost of trading from this number, so a single
//! constant makes cheap wings unrealistically cheap to trade and expensive
//! contracts unrealistically expensive — and it does so in the direction that
//! flatters defined-risk structures.
//!
//! # The model
//!
//! Parametric, small, and every coefficient a create-request field:
//!
//! ```text
//! spread(contract) = max(
//!     tick,
//!     absolute_floor
//!       + proportional        * mid
//!       + moneyness_widening  * |ln(strike / underlying)|
//!       + tenor_widening      * sqrt(days_to_expiration / 365)
//! )
//! ```
//!
//! Every widening term defaults to zero, so a request that says nothing about
//! spreads gets `max(tick, absolute_floor)` on every contract, which is the
//! single scalar the service applied before. The legacy `spread` field is
//! exactly the floor term, so it keeps meaning what it meant.
//!
//! # A quote is never withdrawn
//!
//! Upstream's `OptionData::apply_spread` sets bid, ask AND mid to `None` when
//! `mid <= spread`, so cheap wings vanish from the chain exactly as they decay
//! — the moment a consumer most wants to see what closing them costs. That is
//! tracked upstream (joaquinbejar/OptionStratLib#439) and fixed on their main
//! branch, but the published 0.20 this crate builds against still erases.
//!
//! This module therefore does the widening itself, from a chain built with a
//! zero spread so nothing is erased before it gets here, and floors the bid at
//! the tick or at the mid, whichever is lower: a contract that has a mid has a
//! bid and an ask. For every quote upstream would have kept, the arithmetic is
//! upstream's, term for term.
//!
//! # What a legacy request sees change
//!
//! Two things, both consequences of the erasure fix rather than of the model:
//!
//! - **The chain is longer.** Upstream stops extending the strike grid at the
//!   first pair of strikes with no price (`chain.rs`, `some_price_is_none`),
//!   and that condition was the erasure. With nothing erased the ladder runs to
//!   the full `2 * chain_size + 1`: at spot 5000, 30 DTE and `chain_size = 30`,
//!   45 strikes become 61. Response payloads, export rows and warehouse rows
//!   grow with it, within the same configured caps.
//! - **A spread below the tick is raised to it.** `spread: 0.005` used to widen
//!   by a quarter cent each way; it now widens by half a cent. For any spread
//!   at or above the tick — every request that took the documented default —
//!   the quotes are unchanged.
//!
//! One residual erasure is upstream's and stays: a mid of exactly zero, which
//! happens when the pricing CDF underflows, is still dropped by
//! `apply_spread`. Such a contract has no mid either, so nothing is quoted
//! one-sided; it simply is not in the chain.

use crate::session::SimulationParametersV2;
use optionstratlib::chains::OptionData;
use optionstratlib::chains::chain::OptionChain;
use positive::Positive;
use rust_decimal::{Decimal, MathematicalOps};
use rust_decimal_macros::dec;
use std::sync::LazyLock;

/// The spread applied when a request says nothing: one cent.
///
/// The same value the service applied before the model existed, so silence
/// means what it always meant.
pub(crate) const DEFAULT_SPREAD_FLOOR: Decimal = dec!(0.01);

/// The smallest quotable increment, and the lowest a bid may go.
///
/// A penny, matching the two decimal places every quote in this service is
/// rounded to. A bid below it would not be a price anyone could act on.
pub(crate) const DEFAULT_TICK: Decimal = dec!(0.01);

/// [`DEFAULT_TICK`] as a `Positive`, built once.
///
/// Built here rather than at every call site so the conversion has exactly one
/// fallback, and so that fallback is not a value 100 times too large: a tick of
/// one dollar under every bid would be far worse than the penny it replaces.
static DEFAULT_TICK_POSITIVE: LazyLock<Positive> =
    LazyLock::new(|| Positive::new_decimal(DEFAULT_TICK).unwrap_or(Positive::ZERO));

/// The widest each coefficient may be set to.
///
/// Economic, not arbitrary. A `proportional` of 1 means the spread IS the mid,
/// which is already a market nobody can trade; the two widening rates reach the
/// same absurdity long before 10, since `|ln(K/S)|` is about 0.7 at a strike
/// twice the spot and `sqrt(years)` is 1 at a year. Bounding them also keeps
/// the arithmetic below `Decimal`'s range for any price a chain can carry.
pub(crate) const MAX_PROPORTIONAL: f64 = 1.0;

/// The widest the two widening rates may be set to. See [`MAX_PROPORTIONAL`].
pub(crate) const MAX_WIDENING: f64 = 10.0;

/// The largest quotable increment. A tick above this is not a tick.
pub(crate) const MAX_TICK: f64 = 10.0;

/// Days in the year used to scale the tenor term.
const DAYS_PER_YEAR: Decimal = dec!(365);

/// The decimal places quotes are rounded to, matching what upstream's chain
/// builder is given.
const QUOTE_DECIMALS: u32 = 2;

/// A parametric bid-ask model, evaluated per contract.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SpreadModel {
    /// The constant term, and the whole model when the rest are zero.
    floor: Decimal,
    /// How much of the mid price is added to the spread.
    proportional: Decimal,
    /// How fast the spread widens away from the money, in `|ln(K/S)|`.
    moneyness_widening: Decimal,
    /// How fast the spread widens with time to expiry, in `sqrt(years)`.
    tenor_widening: Decimal,
    /// The smallest quotable increment, and the floor under every bid.
    tick: Positive,
}

impl SpreadModel {
    /// Reads the model a simulation was created with.
    ///
    /// Every term has a documented default, and the defaults reproduce the
    /// single-scalar behaviour, so this never fails and never needs the caller
    /// to decide anything.
    #[must_use]
    pub(crate) fn from_parameters(parameters: &SimulationParametersV2) -> Self {
        Self {
            floor: parameters
                .spread
                .map_or(DEFAULT_SPREAD_FLOOR, |spread| spread.to_dec()),
            proportional: parameters.spread_proportional.unwrap_or(Decimal::ZERO),
            moneyness_widening: parameters
                .spread_moneyness_widening
                .unwrap_or(Decimal::ZERO),
            tenor_widening: parameters.spread_tenor_widening.unwrap_or(Decimal::ZERO),
            tick: parameters.spread_tick.unwrap_or(*DEFAULT_TICK_POSITIVE),
        }
    }

    /// The spread one contract carries.
    ///
    /// `mid`, `strike` and `underlying` are prices; `days_to_expiration` is what
    /// the snapshot says, so two contracts of the same chain differ only by
    /// their own moneyness and price.
    ///
    /// The moneyness term uses `|ln(K/S)|` rather than `|K - S|` because a
    /// dollar of distance means something different at a strike of 20 and at a
    /// strike of 2000. A logarithm that cannot be taken contributes nothing
    /// rather than failing the chain: a missing widening term is a narrower
    /// quote, not a broken one.
    #[must_use]
    pub(crate) fn spread_for(
        &self,
        mid: Positive,
        strike: Positive,
        underlying: Positive,
        days_to_expiration: Decimal,
    ) -> Positive {
        // Checked throughout. `Decimal`'s `Mul` and `Add` PANIC on overflow,
        // and both operands here are client input: a coefficient at its cap
        // against a mid near `Decimal`'s range would take the request down.
        // The coefficients are bounded at validation so this is unreachable for
        // any price a chain can carry, but a panic on the serving path is not
        // something to leave resting on a bound elsewhere. An unrepresentable
        // width is not a market, so it degrades to the tick.
        let Some(mut spread) = self
            .proportional
            .checked_mul(mid.to_dec())
            .and_then(|scaled| self.floor.checked_add(scaled))
        else {
            return self.tick;
        };

        if !self.moneyness_widening.is_zero() {
            let Some(widened) = self
                .moneyness_widening
                .checked_mul(log_moneyness(strike, underlying))
                .and_then(|term| spread.checked_add(term))
            else {
                return self.tick;
            };
            spread = widened;
        }

        if !self.tenor_widening.is_zero() && days_to_expiration > Decimal::ZERO {
            let years = days_to_expiration / DAYS_PER_YEAR;
            let Some(widened) = years
                .sqrt()
                .and_then(|root| self.tenor_widening.checked_mul(root))
                .and_then(|term| spread.checked_add(term))
            else {
                return self.tick;
            };
            spread = widened;
        }

        Positive::new_decimal(spread)
            .unwrap_or(self.tick)
            .max(self.tick)
    }

    /// Widens every quote in a chain around its mid.
    ///
    /// The chain must have been built with a zero spread, so the mids are the
    /// theoretical prices and nothing has been erased yet. `OptionChain` stores
    /// its contracts in a `BTreeSet` keyed by strike, and this rewrites values
    /// rather than keys, so the set is rebuilt in place and its order is
    /// unchanged.
    pub(crate) fn apply(&self, chain: &mut OptionChain, days_to_expiration: Decimal) {
        let underlying = chain.underlying_price;

        // Taken and moved back rather than cloned: a snapshot can carry tens of
        // thousands of contracts, each holding a boxed option set, and this
        // rewrites four `Option<Positive>` fields on each.
        let contracts = std::mem::take(&mut chain.options);
        chain.options = contracts
            .into_iter()
            .map(|mut contract| {
                self.widen(&mut contract, underlying, days_to_expiration);
                contract
            })
            .collect();
    }

    /// Widens one contract's two sides.
    fn widen(&self, contract: &mut OptionData, underlying: Positive, days: Decimal) {
        let strike = contract.strike_price;

        if let Some(mid) = contract.call_middle {
            let spread = self.spread_for(mid, strike, underlying, days);
            let (bid, ask) = self.quote_around(mid, spread);
            contract.call_bid = Some(bid);
            contract.call_ask = Some(ask);
        }

        if let Some(mid) = contract.put_middle {
            let spread = self.spread_for(mid, strike, underlying, days);
            let (bid, ask) = self.quote_around(mid, spread);
            contract.put_bid = Some(bid);
            contract.put_ask = Some(ask);
        }
    }

    /// Places a two-sided quote around a mid.
    ///
    /// Half the spread each way, rounded the way upstream rounds, and the quote
    /// is never withdrawn: a contract with a mid always comes back with a bid
    /// and an ask.
    ///
    /// The bid is floored at the tick OR at the mid, whichever is lower. The
    /// distinction matters: flooring unconditionally at the tick would let a
    /// contract worth a third of a cent be sold for a penny, which flatters
    /// short-wing structures — the exact bias this model exists to remove. A
    /// contract worth less than a tick is therefore quoted `0.00 / 0.01`:
    /// present, and worth what it is worth.
    #[must_use]
    fn quote_around(&self, mid: Positive, spread: Positive) -> (Positive, Positive) {
        let half = spread.to_dec() / Decimal::TWO;
        let mid = mid.to_dec();

        // Checked, for the same reason `spread_for` is: both terms are derived
        // from client input, and `Add` panics on overflow. An unquotable price
        // keeps the mid it had rather than taking the request down.
        let ask = match mid.checked_add(half) {
            Some(value) => round_quote(value).max(self.tick),
            None => round_quote(mid).max(self.tick),
        };
        let floor = self.tick.min(round_quote(mid));
        let bid = round_quote((mid - half).max(Decimal::ZERO))
            .max(floor)
            .min(ask);

        (bid, ask)
    }
}

/// `|ln(strike / underlying)|`, or zero when the ratio cannot be logged.
///
/// Checked division: `Positive` admits ZERO, so an underlying of zero is a type
/// the signature accepts even though upstream refuses to price one. A ratio
/// that cannot be divided or logged contributes nothing, which is a narrower
/// quote rather than a failed chain.
#[must_use]
fn log_moneyness(strike: Positive, underlying: Positive) -> Decimal {
    strike
        .to_dec()
        .checked_div(underlying.to_dec())
        .and_then(|ratio| ratio.checked_ln())
        .map_or(Decimal::ZERO, |value| value.abs())
}

/// Rounds a price the way every other quote in this service is rounded.
#[must_use]
fn round_quote(value: Decimal) -> Positive {
    Positive::new_decimal(value.round_dp(QUOTE_DECIMALS)).unwrap_or(Positive::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
    use crate::api::rest::requests_v2::CreateSimulationRequest;
    use crate::session::{ExpiryRule, ExpiryRuleKind};
    use chrono::{TimeZone, Utc};
    use positive::pos_or_panic;

    /// Effective parameters from a request, so the tests exercise the same
    /// conversion a client goes through rather than a hand-built struct that
    /// could hold a combination the request path rejects.
    fn parameters() -> SimulationParametersV2 {
        let start_at = match Utc.with_ymd_and_hms(2026, 1, 5, 14, 30, 0).single() {
            Some(instant) => instant,
            None => panic!("the test instant must be valid"),
        };
        let rule = match ExpiryRule::new("zero_dte", ExpiryRuleKind::Daily, 1) {
            Ok(rule) => rule,
            Err(error) => panic!("the test rule must be valid: {error}"),
        };

        let request = CreateSimulationRequest {
            symbol: "SPX".to_string(),
            steps: 3,
            start_at: Some(start_at),
            step_interval_seconds: Some(86_400),
            timezone: "America/New_York".to_string(),
            calendar: None,
            expiration_time: "17:00".to_string(),
            schedules: vec![rule],
            initial_price: 100.0,
            volatility: 0.2,
            risk_free_rate: 0.04,
            dividend_yield: 0.0,
            method: ApiWalkType::Brownian {
                dt: 1.0 / 252.0,
                drift: 0.0,
                volatility: 0.2,
            },
            time_frame: ApiTimeFrame::Day,
            chain_size: Some(3),
            strike_interval: Some(5.0),
            skew_slope: None,
            smile_curve: None,
            spread: Some(0.02),
            spread_proportional: None,
            spread_moneyness_widening: None,
            spread_tenor_widening: None,
            spread_tick: None,
            seed: Some(42),
        };

        match SimulationParametersV2::try_from(request) {
            Ok(parameters) => parameters,
            Err(error) => panic!("the request must convert: {error}"),
        }
    }

    /// A request that says nothing carries the scalar the service always used.
    #[test]
    fn test_the_default_model_is_the_old_scalar() {
        let mut params = parameters();
        params.spread = None;
        let model = SpreadModel::from_parameters(&params);

        for (mid, strike) in [
            (pos_or_panic!(12.0), pos_or_panic!(90.0)),
            (pos_or_panic!(0.05), pos_or_panic!(150.0)),
        ] {
            assert_eq!(
                model.spread_for(mid, strike, pos_or_panic!(100.0), dec!(30)),
                pos_or_panic!(0.01),
                "no widening term is set, so every contract carries the floor"
            );
        }
    }

    /// The legacy scalar is the floor term, and nothing else.
    #[test]
    fn test_the_legacy_scalar_is_the_floor() {
        let mut params = parameters();
        params.spread = Some(pos_or_panic!(0.25));
        let model = SpreadModel::from_parameters(&params);

        assert_eq!(
            model.spread_for(
                pos_or_panic!(3.0),
                pos_or_panic!(100.0),
                pos_or_panic!(100.0),
                dec!(30)
            ),
            pos_or_panic!(0.25)
        );
    }

    /// The proportional term makes an expensive contract cost more to cross in
    /// absolute terms, and a cheap one more in relative terms.
    ///
    /// That relative inversion is the property a single scalar cannot express,
    /// and the whole reason the model exists.
    #[test]
    fn test_a_cheap_contract_has_the_wider_relative_spread() {
        let mut params = parameters();
        params.spread = Some(pos_or_panic!(0.02));
        params.spread_proportional = Some(dec!(0.01));
        let model = SpreadModel::from_parameters(&params);

        let expensive = model.spread_for(
            pos_or_panic!(12.0),
            pos_or_panic!(90.0),
            pos_or_panic!(100.0),
            dec!(30),
        );
        let cheap = model.spread_for(
            pos_or_panic!(0.05),
            pos_or_panic!(130.0),
            pos_or_panic!(100.0),
            dec!(30),
        );

        assert!(
            expensive > cheap,
            "the dearer contract carries the wider absolute spread: {expensive} vs {cheap}"
        );
        assert!(
            cheap.to_dec() / dec!(0.05) > expensive.to_dec() / dec!(12.0),
            "and the cheaper one the wider relative spread"
        );
    }

    /// Moneyness widens the quote, symmetrically in log space.
    #[test]
    fn test_the_moneyness_term_widens_away_from_the_money() {
        let mut params = parameters();
        params.spread = Some(pos_or_panic!(0.02));
        params.spread_moneyness_widening = Some(dec!(0.5));
        let model = SpreadModel::from_parameters(&params);

        let atm = model.spread_for(
            pos_or_panic!(5.0),
            pos_or_panic!(100.0),
            pos_or_panic!(100.0),
            dec!(30),
        );
        let wing = model.spread_for(
            pos_or_panic!(5.0),
            pos_or_panic!(150.0),
            pos_or_panic!(100.0),
            dec!(30),
        );
        // 100 / 1.5, the mirror of 150 in log space.
        let mirrored_strike = match Positive::new_decimal(dec!(100) / dec!(1.5)) {
            Ok(strike) => strike,
            Err(error) => panic!("the mirrored strike must be positive: {error}"),
        };
        let mirrored = model.spread_for(
            pos_or_panic!(5.0),
            mirrored_strike,
            pos_or_panic!(100.0),
            dec!(30),
        );

        assert!(
            wing > atm,
            "a wing is wider than the money: {wing} vs {atm}"
        );
        assert!(
            (wing.to_dec() - mirrored.to_dec()).abs() < dec!(0.0000001),
            "the term is symmetric in log space: {wing} vs {mirrored}"
        );
    }

    /// Tenor widens the quote, and does so in the square root of time.
    #[test]
    fn test_the_tenor_term_widens_with_time_to_expiry() {
        let mut params = parameters();
        params.spread = Some(pos_or_panic!(0.02));
        params.spread_tenor_widening = Some(dec!(0.10));
        let model = SpreadModel::from_parameters(&params);

        let near = model.spread_for(
            pos_or_panic!(5.0),
            pos_or_panic!(100.0),
            pos_or_panic!(100.0),
            dec!(1),
        );
        let far = model.spread_for(
            pos_or_panic!(5.0),
            pos_or_panic!(100.0),
            pos_or_panic!(100.0),
            dec!(365),
        );

        assert!(far > near, "a year is wider than a day: {far} vs {near}");
        // sqrt(1/365) is about 0.0523, so the near term adds about half a cent
        // to the floor rather than the ten cents the far one adds.
        assert!(near < pos_or_panic!(0.03), "the near term is small: {near}");
        assert_eq!(far, pos_or_panic!(0.12), "a full year adds the coefficient");
    }

    /// The tick is a floor, not a suggestion.
    #[test]
    fn test_the_spread_never_falls_below_the_tick() {
        let mut params = parameters();
        params.spread = Some(pos_or_panic!(0.0001));
        let model = SpreadModel::from_parameters(&params);

        assert_eq!(
            model.spread_for(
                pos_or_panic!(0.02),
                pos_or_panic!(100.0),
                pos_or_panic!(100.0),
                dec!(7)
            ),
            pos_or_panic!(0.01),
            "a spread below one tick is not a quotable width"
        );
    }

    /// A quote is placed half a spread each way, and rounds like every other.
    #[test]
    fn test_a_quote_is_centred_on_the_mid() {
        let mut params = parameters();
        params.spread = Some(pos_or_panic!(0.40));
        let model = SpreadModel::from_parameters(&params);

        let (bid, ask) = model.quote_around(pos_or_panic!(3.00), pos_or_panic!(0.40));

        assert_eq!(bid, pos_or_panic!(2.80));
        assert_eq!(ask, pos_or_panic!(3.20));
    }

    /// The cheap wing that used to vanish is still quoted, at the tick.
    ///
    /// This is the exact case the old behaviour erased: a mid at or under the
    /// spread took bid, ask AND mid with it.
    #[test]
    fn test_a_wing_cheaper_than_the_spread_is_still_quoted() {
        let mut params = parameters();
        params.spread = Some(pos_or_panic!(0.10));
        let model = SpreadModel::from_parameters(&params);

        let (bid, ask) = model.quote_around(pos_or_panic!(0.03), pos_or_panic!(0.10));

        assert_eq!(bid, pos_or_panic!(0.01), "the bid is floored at the tick");
        assert_eq!(ask, pos_or_panic!(0.08));
        assert!(bid <= ask, "and the book is never crossed");
    }

    /// A mid of zero is quoted `0.00 / 0.01`, not withdrawn and not marked up.
    ///
    /// A worthless contract must still appear — that is the erasure fix — but
    /// it must not acquire a penny of value on the way, which is what a bid
    /// floored unconditionally at the tick would do.
    #[test]
    fn test_a_zero_mid_is_quoted_without_being_marked_up() {
        let model = SpreadModel::from_parameters(&parameters());

        let (bid, ask) = model.quote_around(Positive::ZERO, pos_or_panic!(0.01));

        assert_eq!(bid, Positive::ZERO, "nothing is worth nothing");
        assert_eq!(ask, pos_or_panic!(0.01));
        assert!(bid <= ask);
    }

    /// A sub-tick mid is never bid above itself.
    #[test]
    fn test_a_sub_tick_mid_is_not_bid_above_its_value() {
        let model = SpreadModel::from_parameters(&parameters());

        let (bid, ask) = model.quote_around(pos_or_panic!(0.003), pos_or_panic!(0.01));

        assert_eq!(bid, Positive::ZERO, "a third of a cent is not worth a cent");
        assert!(ask >= pos_or_panic!(0.01));
    }

    /// An absurd coefficient degrades to the tick instead of panicking.
    ///
    /// `Decimal` multiplication panics on overflow, and both operands come from
    /// a client: the coefficient from the request, the mid from the price.
    #[test]
    fn test_an_unrepresentable_spread_degrades_to_the_tick() {
        let mut params = parameters();
        params.spread_proportional = Some(Decimal::MAX);
        let model = SpreadModel::from_parameters(&params);

        assert_eq!(
            model.spread_for(
                pos_or_panic!(1000.0),
                pos_or_panic!(100.0),
                pos_or_panic!(100.0),
                dec!(30)
            ),
            pos_or_panic!(0.01),
            "an unrepresentable width is not a market"
        );
    }
}
