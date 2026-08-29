//! Which strikes a simulation quotes, and for how long.
//!
//! Upstream rebuilds the strike ladder around the CURRENT underlying at every
//! step, so the set of quoted contracts follows the spot. `chain_size` fixes how
//! many strikes are quoted; it does not fix which. Measured on a live v2
//! simulation, a spot move of 0.27 percent over two steps was enough for the
//! 4850 strike to leave the chain.
//!
//! For a client browsing a chain that is the right behaviour. For a client
//! HOLDING a position it is not: a leg opened at 4850 cannot be marked, closed
//! or settled at a step where 4850 does not exist, and over the 500 steps of the
//! reference scenario that is the normal case rather than an edge one. A
//! defined-risk structure loses its furthest wings first, which are exactly the
//! legs that cap its risk.
//!
//! [`StrikeLadder::Pinned`] answers that by fixing the contract universe at
//! creation: the ladder is computed once from `initial_price`, `chain_size` and
//! `strike_interval`, and every step quotes that same set. A simulation becomes
//! a closed world for its whole life, which is what makes a position in it well
//! defined. [`StrikeLadder::Rolling`] is the default and is exactly what the
//! service did before.
//!
//! # How a pinned ladder is served
//!
//! Upstream anchors every chain at `rounder(underlying, strike_interval)`, which
//! snaps to a MULTIPLE of the interval, so every ladder it builds — at any spot
//! — lies on the same grid. A pinned strike is therefore always reachable from
//! the current at-the-money strike by a whole number of intervals.
//!
//! Serving a pinned ladder is then: ask upstream for a chain wide enough to
//! reach the furthest pinned strike from where the spot is now, and keep the
//! pinned strikes out of it. The chain is priced by upstream exactly as it
//! always was, at the current spot, and nothing here builds a contract by hand.
//!
//! The cost is the strikes that get priced and dropped, which grows as the spot
//! drifts away from where it started. That is the price of the closed world, and
//! it is paid only by a simulation that asked for one.
//!
//! **A pinned ladder does not follow a large move.** If the spot leaves the
//! pinned range the chain becomes all calls or all puts, which is correct and
//! informative: a real chain does get listed further out, but a simulation that
//! silently invented new strikes would not be the closed world this exists to
//! provide. Widen the ladder at creation, with `chain_size`, rather than at step
//! time.

use crate::session::SimulationParametersV2;
use crate::utils::ChainError;
use positive::Positive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use utoipa::ToSchema;

/// Which strikes a simulation quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum StrikeLadder {
    /// The ladder is rebuilt around the current underlying at every step, so
    /// the quoted strikes stay near the money and a contract can leave the
    /// chain. The default, and what the service has always done.
    #[default]
    Rolling,
    /// The ladder is fixed at creation and every step quotes the same strikes,
    /// so a contract quoted once is quoted for the simulation's whole life.
    Pinned,
}

impl StrikeLadder {
    /// Whether this ladder fixes the contract universe.
    #[must_use]
    pub fn is_pinned(self) -> bool {
        matches!(self, StrikeLadder::Pinned)
    }
}

/// The strikes a pinned simulation quotes, resolved once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PinnedLadder {
    /// The strikes, ascending. A `BTreeSet` because that is how a chain stores
    /// its contracts and because membership is what the filter asks.
    strikes: BTreeSet<Positive>,
    /// The grid the strikes sit on.
    interval: Positive,
}

