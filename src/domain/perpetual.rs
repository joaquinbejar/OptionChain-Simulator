//! Perpetual options: contracts on an option payoff that never expire.
//!
//! A perpetual option is kept at its payoff by a funding payment instead of by
//! an expiration. The long side pays the short side, continuously, the
//! difference between the contract's price and the payoff at a rate
//! `funding_rate` per year, which makes the contract worth what a European
//! option of a random, exponentially distributed maturity is worth: the mean
//! maturity is `1 / funding_rate` years.
//!
//! A **ladder** stacks them. Rung 1 is that contract. Rung `j` is funded
//! against the price of rung `j - 1` instead of against the payoff, and its
//! mean maturity is `j / funding_rate` years, so the rungs of one ladder are a
//! term structure with no expiration in it.
//!
//! # What this module does not do
//!
//! It does not price anything. Every value comes from upstream
//! `optionstratlib::pricing`, which has the closed form under geometric
//! Brownian motion; this module only decides which contracts are quoted and
//! from which inputs.
//!
//! # The inputs, and the ones it leaves out
//!
//! A step prices its perpetual contracts from the factor row every chain at
//! that step is priced from: the same underlying price and the same base
//! volatility, with the drift `risk_free_rate - dividend_yield`. That makes the
//! block a pure function of the parameters and the tape, exactly as a chain is.
//!
//! Skew and smile are **not** applied. Upstream shapes them per expiration, and
//! a perpetual contract has none to read them at, so every strike is priced at
//! the base volatility. The spread model is not applied either: the quote is
//! the model value.
//!
//! # Why a pinned ladder
//!
//! A contract that never expires has to stay quoted. Under a rolling ladder a
//! strike leaves the chain as the underlying moves, which for an expiring
//! option is survivable and for a perpetual one is not: the position would
//! outlive its own quote. A simulation that quotes perpetual options therefore
//! pins its ladder, and quotes them at exactly the pinned strikes.

use crate::domain::ladder::PinnedLadder;
use crate::session::SimulationParametersV2;
use crate::utils::ChainError;
use optionstratlib::OptionStyle;
use optionstratlib::pricing::{
    EverlastingParams, everlasting_delta, everlasting_funding, everlasting_gamma,
    everlasting_mean_maturity, everlasting_price, everlasting_vega,
};
use positive::Positive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

/// The most rungs a simulation may quote.
///
/// A bound on work, like the chain size: every rung prices the whole pinned
/// ladder. Eight rungs span a factor of eight in mean maturity, which is more
/// term structure than one funding rate can usefully carry.
pub(crate) const MAX_PERPETUAL_RUNGS: u32 = 8;

/// The lowest funding rate accepted, per year: a mean maturity of a century.
pub(crate) const MIN_FUNDING_RATE: Decimal = dec!(0.01);

/// The highest funding rate accepted, per year: a mean maturity of one hour.
pub(crate) const MAX_FUNDING_RATE: Decimal = dec!(8760);

/// The largest growth of the forward a rung may add: `κ / (κ − μ)`, with `κ`
/// the funding rate and `μ` the drift.
///
/// The forward of rung `j` is the spot times that ratio to the power `j`, so a
/// funding rate barely above the drift makes it astronomically large: at a
/// ratio of 1e5 the eighth rung is 1e40 times the spot, which no price type
/// holds. Capping the ratio at two, which is a funding rate of at least twice
/// the drift, keeps the highest rung's forward within 256 times the spot.
pub(crate) const MAX_FORWARD_GROWTH: Decimal = dec!(2);

/// The perpetual options a simulation quotes.
///
/// Resolved from the request and stored with the parameters, so a replay
/// quotes the same contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerpetualSpec {
    /// The funding rate, per year. Rung `j` has a mean maturity of
    /// `j / funding_rate` years.
    pub funding_rate: Positive,
    /// How many rungs of the ladder are quoted, from rung 1 up.
    pub rungs: NonZeroU32,
}

