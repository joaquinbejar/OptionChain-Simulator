//! The market-factor tape for v2 rolling simulations.
//!
//! A **factor row** is the whole market state at one step that is *not* an
//! option: the simulated instant, the underlying price, and the base implied
//! volatility that will price every chain at that step. The tape is the ordered
//! sequence of those rows, and it is the deterministic source both the snapshot
//! builder (#46) and the export (#49) read from.
//!
//! # Why a separate tape at all
//!
//! v1 caches a `RandomWalk<Positive, OptionChain>`, so the entire chain tape is
//! materialised up front and the walk stops when its one expiry reaches zero
//! (`src/domain/simulator.rs`). Neither works for a rolling simulation. The
//! factor tape splits the two concerns: the market path runs for the **whole**
//! requested horizon regardless of what expires along the way, and chains are
//! built on demand from a row. Memory is `O(steps)` in small rows rather than
//! `O(steps × expiries × strikes)` in contracts.
//!
//! # What makes it reproducible
//!
//! Three properties, each of them tested:
//!
//! - **The tape is a pure function of the effective parameters.** Same seed,
//!   same start, same model ⇒ the same rows, on any machine. One caveat, stated
//!   because it is real: building the seeding chain reaches upstream code that
//!   reads `Utc::now()` while stamping a calendar date. That value cannot reach
//!   a row — only the chain's `underlying_price` flows into the walk, and it is
//!   `initial_price` verbatim — so the rows stay clock-free even though the
//!   call is not.
//! - **Expiration schedules cannot perturb it.** `build` never reads the
//!   schedule at all, so adding, removing or reordering rules leaves every row
//!   byte-identical — which is what lets a client change its expiration
//!   inventory and still compare two runs' underlying paths. The tests here
//!   guard that cheaply; the *load-bearing* version of the property, that
//!   building snapshots' chains cannot consume the walker's stream either,
//!   belongs where those chains are built (#46).
//! - **The walk kernels are the ones v1 already uses.** The tape asks the same
//!   seeded [`Walker`] for the same `WalkParams` v1 builds, and reads the price
//!   path from `generate_with_vol`. It does not reimplement the mathematics, so
//!   there is no second copy to diverge.

use crate::domain::Walker;
use crate::domain::simulator::{
    DEFAULT_CHAIN_SIZE, DEFAULT_SKEW_SLOPE, DEFAULT_SMILE_CURVE, DEFAULT_SPREAD,
};
use crate::session::{SimulationMethod, SimulationParametersV2};
use crate::utils::ChainError;
use chrono::{DateTime, Utc};
use optionstratlib::ExpirationDate;
use optionstratlib::chains::{
    OptionChainBuildParams, chain::OptionChain, utils::OptionDataPriceParams,
};
use optionstratlib::simulation::steps::{Step, Xstep, Ystep};
use optionstratlib::simulation::{WalkParams, WalkTypeAble};
use positive::Positive;
use tracing::{debug, instrument};

/// One step of the market path: everything a snapshot needs that is not an
/// option contract.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the factor tape has no in-tree caller until #46 builds snapshots from it"
    )
)]
pub(crate) struct FactorRow {
    /// The 0-based step index this row describes.
    pub(crate) step: usize,
    /// The simulated instant of the step, derived from the effective start and
    /// the cursor — never from the wall clock.
    pub(crate) simulated_at: DateTime<Utc>,
    /// The underlying price at this step.
    pub(crate) spot: Positive,
    /// The **canonical** base implied volatility used to price every chain at
    /// this step.
    ///
    /// For a constant-volatility model this is the model's volatility at every
    /// row. For `Garch`, `Heston`, `Custom` and `Telegraph` it is the
    /// annualised volatility prevailing at this step, index-aligned with
    /// `spot`, as upstream's `generate_with_vol` reports it.
    pub(crate) base_volatility: Positive,
}