impl PinnedLadder {
    /// Resolves the ladder a simulation pinned at creation.
    ///
    /// A pure function of parameters that are already stored — the initial
    /// price, the chain size and the strike interval — so a replay reproduces
    /// it without anything extra being persisted.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] when the parameters carry no
    /// `strike_interval`. A pinned ladder needs a fixed grid, and with the
    /// interval absent upstream derives a different one per expiration, so
    /// there would be no single ladder to pin. The request path refuses the
    /// combination, so this is the stored-document half of the same guard.
    pub(crate) fn resolve(parameters: &SimulationParametersV2) -> Result<Self, ChainError> {
        let Some(interval) = parameters.strike_interval else {
            return Err(ChainError::Validation {
                field: "strike_interval".to_string(),
                reason: "a pinned strike ladder needs an explicit strike_interval: without one \
                         the interval is derived per expiration and there is no fixed grid to pin"
                    .to_string(),
            });
        };

        let chain_size = parameters
            .chain_size
            .unwrap_or(crate::domain::simulator::DEFAULT_CHAIN_SIZE);
        let centre = at_the_money(parameters.initial_price, interval);

        let mut strikes = BTreeSet::new();
        strikes.insert(centre);
        for step in 1..=chain_size {
            let offset = interval * Decimal::from(step as u64);
            if let Ok(upper) = Positive::new_decimal(centre.to_dec() + offset.to_dec()) {
                strikes.insert(upper);
            }
            // A strike at or below zero is not a contract; upstream stops
            // extending downwards at the same point.
            let lower = centre.to_dec() - offset.to_dec();
            if lower > Decimal::ZERO
                && let Ok(lower) = Positive::new_decimal(lower)
            {
                strikes.insert(lower);
            }
        }

        Ok(Self { strikes, interval })
    }

    /// The strikes, ascending.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn strikes(&self) -> &BTreeSet<Positive> {
        &self.strikes
    }

    /// How wide a chain built at `spot` must be to contain every pinned strike.
    ///
    /// Measured in strikes per side, which is what `chain_size` means to
    /// upstream: the distance in intervals from the spot's at-the-money strike
    /// to whichever pinned strike is furthest from it.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Internal`] when that distance exceeds `maximum`.
    /// Reaching it takes a spot that has moved `maximum` intervals away from
    /// where the simulation started — hundreds of percent for any ordinary
    /// configuration — and quoting a subset of the pinned ladder would silently
    /// break the guarantee the ladder exists to make, so it fails loudly
    /// instead.
    pub(crate) fn width_from(&self, spot: Positive, maximum: usize) -> Result<usize, ChainError> {
        let centre = at_the_money(spot, self.interval).to_dec();
        let interval = self.interval.to_dec();

        let mut widest = 0_usize;
        for strike in &self.strikes {
            let distance = (strike.to_dec() - centre).abs();
            let steps = distance
                .checked_div(interval)
                .map(|steps| steps.ceil())
                .and_then(|steps| usize::try_from(steps).ok())
                .ok_or_else(|| {
                    ChainError::Internal(format!(
                        "the pinned strike {strike} is unreachable from a spot of {spot}"
                    ))
                })?;
            widest = widest.max(steps);
        }

        if widest > maximum {
            return Err(ChainError::Internal(format!(
                "a pinned ladder {widest} strikes from the spot exceeds the {maximum} a chain may \
                 carry; the underlying has left the range this simulation pinned at creation"
            )));
        }
        Ok(widest)
    }

    /// Drops from a built chain every strike this ladder does not name.
    ///
    /// The chain was built wide enough to contain all of them, so what is left
    /// is exactly the pinned set, priced by upstream at the current spot.
    pub(crate) fn keep_pinned(&self, chain: &mut optionstratlib::chains::chain::OptionChain) {
        let strikes = &self.strikes;
        let contracts = std::mem::take(&mut chain.options);
        chain.options = contracts
            .into_iter()
            .filter(|contract| strikes.contains(&contract.strike_price))
            .collect();
    }
}

/// The strike upstream anchors a chain at, for a given underlying.
///
/// Mirrors `optionstratlib::chains::utils::rounder`, which is `pub(crate)`
/// there: the nearest multiple of the interval, halves rounding up. It is
/// mirrored rather than guessed, and
/// `test_the_mirrored_anchor_matches_upstream` pins the two together by
/// building a real chain and reading the strike upstream chose.
#[must_use]
pub(crate) fn at_the_money(underlying: Positive, interval: Positive) -> Positive {
    if interval == Positive::ZERO {
        return underlying;
    }

    let price = underlying.to_dec();
    let interval = interval.to_dec();
    let Some(remainder) = price.checked_rem(interval) else {
        return underlying;
    };
    let base = price - remainder;

    let rounded = if remainder + remainder >= interval {
        base + interval
    } else {
        base
    };
    Positive::new_decimal(rounded).unwrap_or(underlying)
}