impl PerpetualSpec {
    /// Checks the specification against the simulation it belongs to.
    ///
    /// Held to on both paths, the request and a document read back from the
    /// store, so a hand-edited document cannot quote what a request could not
    /// ask for.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming the offending field when the
    /// rung count is above [`MAX_PERPETUAL_RUNGS`], the funding rate is outside
    /// `[MIN_FUNDING_RATE, MAX_FUNDING_RATE]` or below twice the drift
    /// `risk_free_rate - dividend_yield`, or the simulation's strike ladder is
    /// not pinned.
    pub(crate) fn validate(&self, parameters: &SimulationParametersV2) -> Result<(), ChainError> {
        if self.rungs.get() > MAX_PERPETUAL_RUNGS {
            return Err(ChainError::Validation {
                field: "perpetuals.rungs".to_string(),
                reason: format!("must not exceed {MAX_PERPETUAL_RUNGS}, got {}", self.rungs),
            });
        }

        let funding_rate = self.funding_rate.to_dec();
        if !(MIN_FUNDING_RATE..=MAX_FUNDING_RATE).contains(&funding_rate) {
            return Err(ChainError::Validation {
                field: "perpetuals.funding_rate".to_string(),
                reason: format!(
                    "must be within [{MIN_FUNDING_RATE}, {MAX_FUNDING_RATE}] per year, got \
                     {funding_rate}"
                ),
            });
        }

        // The contract is worth the payoff at a random maturity, and the
        // expected underlying at that maturity is finite only when the funding
        // rate outruns the drift. Refused here, by name, rather than as an
        // upstream pricing error on the first step.
        let drift = drift(parameters)?;
        if funding_rate <= drift {
            return Err(ChainError::Validation {
                field: "perpetuals.funding_rate".to_string(),
                reason: format!(
                    "must exceed risk_free_rate - dividend_yield ({drift}), got {funding_rate}; \
                     at or below it the contract has no finite price"
                ),
            });
        }
        // Finite is not enough: see `MAX_FORWARD_GROWTH`. `κ / (κ − μ) <= 2` is
        // `κ >= 2μ`, compared in that form so nothing is divided. A drift that
        // is zero or negative passes for every funding rate, as it should.
        let floor =
            drift
                .checked_mul(MAX_FORWARD_GROWTH)
                .ok_or_else(|| ChainError::Validation {
                    field: "risk_free_rate".to_string(),
                    reason: "risk_free_rate - dividend_yield is too large to bound a funding rate"
                        .to_string(),
                })?;
        if funding_rate < floor {
            return Err(ChainError::Validation {
                field: "perpetuals.funding_rate".to_string(),
                reason: format!(
                    "must be at least twice risk_free_rate - dividend_yield ({floor}), got \
                     {funding_rate}; closer to it the expected underlying at the contract's \
                     maturity is too large to quote"
                ),
            });
        }

        if !parameters.strike_ladder.is_pinned() {
            return Err(ChainError::Validation {
                field: "strike_ladder".to_string(),
                reason: "perpetual options require a pinned strike_ladder: a contract that \
                         never expires has to stay quoted"
                    .to_string(),
            });
        }

        Ok(())
    }

    /// How many contracts this specification adds to every snapshot of a
    /// simulation quoting `strikes` strikes: one per strike and rung, the unit
    /// the per-snapshot cap counts in.
    #[must_use]
    pub(crate) fn contracts(&self, strikes: usize) -> Option<usize> {
        let rungs = usize::try_from(self.rungs.get()).ok()?;
        strikes.checked_mul(rungs)
    }
}

/// One side of a perpetual contract at one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PerpetualQuote {
    /// The model value.
    pub(crate) price: Decimal,
    /// The change in price per unit move of the underlying.
    pub(crate) delta: Decimal,
    /// The change in delta per unit move of the underlying.
    pub(crate) gamma: Decimal,
    /// The change in price per unit of volatility.
    pub(crate) vega: Decimal,
    /// What the long side pays the short side, per year and per unit of
    /// underlying, at this step.
    pub(crate) funding: Decimal,
}

/// One strike of one rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PerpetualContract {
    /// The strike price.
    pub(crate) strike: Positive,
    /// The call side.
    pub(crate) call: PerpetualQuote,
    /// The put side.
    pub(crate) put: PerpetualQuote,
}