/// The ordered market path of a simulation, one row per requested step.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the factor tape has no in-tree caller until #46 builds snapshots from it"
    )
)]
pub(crate) struct FactorTape {
    rows: Vec<FactorRow>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the factor tape has no in-tree caller until #46 builds snapshots from it"
    )
)]
impl FactorTape {
    /// Builds the tape for `parameters`, using `method` as the resolved walk
    /// model.
    ///
    /// `method` is passed in rather than read from `parameters` because a
    /// `Historical` walk has to be resolved against the database first — a
    /// seeded symbol-and-date selection that already lives in
    /// [`crate::domain::Simulator`]. Keeping the resolution outside leaves this
    /// function synchronous and free of I/O, and therefore directly testable
    /// for the reproducibility properties that matter. It is not clock-free:
    /// the seeding chain stamps an expiration date through `Utc::now()`, as the
    /// module docs explain, but that value reaches no row. A caller with
    /// nothing to resolve passes `&parameters.method`.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] when the parameters are invalid, when
    /// the model's volatility disagrees with the parameters' (see
    /// [`resolve_base_volatility`]), when a `Historical` series is too short
    /// for the horizon, or when the simulated clock overflows; and
    /// [`ChainError::Internal`] when the resolved method is not the one the
    /// parameters name, when the initial chain cannot be built, or when the
    /// walk returns fewer points than requested.
    #[instrument(skip(parameters, method), level = "debug")]
    pub(crate) fn build(
        parameters: &SimulationParametersV2,
        method: &SimulationMethod,
    ) -> Result<Self, ChainError> {
        // `SimulationParametersV2` is public with public fields, so a crate
        // caller can hand this a struct literal that never passed a validating
        // constructor. `steps: 0` is the one that bites: two of the mirrored
        // kernels compute `size - 1`, which panics in debug and wraps to an
        // unbounded loop in release.
        parameters.validate()?;
        ensure_method_matches(parameters, method)?;
        ensure_historical_series_covers_the_horizon(parameters, method)?;
        let base_volatility = resolve_base_volatility(parameters, method)?;
        let walker = Walker::new_with_seed(parameters.seed);

        // The walk starts from an `OptionChain` because that is the shape v1's
        // `WalkParams` takes, and reusing it verbatim is what guarantees the
        // price path is the one v1 would produce for the same seed and
        // parameters. Exactly one chain is built, to seed the walk; the tape
        // itself stores none.
        let initial_chain = build_initial_chain(parameters, base_volatility)?;

        let walk_params = WalkParams {
            size: parameters.steps,
            init_step: Step {
                x: Xstep::new(
                    Positive::ONE,
                    parameters.time_frame,
                    // A nominal relative expiration for the seeding step only.
                    // The tape carries no expiration of its own — every real
                    // one comes from the planner — and this value never reaches
                    // a price. It is safe only because the generation below is
                    // called directly: never route this `WalkParams` through a
                    // `walk_steps` driver, which advances the expiry per step
                    // and would truncate the walk at the first one.
                    ExpirationDate::Days(Positive::ONE),
                ),
                y: Ystep::new(0, initial_chain),
            },
            walk_type: method.clone(),
            // `WalkParams` owns a walker, but the path below is generated by
            // calling `generate_with_vol` on ours. The clone shares the same
            // `Arc<Mutex<StdRng>>`, so there is exactly one stream either way.
            walker: Box::new(walker.clone()),
        };

        let path = walker.generate_with_vol(&walk_params).map_err(|e| {
            ChainError::Internal(format!("Failed to generate the factor tape: {e}"))
        })?;

        // Every kernel — upstream and mirrored — pushes the initial value and
        // then loops `1..size`, so the path holds **exactly** `size` points,
        // element 0 duplicating the walk's initial value. Row 0 is therefore
        // the simulation's starting state and `prices[0..steps]` consumes the
        // whole path with no slack. The guard below is the exact bound: there
        // is no spare element to shift into, and a `prices[1..=steps]` "fix"
        // would truncate the tape or fail outright.
        if path.prices.len() < parameters.steps {
            return Err(ChainError::Internal(format!(
                "the walk produced {} points but {} steps were requested",
                path.prices.len(),
                parameters.steps
            )));
        }

        let mut rows = Vec::with_capacity(parameters.steps);
        for step in 0..parameters.steps {
            let spot = *path.prices.get(step).ok_or_else(|| {
                ChainError::Internal(format!("the walk has no price for step {step}"))
            })?;

            // A stochastic-volatility model reports its own per-step path,
            // index-aligned with the prices. Every other model is constant.
            let row_volatility = match &path.vols {
                Some(vols) => *vols.get(step).ok_or_else(|| {
                    ChainError::Internal(format!("the walk has no volatility for step {step}"))
                })?,
                None => base_volatility,
            };

            rows.push(FactorRow {
                step,
                simulated_at: parameters.simulated_at(step)?,
                spot,
                base_volatility: row_volatility,
            });
        }

        debug!(
            steps = rows.len(),
            seed = parameters.seed,
            "Built the factor tape"
        );
        Ok(Self { rows })
    }