#[cfg(test)]
mod tests {
    use super::*;
    use positive::pos_or_panic;
    use rust_decimal_macros::dec;

    /// The anchor rounds to the nearest multiple, halves up.
    #[test]
    fn test_the_anchor_rounds_to_the_grid() {
        let interval = pos_or_panic!(25.0);

        for (underlying, expected) in [
            (5000.0, 5000.0),
            (5004.95, 5000.0),
            (5013.54, 5025.0),
            (5012.5, 5025.0),
            (5012.49, 5000.0),
        ] {
            assert_eq!(
                at_the_money(pos_or_panic!(underlying), interval),
                pos_or_panic!(expected),
                "for {underlying}"
            );
        }
    }

    /// A zero interval leaves the price alone rather than dividing by it.
    #[test]
    fn test_a_zero_interval_is_not_a_grid() {
        assert_eq!(
            at_the_money(pos_or_panic!(5000.0), Positive::ZERO),
            pos_or_panic!(5000.0)
        );
    }

    /// The ladder is the grid around the initial price, `2n + 1` wide.
    #[test]
    fn test_the_ladder_is_the_grid_around_the_initial_price() {
        let ladder = ladder_of(5000.0, 25.0, 2);

        assert_eq!(
            ladder.strikes().iter().copied().collect::<Vec<_>>(),
            vec![
                pos_or_panic!(4950.0),
                pos_or_panic!(4975.0),
                pos_or_panic!(5000.0),
                pos_or_panic!(5025.0),
                pos_or_panic!(5050.0),
            ]
        );
    }

    /// A ladder wide enough to reach zero stops at the last positive strike.
    #[test]
    fn test_the_ladder_never_reaches_a_zero_strike() {
        let ladder = ladder_of(50.0, 25.0, 4);

        assert!(
            ladder
                .strikes()
                .iter()
                .all(|strike| *strike > Positive::ZERO),
            "a strike of zero is not a contract: {:?}",
            ladder.strikes()
        );
        assert!(ladder.strikes().contains(&pos_or_panic!(25.0)));
    }

    /// The width a chain needs grows with the distance the spot has moved.
    #[test]
    fn test_the_width_grows_with_the_distance_from_the_ladder() {
        let ladder = ladder_of(5000.0, 25.0, 2);

        // At the centre, the ladder's own half-width is enough.
        match ladder.width_from(pos_or_panic!(5000.0), 500) {
            Ok(width) => assert_eq!(width, 2),
            Err(error) => panic!("the centre must resolve: {error}"),
        }
        // Four intervals up, the furthest strike is six intervals away.
        match ladder.width_from(pos_or_panic!(5100.0), 500) {
            Ok(width) => assert_eq!(width, 6),
            Err(error) => panic!("a drifted spot must resolve: {error}"),
        }
    }

    /// A spot that has left the pinned range entirely fails loudly.
    #[test]
    fn test_a_spot_beyond_the_maximum_is_refused() {
        let ladder = ladder_of(5000.0, 25.0, 2);

        match ladder.width_from(pos_or_panic!(9000.0), 10) {
            Ok(width) => panic!("a spot 160 intervals away must not resolve, got {width}"),
            Err(ChainError::Internal(message)) => assert!(
                message.contains("left the range"),
                "the failure must explain: {message}"
            ),
            Err(error) => panic!("expected an internal failure, got {error:?}"),
        }
    }