/// One rung of the ladder at one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PerpetualRung {
    /// The rung, from 1.
    pub(crate) rung: NonZeroU32,
    /// The rung's mean maturity, `rung / funding_rate`, in years.
    pub(crate) mean_maturity_years: Positive,
    /// The pinned strikes, ascending.
    pub(crate) contracts: Vec<PerpetualContract>,
}

/// Every perpetual contract a simulation quotes at one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PerpetualChain {
    /// The funding rate every rung is funded at, per year.
    pub(crate) funding_rate: Positive,
    /// The volatility every contract is priced at: the step's base volatility.
    pub(crate) volatility: Positive,
    /// The rungs, ascending.
    pub(crate) rungs: Vec<PerpetualRung>,
}

impl PerpetualChain {
    /// How many contracts the block holds, one per strike and rung.
    #[must_use]
    pub(crate) fn contracts(&self) -> usize {
        self.rungs.iter().map(|rung| rung.contracts.len()).sum()
    }
}

/// The drift the perpetual contracts are priced under.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] when the difference is not representable.
fn drift(parameters: &SimulationParametersV2) -> Result<Decimal, ChainError> {
    parameters
        .risk_free_rate
        .checked_sub(parameters.dividend_yield.to_dec())
        .ok_or_else(|| ChainError::Validation {
            field: "risk_free_rate".to_string(),
            reason: "risk_free_rate - dividend_yield is not representable".to_string(),
        })
}

/// Reports an upstream pricing failure with the contract it happened at.
#[cold]
#[inline(never)]
fn pricing_error(strike: Positive, rung: NonZeroU32, error: &dyn std::fmt::Display) -> ChainError {
    ChainError::Internal(format!(
        "the perpetual option at strike {strike}, rung {rung} could not be priced: {error}"
    ))
}

/// Prices one side of one contract.
fn quote(
    spot: Positive,
    strike: Positive,
    volatility: Positive,
    drift: Decimal,
    spec: &PerpetualSpec,
    rung: NonZeroU32,
    style: OptionStyle,
) -> Result<PerpetualQuote, ChainError> {
    let params = EverlastingParams::try_new(
        spot,
        strike,
        volatility,
        drift,
        spec.funding_rate,
        rung,
        style,
    )
    .map_err(|e| pricing_error(strike, rung, &e))?;

    Ok(PerpetualQuote {
        price: everlasting_price(&params).map_err(|e| pricing_error(strike, rung, &e))?,
        delta: everlasting_delta(&params).map_err(|e| pricing_error(strike, rung, &e))?,
        gamma: everlasting_gamma(&params).map_err(|e| pricing_error(strike, rung, &e))?,
        vega: everlasting_vega(&params).map_err(|e| pricing_error(strike, rung, &e))?,
        funding: everlasting_funding(&params).map_err(|e| pricing_error(strike, rung, &e))?,
    })
}