    /// The rows, in step order.
    #[must_use]
    pub(crate) fn rows(&self) -> &[FactorRow] {
        &self.rows
    }

    /// The number of steps the tape covers.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the tape is empty. A built tape never is — `steps >= 1` is
    /// validated at the request boundary — but the accessor keeps clippy and
    /// callers honest.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The row at `step`, or `None` past the end of the tape.
    #[must_use]
    pub(crate) fn row(&self, step: usize) -> Option<&FactorRow> {
        self.rows.get(step)
    }
}

/// Rejects a resolved method that is not the one the stored parameters name.
///
/// `build` takes the method separately so a `Historical` walk can be resolved
/// against the database first, but that flexibility would otherwise let a
/// caller build a `Brownian` tape for parameters that say `Heston` — a tape
/// nothing could reproduce from the persisted replay inputs, which is precisely
/// what ADR 0001 §8 promises. A `Historical` method is compared by variant
/// rather than by value, because resolution is exactly what fills in its
/// symbol and prices.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] naming both methods when they disagree.
/// Internal rather than validation: no request can produce this, only a
/// mis-wired caller.
fn ensure_method_matches(
    parameters: &SimulationParametersV2,
    method: &SimulationMethod,
) -> Result<(), ChainError> {
    let agrees = match (&parameters.method, method) {
        (SimulationMethod::Historical { .. }, SimulationMethod::Historical { .. }) => true,
        (stored, resolved) => stored == resolved,
    };

    if agrees {
        Ok(())
    } else {
        Err(ChainError::Internal(format!(
            "the resolved walk method does not match the simulation's parameters: \
             parameters say {:?}, the caller passed {method:?}",
            parameters.method
        )))
    }
}

/// Rejects a historical series too short to cover the requested horizon.
///
/// Upstream's `historical` kernel errors when the embedded series is shorter
/// than the walk, and that error would surface as a `500`. It is a client
/// mistake — a request embedding five prices and asking for a hundred steps —
/// so it is caught here and named, the way v1 avoids the problem entirely by
/// refetching from the database.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] naming `method.prices`.
fn ensure_historical_series_covers_the_horizon(
    parameters: &SimulationParametersV2,
    method: &SimulationMethod,
) -> Result<(), ChainError> {
    let SimulationMethod::Historical { prices, .. } = method else {
        return Ok(());
    };

    if prices.len() < parameters.steps {
        return Err(ChainError::Validation {
            field: "method.prices".to_string(),
            reason: format!(
                "must carry at least one price per step: {} supplied for {} steps",
                prices.len(),
                parameters.steps
            ),
        });
    }
    Ok(())
}

/// Determines the one authoritative base volatility for a simulation, and
/// rejects a request that specifies two different ones.
///
/// v1 leaves this ambiguous: the top-level `volatility` prices step zero while
/// the selected `WalkType` carries its own volatility for every later step, so
/// a request naming `volatility: 0.18` and `Brownian { volatility: 0.35 }` is
/// accepted and silently produces a chain priced at one number and a path
/// driven by another. #45 exists partly to remove that.
///
/// The agreement itself is enforced at the boundary, by
/// `SimulationParametersV2::validate`, which runs on both the request and the
/// stored-document path. The check is repeated here because this function is
/// what would silently pick a winner otherwise, and a domain type that depends
/// on an invariant should say so rather than assume it.
///
/// The rule: for the nine synthetic walk types the model's volatility is
/// authoritative, and the parameters' must agree with it. For `Historical`
/// there is no model volatility — `WalkType::volatility()` returns `None` — so
/// the parameters' value is used as a constant.
///
/// **That is a deliberate simplification, and it changes behaviour relative to
/// v1.** Upstream ships a causal estimator, `expanding_window_vols`, whose
/// estimate at index `i` uses only the returns of `prices[..=i]`, and v1 prices
/// its historical chains with it through `walk_steps_par`. v2 does not walk
/// through that driver — the whole point of the factor tape is to stop
/// materialising chains per step — and the estimator is private to it, so
/// adopting it means either duplicating the mathematics here, which is exactly
/// what this module exists to avoid, or a change upstream. Until then a
/// historical v2 simulation prices every step at the requested constant
/// volatility. Worth knowing before comparing a v1 and a v2 historical run.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] naming `volatility` when the two
/// disagree.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the factor tape has no in-tree caller until #46 builds snapshots from it"
    )
)]
fn resolve_base_volatility(
    parameters: &SimulationParametersV2,
    method: &SimulationMethod,
) -> Result<Positive, ChainError> {
    match method.volatility() {
        Some(model_volatility) => {
            if model_volatility != parameters.volatility {
                return Err(ChainError::Validation {
                    field: "volatility".to_string(),
                    reason: format!(
                        "must match the walk model's volatility ({model_volatility}), got {}; \
                         a simulation has exactly one base volatility",
                        parameters.volatility
                    ),
                });
            }
            Ok(model_volatility)
        }
        None => Ok(parameters.volatility),
    }
}