    /// A pinned ladder needs an explicit interval, and says so.
    #[test]
    fn test_a_ladder_without_an_interval_is_refused() {
        let mut parameters = parameters(5000.0, 25.0, 2);
        parameters.strike_interval = None;

        match PinnedLadder::resolve(&parameters) {
            Ok(ladder) => panic!("a ladder with no grid must not resolve, got {ladder:?}"),
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "strike_interval");
                assert!(reason.contains("per expiration"), "{reason}");
            }
            Err(error) => panic!("expected a validation failure, got {error:?}"),
        }
    }

    /// The default is the behaviour the service already had.
    #[test]
    fn test_rolling_is_the_default() {
        assert_eq!(StrikeLadder::default(), StrikeLadder::Rolling);
        assert!(!StrikeLadder::default().is_pinned());
        assert!(StrikeLadder::Pinned.is_pinned());
    }

    /// The wire words are the ones the issue named.
    #[test]
    fn test_the_wire_form_is_snake_case() {
        for (ladder, wire) in [
            (StrikeLadder::Rolling, "\"rolling\""),
            (StrikeLadder::Pinned, "\"pinned\""),
        ] {
            match serde_json::to_string(&ladder) {
                Ok(json) => assert_eq!(json, wire),
                Err(error) => panic!("must serialize: {error}"),
            }
            match serde_json::from_str::<StrikeLadder>(wire) {
                Ok(parsed) => assert_eq!(parsed, ladder),
                Err(error) => panic!("must deserialize: {error}"),
            }
        }
    }

    /// Effective parameters from a request, so the tests go through the same
    /// conversion a client does rather than hand-building a state the request
    /// path would refuse.
    fn parameters(initial_price: f64, interval: f64, chain_size: usize) -> SimulationParametersV2 {
        use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
        use crate::api::rest::requests_v2::CreateSimulationRequest;
        use crate::session::{ExpiryRule, ExpiryRuleKind};
        use chrono::{TimeZone, Utc};

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
            steps: 4,
            start_at: Some(start_at),
            step_interval_seconds: Some(86_400),
            timezone: "America/New_York".to_string(),
            calendar: None,
            expiration_time: "17:00".to_string(),
            schedules: vec![rule],
            initial_price,
            volatility: 0.2,
            risk_free_rate: 0.04,
            dividend_yield: 0.0,
            method: ApiWalkType::Brownian {
                dt: 1.0 / 252.0,
                drift: 0.0,
                volatility: 0.2,
            },
            time_frame: ApiTimeFrame::Day,
            chain_size: Some(chain_size),
            strike_interval: Some(interval),
            skew_slope: None,
            smile_curve: None,
            spread: Some(0.02),
            strike_ladder: Some(StrikeLadder::Pinned),
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

    fn ladder_of(initial_price: f64, interval: f64, chain_size: usize) -> PinnedLadder {
        match PinnedLadder::resolve(&parameters(initial_price, interval, chain_size)) {
            Ok(ladder) => ladder,
            Err(error) => panic!("the ladder must resolve: {error}"),
        }
    }

    /// The mirrored anchor is the strike upstream actually chooses.
    ///
    /// `rounder` is `pub(crate)` upstream, so this mirrors it; the mirror is
    /// only worth having if it agrees, and this builds a real chain to check.
    #[test]
    fn test_the_mirrored_anchor_matches_upstream() {
        use optionstratlib::ExpirationDate;

        let interval = pos_or_panic!(25.0);
        for spot in [5000.0, 5004.95, 5013.54, 4987.5] {
            let mut parameters = parameters(5000.0, 25.0, 1);
            parameters.strike_ladder = StrikeLadder::Rolling;

            let chain = match crate::domain::factors::build_chain(
                &parameters,
                pos_or_panic!(spot),
                pos_or_panic!(0.2),
                ExpirationDate::Days(pos_or_panic!(30.0)),
                false,
            ) {
                Ok(chain) => chain,
                Err(error) => panic!("the chain must build at {spot}: {error}"),
            };

            // A chain of half-width one is [atm - interval, atm, atm + interval],
            // so the middle strike is upstream's anchor.
            let strikes: Vec<Positive> =
                chain.iter().map(|contract| contract.strike_price).collect();
            let middle = strikes[strikes.len() / 2];

            assert_eq!(
                at_the_money(pos_or_panic!(spot), interval),
                middle,
                "the mirror disagrees with upstream at a spot of {spot}: {strikes:?}"
            );
        }
        let _ = dec!(0);
    }
}