/// Builds the perpetual block of one step, or `None` for a simulation that
/// does not quote perpetual options.
///
/// Pure: the same `(parameters, spot, volatility)` always gives the same
/// block. Nothing here reads a clock or draws a sample.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] when the parameters carry no pinned
/// ladder to quote on, and [`ChainError::Internal`] when upstream cannot price
/// a contract.
pub(crate) fn build_perpetual_chain(
    parameters: &SimulationParametersV2,
    spot: Positive,
    volatility: Positive,
) -> Result<Option<PerpetualChain>, ChainError> {
    let Some(spec) = parameters.perpetuals else {
        return Ok(None);
    };

    let ladder = PinnedLadder::resolve(parameters)?;
    let drift = drift(parameters)?;

    let mut rungs = Vec::new();
    for index in 1..=spec.rungs.get() {
        // `index` starts at one, so this is the rung itself and never `None`;
        // the `else` is there so that no arithmetic on it can panic.
        let Some(rung) = NonZeroU32::new(index) else {
            continue;
        };
        let mean_maturity_years = everlasting_mean_maturity(spec.funding_rate, rung)
            .map_err(|e| ChainError::Internal(format!("rung {rung} has no mean maturity: {e}")))?;

        let mut contracts = Vec::with_capacity(ladder.strikes().len());
        for &strike in ladder.strikes() {
            contracts.push(PerpetualContract {
                strike,
                call: quote(
                    spot,
                    strike,
                    volatility,
                    drift,
                    &spec,
                    rung,
                    OptionStyle::Call,
                )?,
                put: quote(
                    spot,
                    strike,
                    volatility,
                    drift,
                    &spec,
                    rung,
                    OptionStyle::Put,
                )?,
            });
        }

        rungs.push(PerpetualRung {
            rung,
            mean_maturity_years,
            contracts,
        });
    }

    Ok(Some(PerpetualChain {
        funding_rate: spec.funding_rate,
        volatility,
        rungs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
    use crate::api::rest::requests_v2::{CreateSimulationRequest, PerpetualsRequest};
    use crate::domain::ladder::StrikeLadder;
    use crate::session::{ExpiryRule, ExpiryRuleKind};
    use chrono::{TimeZone, Utc};
    use optionstratlib::pricing::everlasting_forward;
    use positive::pos_or_panic;

    /// A pinned request quoting `rungs` rungs at `funding_rate`, on a ladder of
    /// seven strikes, 25 apart, around 5000.
    fn request(funding_rate: f64, rungs: Option<u32>) -> CreateSimulationRequest {
        let start_at = match Utc.with_ymd_and_hms(2026, 1, 5, 14, 30, 0).single() {
            Some(instant) => instant,
            None => panic!("the test instant must be valid"),
        };
        let rule = match ExpiryRule::new("zero_dte", ExpiryRuleKind::Daily, 1) {
            Ok(rule) => rule,
            Err(error) => panic!("the test rule must be valid: {error}"),
        };

        CreateSimulationRequest {
            symbol: "SPX".to_string(),
            steps: 4,
            start_at: Some(start_at),
            step_interval_seconds: Some(86_400),
            timezone: "America/New_York".to_string(),
            calendar: None,
            expiration_time: "17:00".to_string(),
            schedules: vec![rule],
            initial_price: 5000.0,
            volatility: 0.2,
            risk_free_rate: 0.04,
            dividend_yield: 0.01,
            method: ApiWalkType::Brownian {
                dt: 1.0 / 252.0,
                drift: 0.0,
                volatility: 0.2,
            },
            time_frame: ApiTimeFrame::Day,
            chain_size: Some(3),
            strike_interval: Some(25.0),
            skew_slope: None,
            smile_curve: None,
            spread: Some(0.02),
            strike_ladder: Some(StrikeLadder::Pinned),
            spread_proportional: None,
            spread_moneyness_widening: None,
            spread_tick: None,
            spread_tenor_widening: None,
            seed: Some(42),
            perpetuals: Some(PerpetualsRequest {
                funding_rate,
                rungs,
            }),
        }
    }

    fn parameters(request: CreateSimulationRequest) -> SimulationParametersV2 {
        match SimulationParametersV2::try_from(request) {
            Ok(parameters) => parameters,
            Err(error) => panic!("the request must convert: {error}"),
        }
    }

    /// The field and the reason a request is refused with.
    fn refusal(request: CreateSimulationRequest) -> (String, String) {
        match SimulationParametersV2::try_from(request) {
            Ok(_) => panic!("the request must be refused"),
            Err(ChainError::Validation { field, reason }) => (field, reason),
            Err(other) => panic!("expected a validation error, got {other}"),
        }
    }

    fn chain(parameters: &SimulationParametersV2, spot: f64, volatility: f64) -> PerpetualChain {
        match build_perpetual_chain(parameters, pos_or_panic!(spot), pos_or_panic!(volatility)) {
            Ok(Some(chain)) => chain,
            Ok(None) => panic!("the simulation quotes perpetual options"),
            Err(error) => panic!("the perpetual block must build: {error}"),
        }
    }

    fn rung(index: u32) -> NonZeroU32 {
        match NonZeroU32::new(index) {
            Some(rung) => rung,
            None => panic!("a rung is at least one"),
        }
    }

    /// A simulation that did not ask for perpetual options has no block.
    #[test]
    fn test_a_simulation_without_perpetuals_has_no_block() {
        let mut request = request(12.0, None);
        request.perpetuals = None;
        let parameters = parameters(request);

        match build_perpetual_chain(&parameters, pos_or_panic!(5000.0), pos_or_panic!(0.2)) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("no perpetual block was asked for"),
            Err(error) => panic!("an absent block is not an error: {error}"),
        }
    }

    /// Every rung quotes exactly the pinned ladder, ascending.
    #[test]
    fn test_every_rung_quotes_the_pinned_ladder() {
        let parameters = parameters(request(12.0, Some(3)));
        let chain = chain(&parameters, 5010.0, 0.2);

        let expected: Vec<Positive> = [4925.0, 4950.0, 4975.0, 5000.0, 5025.0, 5050.0, 5075.0]
            .into_iter()
            .map(|strike| pos_or_panic!(strike))
            .collect();

        assert_eq!(chain.rungs.len(), 3);
        assert_eq!(chain.contracts(), 21);
        for (index, quoted) in chain.rungs.iter().enumerate() {
            let strikes: Vec<Positive> = quoted.contracts.iter().map(|c| c.strike).collect();
            assert_eq!(strikes, expected, "rung {}", quoted.rung);
            assert_eq!(
                usize::try_from(quoted.rung.get()).ok(),
                index.checked_add(1)
            );
        }
    }

    /// The strikes do not follow the spot: that is what pinning is for.
    #[test]
    fn test_the_quoted_strikes_do_not_follow_the_spot() {
        let parameters = parameters(request(12.0, Some(1)));
        let near = chain(&parameters, 5000.0, 0.2);
        let far = chain(&parameters, 6200.0, 0.2);

        let strikes = |chain: &PerpetualChain| -> Vec<Positive> {
            chain
                .rungs
                .iter()
                .flat_map(|rung| rung.contracts.iter().map(|contract| contract.strike))
                .collect()
        };
        assert_eq!(strikes(&near), strikes(&far));
    }

    /// The rung count defaults to one, the plain perpetual option.
    #[test]
    fn test_the_rung_count_defaults_to_one() {
        let parameters = parameters(request(12.0, None));
        assert_eq!(
            parameters.perpetuals.map(|perpetuals| perpetuals.rungs),
            Some(rung(1))
        );
        assert_eq!(chain(&parameters, 5000.0, 0.2).rungs.len(), 1);
    }

    /// Rung `j` has a mean maturity of `j / funding_rate` years.
    #[test]
    fn test_the_mean_maturity_is_the_rung_over_the_funding_rate() {
        let parameters = parameters(request(4.0, Some(3)));
        let chain = chain(&parameters, 5000.0, 0.2);

        let maturities: Vec<Decimal> = chain
            .rungs
            .iter()
            .map(|rung| rung.mean_maturity_years.to_dec())
            .collect();
        assert_eq!(maturities, vec![dec!(0.25), dec!(0.5), dec!(0.75)]);
    }

    /// Every value is upstream's, called with the row's spot and volatility
    /// and the drift `risk_free_rate - dividend_yield`.
    #[test]
    fn test_the_values_are_upstreams() {
        let parameters = parameters(request(12.0, Some(2)));
        let chain = chain(&parameters, 5010.0, 0.25);

        let quoted = match chain
            .rungs
            .iter()
            .find(|quoted| quoted.rung == rung(2))
            .and_then(|quoted| {
                quoted
                    .contracts
                    .iter()
                    .find(|contract| contract.strike == pos_or_panic!(5025.0))
            }) {
            Some(contract) => contract,
            None => panic!("the 5025 strike of rung 2 must be quoted"),
        };

        let direct = match EverlastingParams::try_new(
            pos_or_panic!(5010.0),
            pos_or_panic!(5025.0),
            pos_or_panic!(0.25),
            dec!(0.03),
            pos_or_panic!(12.0),
            rung(2),
            OptionStyle::Put,
        ) {
            Ok(params) => params,
            Err(error) => panic!("the direct inputs must be valid: {error}"),
        };

        assert_eq!(everlasting_price(&direct).ok(), Some(quoted.put.price));
        assert_eq!(everlasting_delta(&direct).ok(), Some(quoted.put.delta));
        assert_eq!(everlasting_gamma(&direct).ok(), Some(quoted.put.gamma));
        assert_eq!(everlasting_vega(&direct).ok(), Some(quoted.put.vega));
        assert_eq!(everlasting_funding(&direct).ok(), Some(quoted.put.funding));
        assert_eq!(chain.volatility, pos_or_panic!(0.25));
        assert_eq!(chain.funding_rate, pos_or_panic!(12.0));
    }

    /// Call minus put is the forward minus the strike, at every strike and
    /// rung: the block is internally consistent, not just individually priced.
    #[test]
    fn test_call_minus_put_is_the_forward_minus_the_strike() {
        let parameters = parameters(request(12.0, Some(4)));
        let chain = chain(&parameters, 5010.0, 0.2);

        for quoted in &chain.rungs {
            for contract in &quoted.contracts {
                let params = match EverlastingParams::try_new(
                    pos_or_panic!(5010.0),
                    contract.strike,
                    pos_or_panic!(0.2),
                    dec!(0.03),
                    pos_or_panic!(12.0),
                    quoted.rung,
                    OptionStyle::Call,
                ) {
                    Ok(params) => params,
                    Err(error) => panic!("the inputs must be valid: {error}"),
                };
                let forward = match everlasting_forward(&params) {
                    Ok(forward) => forward.to_dec(),
                    Err(error) => panic!("the forward must exist: {error}"),
                };

                let parity = contract.call.price - contract.put.price;
                let expected = forward - contract.strike.to_dec();
                assert!(
                    (parity - expected).abs() < dec!(0.000001),
                    "strike {}, rung {}: {parity} against {expected}",
                    contract.strike,
                    quoted.rung
                );
                // The deltas differ by the forward's own delta, which is
                // positive, and both sides share one gamma.
                assert!(contract.call.delta > contract.put.delta);
                assert!((contract.call.gamma - contract.put.gamma).abs() < dec!(0.000000001));
            }
        }
    }

    /// The block is a pure function of its inputs.
    #[test]
    fn test_the_same_inputs_give_the_same_block() {
        let parameters = parameters(request(52.0, Some(3)));
        assert_eq!(
            chain(&parameters, 5033.0, 0.31),
            chain(&parameters, 5033.0, 0.31)
        );
    }

    /// The whole accepted domain prices: both ends of the funding rate, the
    /// highest rung, a spot far below and far above the ladder, and a quiet and
    /// a violent volatility. A pinned ladder does not follow the spot, so a
    /// long simulation does reach these.
    #[test]
    fn test_the_accepted_domain_prices_without_error() {
        // 0.06 is the lowest rate the reference drift of 0.03 admits, which is
        // where the forward of the highest rung is largest.
        for funding_rate in [0.06, 12.0, 8760.0] {
            let parameters = parameters(request(funding_rate, Some(MAX_PERPETUAL_RUNGS)));
            for spot in [50.0, 1000.0, 5000.0, 25_000.0, 500_000.0] {
                for volatility in [0.01, 0.2, 3.0] {
                    match build_perpetual_chain(
                        &parameters,
                        pos_or_panic!(spot),
                        pos_or_panic!(volatility),
                    ) {
                        Ok(Some(chain)) => {
                            for quoted in &chain.rungs {
                                for contract in &quoted.contracts {
                                    assert!(
                                        contract.call.price >= Decimal::ZERO
                                            && contract.put.price >= Decimal::ZERO,
                                        "a negative price at funding {funding_rate}, spot \
                                         {spot}, volatility {volatility}, strike {}, rung {}",
                                        contract.strike,
                                        quoted.rung
                                    );
                                }
                            }
                        }
                        Ok(None) => panic!("the simulation quotes perpetual options"),
                        Err(error) => panic!(
                            "funding {funding_rate}, spot {spot}, volatility {volatility}: \
                             {error}"
                        ),
                    }
                }
            }
        }
    }

    /// More rungs than the cap are refused by name.
    #[test]
    fn test_too_many_rungs_are_refused() {
        let (field, reason) = refusal(request(12.0, MAX_PERPETUAL_RUNGS.checked_add(1)));
        assert_eq!(field, "perpetuals.rungs");
        assert!(reason.contains("must not exceed 8"), "{reason}");
    }

    /// Zero rungs is not a ladder.
    #[test]
    fn test_zero_rungs_are_refused() {
        let (field, reason) = refusal(request(12.0, Some(0)));
        assert_eq!(field, "perpetuals.rungs");
        assert!(reason.contains("at least 1"), "{reason}");
    }

    /// A funding rate outside the accepted range is refused by name.
    #[test]
    fn test_a_funding_rate_out_of_range_is_refused() {
        for funding_rate in [0.0, -1.0, 0.001, 9000.0, f64::NAN, f64::INFINITY] {
            let (field, _) = refusal(request(funding_rate, None));
            assert_eq!(field, "perpetuals.funding_rate", "{funding_rate}");
        }
    }

    /// A funding rate at or below the drift has no finite price, and says so.
    #[test]
    fn test_a_funding_rate_at_or_below_the_drift_is_refused() {
        // The drift of the reference request is 0.04 - 0.01.
        for funding_rate in [0.02, 0.03] {
            let (field, reason) = refusal(request(funding_rate, None));
            assert_eq!(field, "perpetuals.funding_rate");
            assert!(reason.contains("no finite price"), "{reason}");
        }
    }

    /// A funding rate above the drift but below twice it is refused too: the
    /// price is finite, and too large to quote on the higher rungs.
    #[test]
    fn test_a_funding_rate_below_twice_the_drift_is_refused() {
        for funding_rate in [0.031, 0.0599] {
            let (field, reason) = refusal(request(funding_rate, Some(MAX_PERPETUAL_RUNGS)));
            assert_eq!(field, "perpetuals.funding_rate");
            assert!(reason.contains("at least twice"), "{reason}");
        }

        // At twice the drift every rung prices, and the highest one's forward
        // is the spot times two to the eighth.
        let parameters = parameters(request(0.06, Some(MAX_PERPETUAL_RUNGS)));
        let chain = chain(&parameters, 5000.0, 0.2);
        assert_eq!(chain.rungs.len(), 8);
        let deepest = match chain.rungs.last().and_then(|rung| rung.contracts.first()) {
            Some(contract) => contract,
            None => panic!("the highest rung must quote its lowest strike"),
        };
        let parity = deepest.call.price - deepest.put.price;
        let expected = dec!(5000) * dec!(256) - deepest.strike.to_dec();
        assert!(
            (parity - expected).abs() < dec!(0.0001),
            "{parity} against {expected}"
        );
    }

    /// A drift that is zero or negative bounds nothing: every funding rate in
    /// range is accepted.
    #[test]
    fn test_a_non_positive_drift_admits_every_funding_rate_in_range() {
        for dividend_yield in [0.04, 0.09] {
            let mut request = request(0.01, Some(MAX_PERPETUAL_RUNGS));
            request.dividend_yield = dividend_yield;
            let parameters = parameters(request);
            assert_eq!(chain(&parameters, 5000.0, 0.2).rungs.len(), 8);
        }
    }

    /// A rolling ladder cannot carry a contract that never expires.
    #[test]
    fn test_a_rolling_ladder_is_refused() {
        for ladder in [None, Some(StrikeLadder::Rolling)] {
            let mut request = request(12.0, None);
            request.strike_ladder = ladder;
            let (field, reason) = refusal(request);
            assert_eq!(field, "strike_ladder");
            assert!(reason.contains("pinned"), "{reason}");
        }
    }

    /// The contract count is one per strike and rung, and refuses to wrap.
    #[test]
    fn test_the_contract_count_is_strikes_times_rungs() {
        let spec = PerpetualSpec {
            funding_rate: pos_or_panic!(12.0),
            rungs: rung(3),
        };
        assert_eq!(spec.contracts(7), Some(21));
        assert_eq!(spec.contracts(usize::MAX), None);
    }

    /// The stored form round-trips, and an unknown field in it is refused.
    #[test]
    fn test_the_stored_form_round_trips() {
        let spec = PerpetualSpec {
            funding_rate: pos_or_panic!(12.0),
            rungs: rung(3),
        };
        let json = match serde_json::to_string(&spec) {
            Ok(json) => json,
            Err(error) => panic!("the specification must serialize: {error}"),
        };
        assert_eq!(
            serde_json::from_str::<PerpetualSpec>(&json).ok(),
            Some(spec)
        );
        assert!(
            serde_json::from_str::<PerpetualSpec>(r#"{"funding_rate":12,"rungs":3,"typo":1}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<PerpetualSpec>(r#"{"funding_rate":12,"rungs":0}"#).is_err());
    }
}