/// Builds the single option chain that seeds the walk.
///
/// Mirrors v1's wiring in [`crate::domain::Simulator`] and the shared defaults,
/// with one deliberate difference: the expiration is nominal here (see below),
/// where v1 passes the request's `days_to_expiration`. That changes the strike
/// ladder when `strike_interval` is `None`, because upstream derives it from
/// the expiration — but not the walk's starting point, which is the chain's
/// `underlying_price` and is copied verbatim from the price parameters. So the
/// price path is the one v1 would produce for the same seed.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the factor tape has no in-tree caller until #46 builds snapshots from it"
    )
)]
fn build_initial_chain(
    parameters: &SimulationParametersV2,
    base_volatility: Positive,
) -> Result<OptionChain, ChainError> {
    let chain_size = parameters.chain_size.unwrap_or(DEFAULT_CHAIN_SIZE);
    let skew_slope = parameters.skew_slope.unwrap_or(DEFAULT_SKEW_SLOPE);
    let smile_curve = parameters.smile_curve.unwrap_or(DEFAULT_SMILE_CURVE);
    let spread = match parameters.spread {
        Some(spread) => spread,
        None => Positive::new_decimal(DEFAULT_SPREAD).map_err(|e| {
            ChainError::Internal(format!("the default spread is not a valid Positive: {e}"))
        })?,
    };

    let price_params = OptionDataPriceParams::new(
        Some(Box::new(parameters.initial_price)),
        // The seeding chain needs *a* expiration; the real ones come from the
        // planner, per snapshot. One day keeps it well-conditioned.
        Some(ExpirationDate::Days(Positive::ONE)),
        Some(parameters.risk_free_rate),
        Some(parameters.dividend_yield),
        Some(parameters.symbol.clone()),
    );

    let build_params = OptionChainBuildParams::new(
        parameters.symbol.clone(),
        Some(Positive::ONE),
        chain_size,
        parameters.strike_interval,
        skew_slope,
        smile_curve,
        spread,
        2,
        price_params,
        base_volatility,
    );

    OptionChain::build_chain(&build_params)
        .map_err(|e| ChainError::Internal(format!("Failed to build the seeding chain: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
    use crate::api::rest::requests_v2::CreateSimulationRequest;
    use crate::session::{ExpiryRule, ExpiryRuleKind};
    use chrono::{TimeZone, Weekday};

    /// The reference market path: a modest daily Brownian walk over the
    /// simulated clock ADR 0001 §14 uses.
    fn request(steps: usize, method: ApiWalkType, volatility: f64) -> CreateSimulationRequest {
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
            schedules: vec![rule("zero_dte", ExpiryRuleKind::Daily, 1)],
            initial_price: 5000.0,
            volatility,
            risk_free_rate: 0.04,
            dividend_yield: 0.0,
            method,
            time_frame: ApiTimeFrame::Day,
            // A small ladder keeps the seeding chain cheap; the tape stores none.
            chain_size: Some(3),
            strike_interval: Some(25.0),
            skew_slope: None,
            smile_curve: None,
            spread: Some(0.02),
            seed: Some(42),
        }
    }

    fn rule(id: &str, kind: ExpiryRuleKind, count: usize) -> ExpiryRule {
        match ExpiryRule::new(id, kind, count) {
            Ok(rule) => rule,
            Err(error) => panic!("the test rule must be valid: {error}"),
        }
    }

    fn brownian(volatility: f64) -> ApiWalkType {
        ApiWalkType::Brownian {
            dt: 1.0 / 252.0,
            drift: 0.0,
            volatility,
        }
    }

    fn garch(volatility: f64) -> ApiWalkType {
        ApiWalkType::Garch {
            dt: 1.0 / 252.0,
            drift: 0.0,
            volatility,
            alpha: 0.1,
            beta: 0.85,
        }
    }

    fn heston(volatility: f64) -> ApiWalkType {
        ApiWalkType::Heston {
            dt: 1.0 / 252.0,
            drift: 0.0,
            volatility,
            kappa: 2.0,
            theta: 0.04,
            xi: 0.3,
            rho: -0.7,
        }
    }

    fn parameters(request: CreateSimulationRequest) -> SimulationParametersV2 {
        match SimulationParametersV2::try_from(request) {
            Ok(parameters) => parameters,
            Err(error) => panic!("the request must convert: {error}"),
        }
    }

    fn tape(parameters: &SimulationParametersV2) -> FactorTape {
        match FactorTape::build(parameters, &parameters.method) {
            Ok(tape) => tape,
            Err(error) => panic!("the tape must build: {error}"),
        }
    }

    // ---- shape and alignment ---------------------------------------------

    /// The tape has exactly one row per requested step, indexed from zero.
    #[test]
    fn test_tape_has_exactly_one_row_per_step() {
        let parameters = parameters(request(20, brownian(0.18), 0.18));

        let tape = tape(&parameters);

        assert_eq!(tape.len(), 20);
        assert!(!tape.is_empty());
        for (index, row) in tape.rows().iter().enumerate() {
            assert_eq!(row.step, index);
        }
        assert!(tape.row(20).is_none(), "the tape must end at its last step");
    }

    /// Row zero is the simulation's starting state: the effective start and the
    /// requested initial price.
    #[test]
    fn test_first_row_is_the_starting_state() {
        let parameters = parameters(request(5, brownian(0.18), 0.18));

        let tape = tape(&parameters);
        let first = match tape.row(0) {
            Some(row) => row,
            None => panic!("the tape must have a first row"),
        };

        assert_eq!(first.simulated_at, parameters.effective_start);
        assert_eq!(first.spot, parameters.initial_price);
    }

    /// Each row's instant follows the simulated clock, not the wall clock.
    #[test]
    fn test_row_instants_follow_the_simulated_clock() {
        let parameters = parameters(request(10, brownian(0.18), 0.18));

        let tape = tape(&parameters);

        for row in tape.rows() {
            match parameters.simulated_at(row.step) {
                Ok(expected) => assert_eq!(row.simulated_at, expected),
                Err(error) => panic!("the clock must resolve: {error}"),
            }
        }
    }

    /// A simulation spanning years still produces exactly `steps` rows, even
    /// though the short-dated options of §14's reference schedule would expire
    /// hundreds of times over that horizon.
    ///
    /// This is the property v1 cannot offer: its walk truncates when its one
    /// expiry reaches zero.
    #[test]
    fn test_a_multi_year_horizon_produces_every_requested_row() {
        // 800 daily steps is a little over three simulated years.
        let parameters = parameters(request(800, brownian(0.18), 0.18));

        let tape = tape(&parameters);

        assert_eq!(tape.len(), 800);
        let last = match tape.rows().last() {
            Some(row) => row,
            None => panic!("the tape must have a last row"),
        };
        let span = last.simulated_at - parameters.effective_start;
        assert!(
            span > chrono::Duration::days(365 * 2),
            "the horizon must span more than two years, got {span}"
        );
    }

    // ---- determinism ------------------------------------------------------

    /// The same effective parameters produce byte-identical tapes, rebuild
    /// after rebuild — which is what makes a cache eviction or a process
    /// restart invisible.
    #[test]
    fn test_same_parameters_produce_identical_tapes() {
        let parameters = parameters(request(50, brownian(0.18), 0.18));

        assert_eq!(tape(&parameters), tape(&parameters));
    }

    /// Two simulations with the same seed but built independently agree on
    /// every field of every row, not merely on the spot path.
    #[test]
    fn test_same_seed_agrees_on_full_rows() {
        let first = tape(&parameters(request(30, brownian(0.18), 0.18)));
        let second = tape(&parameters(request(30, brownian(0.18), 0.18)));

        assert_eq!(first.rows(), second.rows());
    }

    /// A different seed produces a different tape.
    #[test]
    fn test_a_different_seed_produces_a_different_tape() {
        let mut other = request(30, brownian(0.18), 0.18);
        other.seed = Some(43);

        let baseline = tape(&parameters(request(30, brownian(0.18), 0.18)));
        let different = tape(&parameters(other));

        assert_ne!(baseline.rows(), different.rows());
        // The starting state is shared by construction; the divergence must be
        // in the walk itself.
        assert_eq!(
            baseline.row(0).map(|row| row.spot),
            different.row(0).map(|row| row.spot)
        );
        assert_ne!(
            baseline.rows().last().map(|row| row.spot),
            different.rows().last().map(|row| row.spot)
        );
    }

    /// The tape is independent of the expiration schedule.
    ///
    /// This is the load-bearing isolation property: the planner draws no
    /// randomness, so a client can add, remove or reorder expiration rules and
    /// still compare two runs' underlying paths. What this test catches is
    /// `build` starting to read `parameters.schedule` at all; the version that
    /// covers snapshot building consuming the walker's RNG belongs with the
    /// snapshots themselves, in #46.
    #[test]
    fn test_the_schedule_cannot_perturb_the_tape() {
        let baseline = tape(&parameters(request(40, brownian(0.18), 0.18)));

        let mut richer = request(40, brownian(0.18), 0.18);
        richer.schedules = vec![
            rule(
                "monthlies",
                ExpiryRuleKind::Monthly {
                    weekday: Weekday::Fri,
                },
                12,
            ),
            rule(
                "weeklies",
                ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Wed, Weekday::Fri]),
                3,
            ),
            rule("zero_dte", ExpiryRuleKind::Daily, 1),
        ];

        assert_eq!(baseline.rows(), tape(&parameters(richer)).rows());
    }

    /// Reordering the same rules cannot perturb it either.
    #[test]
    fn test_rule_order_cannot_perturb_the_tape() {
        let mut forwards = request(20, brownian(0.18), 0.18);
        forwards.schedules = vec![
            rule("zero_dte", ExpiryRuleKind::Daily, 1),
            rule("weeklies", ExpiryRuleKind::weekly([Weekday::Fri]), 2),
        ];
        let mut backwards = request(20, brownian(0.18), 0.18);
        backwards.schedules = vec![
            rule("weeklies", ExpiryRuleKind::weekly([Weekday::Fri]), 2),
            rule("zero_dte", ExpiryRuleKind::Daily, 1),
        ];

        assert_eq!(
            tape(&parameters(forwards)).rows(),
            tape(&parameters(backwards)).rows()
        );
    }

    /// A different effective start shifts every instant but leaves the market
    /// path alone: the walk depends on the seed, not on when the simulation is
    /// said to begin.
    #[test]
    fn test_a_different_start_shifts_instants_without_changing_the_path() {
        let baseline = tape(&parameters(request(15, brownian(0.18), 0.18)));

        let mut later = request(15, brownian(0.18), 0.18);
        later.start_at = match Utc.with_ymd_and_hms(2030, 6, 3, 9, 0, 0).single() {
            Some(instant) => Some(instant),
            None => panic!("the test instant must be valid"),
        };
        let shifted = tape(&parameters(later));

        let baseline_spots: Vec<Positive> = baseline.rows().iter().map(|row| row.spot).collect();
        let shifted_spots: Vec<Positive> = shifted.rows().iter().map(|row| row.spot).collect();
        assert_eq!(baseline_spots, shifted_spots);
        assert_ne!(
            baseline.row(0).map(|row| row.simulated_at),
            shifted.row(0).map(|row| row.simulated_at)
        );
    }

    // ---- volatility -------------------------------------------------------

    /// A constant-volatility model reports the same base volatility at every
    /// row, and it is the one the request asked for.
    #[test]
    fn test_constant_volatility_stays_constant() {
        let parameters = parameters(request(25, brownian(0.18), 0.18));

        let tape = tape(&parameters);

        for row in tape.rows() {
            assert_eq!(
                row.base_volatility, parameters.volatility,
                "step {} drifted from the model's volatility",
                row.step
            );
        }
    }

    /// A GARCH model's volatility varies across the tape and stays aligned with
    /// the spot path, one value per row.
    #[test]
    fn test_garch_volatility_varies_and_stays_aligned() {
        let parameters = parameters(request(60, garch(0.18), 0.18));

        let tape = tape(&parameters);

        assert_eq!(tape.len(), 60);

        let series: Vec<Positive> = tape.rows().iter().map(|row| row.base_volatility).collect();
        let distinct: std::collections::BTreeSet<String> =
            series.iter().map(ToString::to_string).collect();
        assert!(
            distinct.len() > 1,
            "a GARCH tape must not have a constant volatility"
        );

        // Alignment, not just variation: row 0 carries the model's own
        // volatility, which is where upstream starts the series, and the series
        // is not its own mirror image — so a reversed or shifted column fails
        // here rather than passing on variation alone.
        assert_eq!(
            series.first(),
            Some(&parameters.volatility),
            "row 0 must carry the model's starting volatility"
        );
        let reversed: Vec<Positive> = series.iter().rev().copied().collect();
        assert_ne!(
            series, reversed,
            "a symmetric series would hide a reversed column"
        );
    }

    /// The same holds for Heston.
    #[test]
    fn test_heston_volatility_varies_and_stays_aligned() {
        let parameters = parameters(request(60, heston(0.18), 0.18));

        let tape = tape(&parameters);

        assert_eq!(tape.len(), 60);

        let series: Vec<Positive> = tape.rows().iter().map(|row| row.base_volatility).collect();
        let distinct: std::collections::BTreeSet<String> =
            series.iter().map(ToString::to_string).collect();
        assert!(
            distinct.len() > 1,
            "a Heston tape must not have a constant volatility"
        );

        // Alignment, not just variation: row 0 carries the model's own
        // volatility, which is where upstream starts the series, and the series
        // is not its own mirror image — so a reversed or shifted column fails
        // here rather than passing on variation alone.
        assert_eq!(
            series.first(),
            Some(&parameters.volatility),
            "row 0 must carry the model's starting volatility"
        );
        let reversed: Vec<Positive> = series.iter().rev().copied().collect();
        assert_ne!(
            series, reversed,
            "a symmetric series would hide a reversed column"
        );
    }

    /// A stochastic-volatility tape is reproducible in both of its series, not
    /// just the spot path.
    #[test]
    fn test_stochastic_volatility_is_reproducible() {
        let first = tape(&parameters(request(40, garch(0.18), 0.18)));
        let second = tape(&parameters(request(40, garch(0.18), 0.18)));

        assert_eq!(first.rows(), second.rows());
    }

    /// A request whose top-level volatility disagrees with its walk model's is
    /// rejected rather than silently pricing step zero at one number and
    /// walking at another.
    ///
    /// v1 accepts exactly this; removing the ambiguity is part of what #45 is
    /// for.
    #[test]
    fn test_disagreeing_volatilities_are_rejected() {
        let parameters = parameters(request(10, brownian(0.35), 0.35));
        // Build parameters whose model says 0.35 but whose top level says 0.18.
        let mismatched = SimulationParametersV2 {
            volatility: match Positive::new(0.18) {
                Ok(value) => value,
                Err(error) => panic!("0.18 must be a valid Positive: {error}"),
            },
            ..parameters
        };

        match FactorTape::build(&mismatched, &mismatched.method) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "volatility");
                assert!(reason.contains("exactly one base volatility"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// The agreement is enforced at the request boundary, not only when a tape
    /// is built — so a contradictory request never reaches the domain.
    #[test]
    fn test_disagreeing_volatilities_are_rejected_at_the_boundary() {
        let mut contradictory = request(10, brownian(0.35), 0.35);
        contradictory.volatility = 0.18;

        match SimulationParametersV2::try_from(contradictory) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "volatility");
                assert!(reason.contains("exactly one base volatility"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// Building a tape with a method the parameters do not name is refused.
    ///
    /// `build` takes the method separately so a historical walk can be resolved
    /// first; without this guard that flexibility would let a caller produce a
    /// tape nothing could reproduce from the persisted replay inputs.
    #[test]
    fn test_a_method_the_parameters_do_not_name_is_refused() {
        let parameters = parameters(request(10, brownian(0.18), 0.18));
        let mismatched = match SimulationParametersV2::try_from(request(10, garch(0.18), 0.18)) {
            Ok(other) => other.method,
            Err(error) => panic!("the request must convert: {error}"),
        };

        match FactorTape::build(&parameters, &mismatched) {
            Err(ChainError::Internal(reason)) => {
                assert!(reason.contains("does not match"), "{reason}");
            }
            other => panic!("expected an internal error, got {other:?}"),
        }
    }

    /// A resolved historical method is accepted even though its prices and
    /// symbol differ from the stored ones — resolution is what fills those in.
    #[test]
    fn test_a_resolved_historical_method_is_accepted() {
        let prices: Vec<f64> = (0..30).map(|i| 5000.0 + f64::from(i)).collect();
        let mut historical = request(15, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices: Vec::new(),
            symbol: None,
        };
        let parameters = parameters(historical);

        let resolved = SimulationMethod::Historical {
            timeframe: optionstratlib::utils::TimeFrame::Day,
            prices: prices
                .iter()
                .map(|price| match Positive::new(*price) {
                    Ok(value) => value,
                    Err(error) => panic!("the test price must be valid: {error}"),
                })
                .collect(),
            symbol: Some("SPX".to_string()),
        };

        match FactorTape::build(&parameters, &resolved) {
            Ok(tape) => assert_eq!(tape.len(), 15),
            Err(error) => panic!("a resolved historical method must build: {error}"),
        }
    }

    /// A historical series too short for the horizon is a client error naming
    /// the field, not an internal failure.
    #[test]
    fn test_a_short_historical_series_is_a_client_error() {
        let mut historical = request(50, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices: vec![5000.0, 5001.0, 5002.0],
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        match FactorTape::build(&parameters, &parameters.method) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "method.prices");
                assert!(reason.contains("3 supplied for 50 steps"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A historical walk has no model volatility, so the parameters' value is
    /// the base — and the tape builds rather than failing the agreement check.
    #[test]
    fn test_historical_uses_the_requested_volatility_as_the_base() {
        let prices: Vec<f64> = (0..40).map(|i| 5000.0 + f64::from(i)).collect();
        let mut historical = request(20, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices,
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        let tape = tape(&parameters);

        assert_eq!(tape.len(), 20);
        for row in tape.rows() {
            assert_eq!(row.base_volatility, parameters.volatility);
        }
    }

    /// A historical tape replays the supplied series in order, with no
    /// look-ahead: row `i` is the `i`-th observation, never a later one.
    #[test]
    fn test_historical_replays_in_order_without_look_ahead() {
        let prices: Vec<f64> = (0..30).map(|i| 5000.0 + f64::from(i)).collect();
        let mut historical = request(15, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices: prices.clone(),
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        let tape = tape(&parameters);

        for row in tape.rows() {
            let expected = match prices.get(row.step) {
                Some(price) => *price,
                None => panic!("the series must cover step {}", row.step),
            };
            assert_eq!(
                row.spot.to_f64(),
                expected,
                "step {} must replay its own observation",
                row.step
            );
        }
    }

    /// A historical tape rebuilds identically, so an eviction or a restart is
    /// invisible.
    #[test]
    fn test_historical_rebuild_is_deterministic() {
        let prices: Vec<f64> = (0..30).map(|i| 5000.0 + f64::from(i)).collect();
        let mut historical = request(15, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices,
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        assert_eq!(tape(&parameters), tape(&parameters));
    }

    // ---- bounds -----------------------------------------------------------

    /// The tape stores rows, not contracts: its memory is `O(steps)`, and the
    /// chain shape reaches neither its size nor its contents.
    ///
    /// The second assertion is the load-bearing one. Only the seeding chain's
    /// `underlying_price` enters the walk, and upstream copies that verbatim
    /// from the price parameters, so a 200-strike ladder and a default one must
    /// produce the same rows for the same seed. If that ever stops holding, the
    /// chain shape has started perturbing the tape.
    #[test]
    fn test_memory_is_o_steps_and_independent_of_chain_size() {
        let mut wide = request(30, brownian(0.18), 0.18);
        wide.chain_size = Some(200);
        let narrow = request(30, brownian(0.18), 0.18);

        let wide_tape = tape(&parameters(wide));
        let narrow_tape = tape(&parameters(narrow));

        assert_eq!(wide_tape.rows(), narrow_tape.rows());
        assert!(
            std::mem::size_of::<FactorRow>() <= 128,
            "a row is a handful of small fields, got {} bytes",
            std::mem::size_of::<FactorRow>()
        );
    }

    /// The step cap is enforced before a tape is ever built, at the request
    /// boundary, so no oversized tape can be requested.
    #[test]
    fn test_the_step_cap_is_enforced_at_the_boundary() {
        let oversized = request(usize::MAX, brownian(0.18), 0.18);

        match SimulationParametersV2::try_from(oversized) {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "steps"),
            other => panic!("expected the step cap to reject it, got {other:?}"),
        }
    }

    /// A single-step simulation is a valid tape with exactly one row.
    #[test]
    fn test_a_single_step_simulation_builds_one_row() {
        let parameters = parameters(request(1, brownian(0.18), 0.18));

        let tape = tape(&parameters);

        assert_eq!(tape.len(), 1);
        assert_eq!(
            tape.row(0).map(|row| row.spot),
            Some(parameters.initial_price)
        );
    }
}
