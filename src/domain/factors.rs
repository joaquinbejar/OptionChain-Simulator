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
//!
//! # Historical volatility is estimated, causally
//!
//! A `Historical` walk carries no volatility of its own — it is a price series,
//! and `WalkType::volatility()` returns `None` for it. Upstream leaves the
//! per-step estimate to the caller (`WalkTypeAble::generate_with_vol` says so,
//! and returns `vols: None` for the variant); v1's caller is `walk_steps_par`,
//! which estimates it over an expanding window so that step `i` is priced by
//! what had been observed by step `i` and nothing later.
//!
//! v2 never enters that driver, so [`expanding_window_volatilities`] is v2's
//! caller-side answer. It is **composed from public upstream primitives rather
//! than copied out of the private estimator**: upstream's expanding kernel
//! inlines its own prefix-sum variance, but records that the result is
//! algebraically identical to `constant_volatility`, which is public — so the
//! window here writes an indexing policy over upstream mathematics instead of a
//! second copy of it. See that function for the indexing and the cost. A
//! historical tape therefore carries a volatility per step, causally, and the
//! request's own `volatility` field prices none of it (see
//! [`resolve_base_volatility`]).
//!
//! Every value read is inside the horizon, the seeding chain's included: a
//! historical walk replays `prices[..steps]`, and nothing here reduces more
//! than that. A horizon of fewer than three steps has no dispersion to measure
//! and is refused rather than priced from observations it could not have seen.
//!
//! Parity with v1 is numeric, not bit-exact: upstream accumulates the window
//! with prefix sums while `constant_volatility` centres in two passes, and the
//! two agree algebraically but not in the last `Decimal` digits. The tolerance
//! is pinned by a test. ADR 0001 §8 records the contract.

use crate::domain::Walker;
use crate::domain::ladder::PinnedLadder;
use crate::domain::simulator::{DEFAULT_CHAIN_SIZE, DEFAULT_SKEW_SLOPE, DEFAULT_SMILE_CURVE};
use crate::domain::spread::SpreadModel;
use crate::session::{SimulationMethod, SimulationParametersV2};
use crate::utils::ChainError;
use chrono::{DateTime, Utc};
use optionstratlib::ExpirationDate;
use optionstratlib::chains::{
    OptionChainBuildParams, chain::OptionChain, utils::OptionDataPriceParams,
};
use optionstratlib::simulation::steps::{Step, Xstep, Ystep};
use optionstratlib::simulation::{WalkParams, WalkTypeAble};
use optionstratlib::utils::TimeFrame;
use optionstratlib::volatility::{adjust_volatility, constant_volatility};
use positive::Positive;
use rust_decimal::{Decimal, MathematicalOps};
use rust_decimal_macros::dec;
use tracing::{debug, instrument};

/// One step of the market path: everything a snapshot needs that is not an
/// option contract.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Three cases, one field. For a constant-volatility model this is the
    /// model's volatility at every row. For `Garch`, `Heston`, `Custom` and
    /// `Telegraph` it is the annualised volatility prevailing at this step,
    /// index-aligned with `spot`, as upstream's `generate_with_vol` reports it.
    /// For `Historical` it is the realized volatility of the walked prices up
    /// to and including this step, estimated here because upstream leaves it to
    /// the caller — see [`expanding_window_volatilities`].
    pub(crate) base_volatility: Positive,
}

/// The ordered market path of a simulation, one row per requested step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FactorTape {
    rows: Vec<FactorRow>,
}

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
    /// [`resolve_base_volatility`]), when a `Historical` series is too short for
    /// the horizon or carries a zero price, when a volatility — a
    /// stochastic-volatility path's or a historical estimate's — leaves the
    /// range an option chain can be priced at, when a historical window cannot
    /// be reduced or annualised, or when the simulated clock overflows; and
    /// [`ChainError::Internal`] when the resolved method is not the one the
    /// parameters name, when the initial chain cannot be built, or when the
    /// walk returns fewer points than requested.
    ///
    /// A historical simulation is refused *lazily*, when its tape is first
    /// built, because creation does not build one. A series whose realized
    /// volatility leaves the priceable range therefore creates successfully and
    /// fails at the first peek — see ADR 0001 §8.1.
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
        reject_unpriceable_volatility(base_volatility, None, volatility_source(method))?;
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

        // Where each step's volatility comes from, in v1's order of precedence
        // (`walk_steps_par`): a stochastic-volatility model reports its own
        // path, index-aligned with the prices; a historical walk reports none,
        // so it is estimated causally from the path itself; everything else is
        // the model's constant.
        let step_volatilities = match path.vols {
            Some(ref vols) => Some(vols.clone()),
            None => match method {
                SimulationMethod::Historical { timeframe, .. } => {
                    expanding_window_volatilities(&path.prices, *timeframe)?
                }
                _ => None,
            },
        };

        let mut rows = Vec::with_capacity(parameters.steps);
        for step in 0..parameters.steps {
            let spot = *path.prices.get(step).ok_or_else(|| {
                ChainError::Internal(format!("the walk has no price for step {step}"))
            })?;

            let row_volatility = match step_volatilities {
                Some(ref vols) => *vols.get(step).ok_or_else(|| {
                    ChainError::Internal(format!("the walk has no volatility for step {step}"))
                })?,
                None => base_volatility,
            };

            reject_unpriceable_volatility(row_volatility, Some(step), volatility_source(method))?;

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
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the whole-tape accessor the tests compare against; the service reads \
                      one row at a time through `row`"
        )
    )]
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
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "clippy's len_without_is_empty requires this alongside `len`; a built \
                      tape is never empty, since `steps >= 1` is validated at creation"
        )
    )]
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

/// Rejects a historical series that cannot be walked or priced.
///
/// Two client mistakes, both of which would otherwise surface as a `500`:
///
/// - **Too short for the horizon.** Upstream's `historical` kernel errors when
///   the embedded series is shorter than the walk — a request embedding five
///   prices and asking for a hundred steps — so it is caught here and named,
///   the way v1 avoids the problem entirely by refetching from the database.
/// - **A zero price inside the walked window.** A log return divides by the
///   previous price and takes the log of the ratio, and *both* panic on a zero
///   rather than returning an error, so one zero close would take down the
///   thread building the tape. `SimulationParametersV2::validate` already
///   rejects it on the stored series, but the method passed here is the
///   **resolved** one — the whole reason the argument exists is that a
///   `Historical` walk may have been filled in from the database since — and
///   that series has passed no validation at all.
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

    // Only the walked window, for the same reason nothing else reads past it: a
    // zero at index `steps + 50` is touched by no division, no logarithm and no
    // reduction, and refusing the request over it would be exactly the
    // look-ahead this module removed.
    let window = prices
        .get(..parameters.steps)
        .ok_or_else(|| ChainError::Internal("the horizon guard above did not hold".to_string()))?;
    if let Some(index) = window.iter().position(|price| price.is_zero()) {
        return Err(ChainError::Validation {
            field: "method.prices".to_string(),
            reason: format!(
                "must be strictly positive: the price at index {index} is zero, and a log \
                 return divides by the previous price and takes the log of the ratio"
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
/// the series itself is authoritative and the value is **estimated** from it by
/// [`historical_constant_volatility`], exactly as v1 does.
///
/// # What the request's `volatility` means for a `Historical` walk
///
/// Nothing. A price series already prices itself, and inventing a second answer
/// would put the tape and the chains on different numbers again — the very
/// thing this function exists to prevent. The field is not silently swallowed:
/// the values actually used are the per-step ones in every row, which the
/// snapshot and export surfaces report, so a client reads back what priced its
/// chains rather than what it asked for. Dropping the field for this one
/// variant is a DTO change and is deliberately not made here.
///
/// # Only the walked window is read
///
/// The estimate covers `prices[..steps]` — the prefix upstream's historical
/// kernel actually replays — and not the whole embedded series. Reducing the
/// series would let an observation *past* the horizon price the simulation, or
/// refuse it, in a change whose entire point is that nothing later may reach an
/// earlier step. It is a deliberate divergence from v1, which reduces the whole
/// series for its own fallback.
///
/// A consequence worth stating: a horizon shorter than three steps has at most
/// one return and therefore no dispersion to measure, so the estimate is zero
/// and [`reject_unpriceable_volatility`] refuses the simulation. v1 would have
/// priced it from data it could not have seen; issue #63 lists refusing as an
/// accepted answer, and it is the honest one.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] naming `volatility` when a synthetic
/// model's volatility and the parameters' disagree, or `method.prices` when the
/// walked window cannot be reduced to a volatility.
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
        None => match method {
            SimulationMethod::Historical {
                timeframe, prices, ..
            } => {
                // The length is guaranteed by
                // `ensure_historical_series_covers_the_horizon`, which runs
                // first; the checked slice keeps a future reordering from
                // becoming a panic.
                let window =
                    prices
                        .get(..parameters.steps)
                        .ok_or_else(|| ChainError::Validation {
                            field: "method.prices".to_string(),
                            reason: format!(
                                "must carry at least one price per step: {} supplied for {} steps",
                                prices.len(),
                                parameters.steps
                            ),
                        })?;
                historical_constant_volatility(window, *timeframe)
            }
            // `volatility()` returns `None` only for `Historical`; the match is
            // exhaustive over what can reach here, and a future variant that
            // also returns `None` lands on the requested value rather than
            // silently borrowing a historical estimator that does not apply.
            _ => Ok(parameters.volatility),
        },
    }
}

/// The single volatility of a stretch of historical prices, annualised.
///
/// Composed from the same three public upstream functions v1's own whole-series
/// fallback (`walk_driver::walk_volatility`) composes itself from: log returns,
/// the sample standard deviation of those returns, and a rescaling from the
/// series' timeframe to a year.
///
/// Callers pass the walked window, never the whole embedded series — see
/// [`resolve_base_volatility`] for why. It prices the seeding chain, and it is
/// what a window too short for an expanding estimate reduces to.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] naming `volatility` when the reduction or
/// the annualisation fails. Prices are a precondition, not an error case — see
/// [`log_returns`].
fn historical_constant_volatility(
    prices: &[Positive],
    timeframe: TimeFrame,
) -> Result<Positive, ChainError> {
    let returns = log_returns(prices)?;
    let volatility = constant_volatility(&returns).map_err(|e| ChainError::Validation {
        field: "volatility".to_string(),
        reason: format!("the historical series has no usable volatility: {e}"),
    })?;
    annualise(volatility, timeframe)
}

/// The causal expanding-window volatility of a walked price path, one estimate
/// per point.
///
/// # Why this exists
///
/// v1 prices a historical chain at step `i` with the volatility of everything
/// observed **up to** `i`, never the whole series: `walk_steps_par` calls
/// upstream's `expanding_window_vols`. A tape priced at one constant is a
/// different, non-causal simulation — it lets a backtest see, at step 3, the
/// turbulence of step 900. Issue #63.
///
/// # Why it composes rather than copies
///
/// Upstream's three ingredients are public, and upstream's own comment records
/// that its prefix-sum variance is *algebraically identical to the two-pass
/// form in `constant_volatility`*. So the window below writes the indexing
/// policy and the backfill and **no mathematics**: there is no second copy of
/// an upstream kernel in this repo to drift out of sync, which is the property
/// the module docs above claim.
///
/// The cost of composing is quadratic time — `constant_volatility` reduces a
/// whole slice, where upstream's prefix-sum recurrence is linear. It runs once
/// per tape and only for `Historical`; the nine synthetic models never reach
/// it. optionstratlib#423 asked for the estimator itself to be exposed, and
/// 0.20 did expose it as `simulation::expanding_window_vols`, which would
/// collapse this to a single call and erase the quadratic term. Making that
/// swap is a change to the priced tape's provenance, so it belongs to its own
/// issue with its own same-seed comparison, not to a dependency bump.
///
/// # Indexing
///
/// `prices[p]` has seen the returns `returns[..p]`, so an estimate needs
/// `p >= 2` — one return has no dispersion. Points 0 and 1 are backfilled with
/// the first computable estimate, matching upstream, so the vector is aligned
/// index-by-index with `prices` and has no holes. Fewer than three prices leave
/// nothing to expand over and return `None`, which is the caller's signal to
/// fall back to the constant.
///
/// The `p >= 2` guard is explicit rather than delegated: `constant_volatility`
/// answers `Positive::ZERO` for a shorter slice instead of refusing, and a
/// zero volatility is an answer, not an absence.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] when a window cannot be reduced or
/// annualised. Prices are a precondition, not an error case — see
/// [`log_returns`].
fn expanding_window_volatilities(
    prices: &[Positive],
    timeframe: TimeFrame,
) -> Result<Option<Vec<Positive>>, ChainError> {
    // Two prices give one return, which has no sample dispersion; the first
    // window that does is the one over `prices[..3]`.
    if prices.len() < 3 {
        return Ok(None);
    }

    let returns = log_returns(prices)?;
    let mut volatilities = Vec::with_capacity(prices.len());
    let mut first_computable: Option<Positive> = None;

    for point in 0..prices.len() {
        if point < 2 {
            // Backfilled below, once the first real estimate is known.
            continue;
        }

        let window = returns.get(..point).ok_or_else(|| ChainError::Validation {
            field: "method.prices".to_string(),
            reason: format!(
                "the historical series yielded {} returns for {} prices, too few to \
                     estimate the volatility at point {point}",
                returns.len(),
                prices.len()
            ),
        })?;

        let volatility = constant_volatility(window).map_err(|e| ChainError::Validation {
            field: "volatility".to_string(),
            reason: format!("the historical window ending at point {point} has no volatility: {e}"),
        })?;
        let annualised = annualise(volatility, timeframe)?;

        if first_computable.is_none() {
            first_computable = Some(annualised);
        }
        volatilities.push(annualised);
    }

    let Some(fill) = first_computable else {
        // Unreachable: `prices.len() >= 3` guarantees one pass through the loop
        // body. Answering `None` rather than asserting keeps a future change to
        // the guard above from turning a bad bound into a panic.
        return Ok(None);
    };

    // Points 0 and 1 carry the first computable estimate, so the vector aligns
    // with `prices` index by index.
    let mut aligned = vec![fill; 2];
    aligned.append(&mut volatilities);
    Ok(Some(aligned))
}

/// The log returns of a price series as decimals.
///
/// Computed here rather than through upstream's `calculate_log_returns`, which
/// divides two `Positive`s: that operator panics on a zero divisor **and on a
/// ratio it cannot represent**, and it never returns an error for either. Both
/// operands come straight from a request, and a `Decimal` holds about 29
/// significant digits, so a series stepping from `1e-28` to `7e28` is a
/// perfectly legal pair of strictly positive prices whose ratio is not
/// representable. A request must not be able to abort the process, so the
/// division is checked and an unrepresentable ratio is a rejection naming the
/// field.
///
/// The log itself is taken on the `Decimal` rather than on the ratio's
/// `Positive`. Since `positive` 0.6 `Positive::ln` returns a `Decimal` and so
/// carries the sign correctly on its own, but taking it here keeps the checked
/// division and the checked log in one expression, each with its own
/// rejection.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] naming `method.prices` when a ratio is
/// not representable, when a price is zero, or when the log of a ratio is not
/// representable.
fn log_returns(prices: &[Positive]) -> Result<Vec<Decimal>, ChainError> {
    let unusable = |reason: String| ChainError::Validation {
        field: "method.prices".to_string(),
        reason,
    };

    let mut returns = Vec::with_capacity(prices.len().saturating_sub(1));
    for (index, pair) in prices.windows(2).enumerate() {
        let [previous, current] = pair else {
            // `windows(2)` yields pairs; destructuring keeps it indexing-free.
            continue;
        };

        let previous = previous.to_dec();
        let current = current.to_dec();
        if previous.is_zero() {
            return Err(unusable(format!(
                "the price at index {index} is zero, so the series has no return at {}",
                index + 1
            )));
        }

        let ratio = current.checked_div(previous).ok_or_else(|| {
            unusable(format!(
                "the price ratio at index {} is not representable ({current} over {previous})",
                index + 1
            ))
        })?;

        let log = ratio.checked_ln().ok_or_else(|| {
            unusable(format!(
                "the log return at index {} is not representable (ratio {ratio})",
                index + 1
            ))
        })?;

        returns.push(log);
    }

    Ok(returns)
}

/// Rescales a volatility from the series' timeframe to a year.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] naming `volatility` when the rescaling
/// fails.
fn annualise(volatility: Positive, timeframe: TimeFrame) -> Result<Positive, ChainError> {
    adjust_volatility(volatility, timeframe, TimeFrame::Year).map_err(|e| ChainError::Validation {
        field: "volatility".to_string(),
        reason: format!("the historical volatility cannot be annualised from {timeframe}: {e}"),
    })
}

/// Which of the two a walk method's volatility comes from.
fn volatility_source(method: &SimulationMethod) -> VolatilitySource {
    match method {
        SimulationMethod::Historical { .. } => VolatilitySource::Series,
        _ => VolatilitySource::Model,
    }
}

/// Where a volatility that failed the priceable range came from, so the error
/// names a field the client can actually act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolatilitySource {
    /// The walk model's own volatility, or the request's, which must agree.
    Model,
    /// Estimated from a historical price series, where the request's
    /// `volatility` prices nothing and lowering it would change nothing.
    Series,
}

impl VolatilitySource {
    /// The request field to name.
    fn field(self) -> &'static str {
        match self {
            Self::Model => "volatility",
            Self::Series => "method.prices",
        }
    }

    /// What the client has to change to get under the upper bound.
    fn remedy_too_high(self) -> &'static str {
        match self {
            Self::Model => "lower the model's volatility or shorten the horizon",
            Self::Series => Self::SERIES_REMEDY,
        }
    }

    /// What the client has to change to get off zero. Telling a model to lower
    /// its volatility here would make the zero *more* likely, not less.
    fn remedy_zero(self) -> &'static str {
        match self {
            Self::Model => {
                "raise the model's volatility, or change the parameters that let its variance \
                 collapse to zero"
            }
            Self::Series => Self::SERIES_REMEDY,
        }
    }

    /// The same either way: the request's `volatility` is not the input.
    const SERIES_REMEDY: &'static str = "the volatility is estimated from the series, so it is the series that has to change — \
         the request's volatility prices nothing for a historical walk";
}

/// Rejects a volatility no chain can be priced at, for a caller outside the
/// factor tape.
///
/// The v2 path reaches [`reject_unpriceable_volatility`] through
/// `FactorTape::build`, which validates every step of the walk. v1 has no tape
/// and validates its parameters at the request boundary instead, so it needs the
/// same bound applied at the same place — otherwise a session created with a
/// volatility of `1e-13` builds fine and PANICS the first time a client asks for
/// greeks, inside upstream's `vomma`.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] naming `field` when the volatility is
/// zero, above one, or below the adjusted floor.
pub(crate) fn validate_priceable_volatility(
    volatility: Positive,
    field: &str,
) -> Result<(), ChainError> {
    reject_unpriceable_volatility(volatility, None, VolatilitySource::Model).map_err(|error| {
        match error {
            // The shared guard names the walk model's field; a caller checking a
            // request field wants its own name on the error.
            ChainError::Validation { reason, .. } => ChainError::Validation {
                field: field.to_string(),
                reason,
            },
            other => other,
        }
    })
}

/// The smallest EFFECTIVE per-strike volatility a chain can be priced at.
///
/// Not where pricing stops being interesting, but where upstream's greek
/// arithmetic stops fitting in a `Decimal`: `vomma` is `vega * d1 * d2 / iv`,
/// and as `iv` goes to zero that quotient overflows and `Positive` PANICS
/// inside `OptionChain::build_chain`, on a request that passed every field
/// check — the model's own volatility is only checked for zero and for
/// exceeding one.
///
/// Measured: `1e-11` panics, while `3e-11` and `5e-11` do not. The boundary is
/// not sharp, because it depends on `d1 * d2` as well as on `iv`, which is why
/// this is a floor with margin rather than an attempt to predict the overflow.
/// `1e-9` sits two orders of magnitude above the lowest observed overflow.
const MIN_PRICEABLE_VOLATILITY: Decimal = dec!(0.000000001);

/// The floor upstream clamps the per-strike skew and smile multiplier to.
///
/// `adjust_volatility` ends in `factor.clamp(0.01, 3.0)`, so a strike's
/// EFFECTIVE volatility is never less than a hundredth of the base — but it can
/// be exactly that, under a steep enough skew or smile. Checking only the base
/// would therefore admit a base of `1e-9` that prices strikes at `1e-11`, which
/// is precisely the overflow region above.
///
/// Mirrored here rather than computed: this is the one upstream constant the
/// guard depends on, and a change to it is a change to what this service must
/// reject. If upstream lowers the clamp, the tests below start failing, which is
/// the intended way to find out.
const SKEW_SMILE_VOLATILITY_FLOOR: Decimal = dec!(0.01);

/// The smallest BASE volatility whose worst-case strike still prices.
///
/// The base a client supplies is not what a strike is priced at: the skew and
/// smile scale it per strike, down to [`SKEW_SMILE_VOLATILITY_FLOOR`] of it. The
/// bound a request is checked against is therefore the effective floor divided
/// by that clamp — `1e-7` — so that even the most aggressively shaped strike in
/// the chain lands above [`MIN_PRICEABLE_VOLATILITY`].
///
/// A historical walk estimates its volatility from the client's series and can
/// legitimately be small; this repo's own fixture reaches `4.5e-7`, which still
/// clears this bound.
#[must_use]
fn min_priceable_base_volatility() -> Decimal {
    MIN_PRICEABLE_VOLATILITY
        .checked_div(SKEW_SMILE_VOLATILITY_FLOOR)
        .unwrap_or(MIN_PRICEABLE_VOLATILITY)
}

/// Rejects a volatility no option chain can be priced at.
///
/// Upstream refuses anything above 1.0 annualised **and anything equal to
/// zero**, so without this a run whose volatility leaves that range fails with
/// an internal error the first time a chain is built — at creation for a
/// constant model, and halfway through the horizon for a stochastic or
/// historical one, at whatever step crosses first. All of them become one 400
/// naming a field, at tape build, where the whole path is in hand.
///
/// The lower bound is not theoretical for a historical walk, and it fires on
/// **a flat opening, not only a flat series**: the estimate needs two returns,
/// so points 0 and 1 carry the first computable one, and if the first three
/// prices are equal that value is zero. A series that is flat for three ticks
/// and lively afterwards is refused at step 0. v1 fails on the same input, from
/// inside the chain builder and as a `500`; this is the same refusal with a
/// status and a field a client can act on.
///
/// Rejecting rather than clamping: a clamped path is a different tape, and the
/// seed would no longer reproduce it.
fn reject_unpriceable_volatility(
    volatility: Positive,
    step: Option<usize>,
    source: VolatilitySource,
) -> Result<(), ChainError> {
    if volatility.is_zero() {
        return Err(ChainError::Validation {
            field: source.field().to_string(),
            reason: format!(
                "the volatility is zero{}, and an option chain priced at zero volatility is a \
                 chain of zero-value options; {}",
                at_step(step),
                source.remedy_zero()
            ),
        });
    }

    let floor = min_priceable_base_volatility();
    if volatility < floor {
        return Err(ChainError::Validation {
            field: source.field().to_string(),
            reason: format!(
                "the volatility falls to {volatility}{}, below the {floor} floor an option chain \
                 can be priced at — the skew and smile scale it per strike down to a hundredth, \
                 and {MIN_PRICEABLE_VOLATILITY} is where the greek arithmetic overflows; {}",
                at_step(step),
                source.remedy_zero()
            ),
        });
    }

    if volatility <= Positive::ONE {
        return Ok(());
    }

    Err(ChainError::Validation {
        field: source.field().to_string(),
        reason: format!(
            "the volatility reaches {volatility}{}, above the 1.0 maximum an option chain can \
             be priced at; {}",
            at_step(step),
            source.remedy_too_high()
        ),
    })
}

/// Names the step in an error, when there is one. Built only on the failing
/// path — the check above runs once per row.
fn at_step(step: Option<usize>) -> String {
    match step {
        Some(step) => format!(" at step {step}"),
        None => String::new(),
    }
}

/// Builds one option chain from a simulation's shape and a point in the market
/// path.
///
/// The single place the v2 stack turns parameters into an
/// [`OptionChainBuildParams`], so the factor tape's seeding chain and every
/// snapshot chain apply the same defaults, the same decimal precision and the
/// same volume — and so there is one place to look when a chain does not come
/// out as expected.
///
/// Mirrors v1's wiring in [`crate::domain::Simulator`] field for field, which
/// is what makes the seeding chain identical to the one v1 would build.
///
/// # A field upstream derives from the host clock
///
/// The chain is stamped with a `YYYY-MM-DD` expiration string that upstream
/// computes in `ExpirationDate::get_date_string()`. For the `Days` variant that
/// goes through `get_date_with_options(true)`, which reads `Utc::now()` and
/// **ignores** the thread-local reference, so the stamp reflects the host's
/// calendar rather than the simulated one and there is no upstream hook to
/// change that.
///
/// Everything that reaches a price is unaffected: `get_years()` divides the
/// `Days` value directly and `get_days()` returns it verbatim, so premiums,
/// Greeks and the derived strike interval are pure functions of the value
/// passed here. The stamp is therefore treated as upstream metadata that the
/// v2 surface does not expose — the authoritative expiration a client sees is
/// the absolute `expires_at` the planner produced.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] when upstream cannot build the chain.
pub(crate) fn build_chain(
    parameters: &SimulationParametersV2,
    spot: Positive,
    volatility: Positive,
    expiration: ExpirationDate,
    greek_snapshots: bool,
) -> Result<OptionChain, ChainError> {
    // A pinned simulation quotes the ladder it fixed at creation, so the chain
    // is built wide enough to REACH those strikes from wherever the spot is now
    // and filtered down to them afterwards. Upstream anchors every ladder on
    // the same interval grid, so the pinned strikes are always among the ones
    // it builds. See `crate::domain::ladder`.
    let pinned = if parameters.strike_ladder.is_pinned() {
        Some(PinnedLadder::resolve(parameters)?)
    } else {
        None
    };

    let chain_size = match &pinned {
        // The ceiling was fixed at creation and travels with the parameters,
        // so it is the same on every instance that ever serves this session:
        // reading it from the environment here would let the same seed run to
        // completion on one deployment and refuse at step k on another.
        Some(ladder) => ladder.width_from(spot, parameters.pinned_width_ceiling)?,
        None => parameters.chain_size.unwrap_or(DEFAULT_CHAIN_SIZE),
    };
    let skew_slope = parameters.skew_slope.unwrap_or(DEFAULT_SKEW_SLOPE);
    let smile_curve = parameters.smile_curve.unwrap_or(DEFAULT_SMILE_CURVE);
    // The spread model, not one scalar. Read once per chain: every coefficient
    // is fixed for a simulation's lifetime, and only the contract varies.
    let spread_model = SpreadModel::from_parameters(parameters);
    // Propagated, not defaulted: this now feeds a PRICED quantity (the tenor
    // term), and silently reading zero days would narrow every spread to its
    // floor. It is infallible for the `Days` variant every caller passes, and
    // the `DateTime` variant reads the wall clock, which a served quote must
    // not depend on.
    let days_to_expiration = expiration
        .get_days()
        .map_err(|e| {
            ChainError::Internal(format!(
                "the chain's days to expiration are unavailable: {e}"
            ))
        })?
        .to_dec();

    let price_params = OptionDataPriceParams::new(
        Some(Box::new(spot)),
        Some(expiration),
        Some(parameters.risk_free_rate),
        Some(parameters.dividend_yield),
        Some(parameters.symbol.clone()),
    );

    // `greek_snapshots` asks upstream for the full twelve-value set per style
    // (issue #74). It is the CALLER's decision, not this function's, because
    // only the caller knows whether anything will read them: the manager turns
    // it on when a warehouse is registered, since a persisted tape that lost
    // everything past delta and gamma would make an exported step strictly
    // poorer than a live one. A deployment with no warehouse and no client
    // asking for greeks must not pay for them.
    //
    // Measured at about 1.54x a plain chain build. Nothing served changes
    // either way: upstream computes `delta` and `gamma` the same way on both
    // branches — `calculate_greeks` calls `calculate_delta` and
    // `calculate_gamma` first — so the convenience mirrors the default response
    // reads are identical, and the seeded tape is untouched.
    // `test_the_greek_snapshots_do_not_move_the_priced_chain` pins that.
    let build_params = OptionChainBuildParams::new(
        parameters.symbol.clone(),
        Some(Positive::ONE),
        chain_size,
        parameters.strike_interval,
        skew_slope,
        smile_curve,
        // ZERO, and the widening happens below instead. The spread model here
        // is per contract, so a single scalar handed to upstream would be the
        // wrong number for every strike but one; asking for no spread leaves
        // each mid untouched, which is what the model works from.
        //
        // It also used to be load-bearing against OptionStratLib#439, where
        // `apply_spread` withdrew a quote whose mid fell below the spread.
        // That is fixed as of 0.21.1, which this pins, so the zero is now
        // about owning the widening rather than about avoiding an erasure.
        Positive::ZERO,
        2,
        price_params,
        volatility,
    )
    .with_greek_snapshots(greek_snapshots);

    let mut chain = OptionChain::build_chain(&build_params)
        .map_err(|e| ChainError::Internal(format!("Failed to build the option chain: {e}")))?;

    // Down to the pinned strikes, before the spread model runs: the widened
    // strikes exist only to reach the pinned ones and nothing should pay to
    // quote them.
    if let Some(ladder) = &pinned {
        ladder.keep_pinned(&mut chain)?;
    }

    spread_model.apply(&mut chain, days_to_expiration);

    Ok(chain)
}

/// Builds the single option chain that seeds the walk.
///
/// The seeding chain needs *an* expiration; the real ones come from the
/// planner, per snapshot. One day keeps it well-conditioned, and the value
/// never reaches a served price — the walk starts from the chain's
/// `underlying_price`, which upstream copies verbatim from the price
/// parameters, so this nominal expiry changes the seeding ladder's shape and
/// nothing else.
fn build_initial_chain(
    parameters: &SimulationParametersV2,
    base_volatility: Positive,
) -> Result<OptionChain, ChainError> {
    // Never with greeks: this chain seeds the walk and is never served or
    // filed, so pricing a full set for it would be pure waste.
    build_chain(
        parameters,
        parameters.initial_price,
        base_volatility,
        ExpirationDate::Days(Positive::ONE),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
    use crate::api::rest::requests_v2::CreateSimulationRequest;
    use crate::domain::ladder::StrikeLadder;
    use crate::session::{ExpiryRule, ExpiryRuleKind};
    use chrono::{TimeZone, Weekday};
    use optionstratlib::error::SimulationError;
    use optionstratlib::simulation::walk_steps_par;
    use positive::pos_or_panic;
    use std::sync::Mutex;

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
            strike_ladder: Default::default(),
            spread_proportional: None,
            spread_moneyness_widening: None,
            spread_tenor_widening: None,
            spread_tick: None,
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

    /// Asking upstream for the greek snapshots does not move a single served
    /// number.
    ///
    /// Issue #74 turned `with_greek_snapshots` on so a persisted tape carries
    /// the full set. That flag also changes which upstream branch prices the
    /// chain, so the claim that nothing served moved has to be checked rather
    /// than assumed: `delta_call`, `delta_put` and `gamma` are what the default
    /// response and the pre-#74 warehouse rows are built from, and a shift in
    /// any of them would be a silent tape break for every existing client.
    #[test]
    fn test_the_greek_snapshots_do_not_move_the_priced_chain() {
        let parameters = parameters(request(4, brownian(0.18), 0.18));
        let expiration = ExpirationDate::Days(pos_or_panic!(7.0));
        let spot = pos_or_panic!(5000.0);
        let volatility = pos_or_panic!(0.18);

        let with_snapshots = match build_chain(&parameters, spot, volatility, expiration, true) {
            Ok(chain) => chain,
            Err(error) => panic!("the chain must build: {error}"),
        };

        // The same call with the flag off, through the function under test —
        // not a hand-assembled `OptionChainBuildParams`, which would keep
        // comparing against a chain the service no longer builds the day
        // `build_chain` changes.
        let without_snapshots = match build_chain(&parameters, spot, volatility, expiration, false)
        {
            Ok(chain) => chain,
            Err(error) => panic!("the comparison chain must build: {error}"),
        };

        let served: Vec<_> = with_snapshots
            .iter()
            .map(|data| {
                (
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
                )
            })
            .collect();
        let baseline: Vec<_> = without_snapshots
            .iter()
            .map(|data| {
                (
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
                )
            })
            .collect();

        assert!(!served.is_empty(), "the fixture chain must quote something");
        assert_eq!(served, baseline, "the flag must not move a served value");

        // And the flag did what it was turned on for.
        assert!(
            with_snapshots
                .iter()
                .all(|data| data.greeks_call.is_some() && data.greeks_put.is_some()),
            "every strike must carry both snapshots"
        );
        assert!(
            without_snapshots
                .iter()
                .all(|data| data.greeks_call.is_none()),
            "the comparison build must carry none"
        );
    }

    /// The warehouse stores each side's snapshot `delta` in the SAME column as
    /// upstream's convenience mirror, so the two must agree at every strike.
    ///
    /// Upstream computes them independently and says so, which is exactly why
    /// this is pinned rather than assumed: if a release made the mirror and the
    /// snapshot diverge, `simulation_option_quotes` would silently report one
    /// where it promised the other, and no round-trip test would notice —
    /// every fixture sets them equal by hand.
    #[test]
    fn test_the_delta_mirror_equals_the_snapshot_delta_at_every_strike() {
        // Several regimes, not one point: upstream warns that the mirrors stay
        // defined at expiry and at zero volatility where the full set does not,
        // so a sub-day expiry and a very low volatility are exactly where the
        // two could part company.
        let regimes = [
            (0.18, 7.0),
            (0.18, 0.5),
            (0.9, 30.0),
            (0.01, 7.0),
            (0.05, 0.25),
        ];

        let mut compared = 0_usize;
        for (volatility, days) in regimes {
            let parameters = parameters(request(4, brownian(volatility), volatility));
            let chain = match build_chain(
                &parameters,
                pos_or_panic!(5000.0),
                match Positive::new_decimal(match Decimal::try_from(volatility) {
                    Ok(value) => value,
                    Err(error) => panic!("the fixture volatility must convert: {error}"),
                }) {
                    Ok(value) => value,
                    Err(error) => panic!("the fixture volatility must be positive: {error}"),
                },
                ExpirationDate::Days(
                    match Positive::new_decimal(match Decimal::try_from(days) {
                        Ok(value) => value,
                        Err(error) => panic!("the fixture expiry must convert: {error}"),
                    }) {
                        Ok(value) => value,
                        Err(error) => panic!("the fixture expiry must be positive: {error}"),
                    },
                ),
                true,
            ) {
                Ok(chain) => chain,
                Err(error) => panic!("the chain must build at {volatility}/{days}: {error}"),
            };

            for data in chain.iter() {
                assert_eq!(
                    data.delta_call,
                    data.greeks_call.as_ref().map(|greeks| greeks.delta),
                    "the call mirror and its snapshot disagree at strike {}",
                    data.strike_price
                );
                assert_eq!(
                    data.delta_put,
                    data.greeks_put.as_ref().map(|greeks| greeks.delta),
                    "the put mirror and its snapshot disagree at strike {}",
                    data.strike_price
                );
                assert_eq!(
                    data.gamma,
                    data.greeks_call.as_ref().map(|greeks| greeks.gamma),
                    "the gamma mirror and its snapshot disagree at strike {}",
                    data.strike_price
                );
                // The shape the storage cannot represent: a mirror without a
                // snapshot would be a strike whose `delta_call` column is
                // written while its other eleven greek columns are NULL, which
                // reads back as no snapshot and silently drops the delta the
                // column does hold.
                assert!(
                    data.delta_call.is_none() || data.greeks_call.is_some(),
                    "a mirror without a snapshot at strike {}",
                    data.strike_price
                );
                compared += 1;
            }
        }
        assert!(compared > 0, "the fixture chains must quote something");
    }

    /// A volatility small enough to overflow upstream's greek arithmetic is
    /// rejected, not priced.
    ///
    /// `vomma` is `vega * d1 * d2 / iv`, so below roughly `1e-11` that quotient
    /// leaves `Decimal`'s range and `Positive` PANICS inside
    /// `OptionChain::build_chain` — on a tokio worker, from a request that
    /// passed every field-level check, because those only reject zero and
    /// values above one.
    ///
    /// It fires only when the full greek set is computed, so it is reachable
    /// through `?greeks=` on any deployment and, since issue #74, through the
    /// DEFAULT path of a warehouse-backed one. The floor closes both.
    #[test]
    fn test_a_volatility_below_the_pricing_floor_is_rejected() {
        for tiny in [1e-10, 1e-11, 1e-13] {
            let request = request(4, brownian(tiny), tiny);
            let parameters = match SimulationParametersV2::try_from(request) {
                Ok(parameters) => parameters,
                // Rejected even earlier, which is also a rejection.
                Err(ChainError::Validation { field, .. }) => {
                    assert_eq!(field, "volatility", "for {tiny:e}");
                    continue;
                }
                Err(error) => panic!("expected a validation failure, got {error:?}"),
            };

            match FactorTape::build(&parameters, &parameters.method) {
                Ok(_) => panic!("a volatility of {tiny:e} must be rejected"),
                Err(ChainError::Validation { field, reason }) => {
                    assert_eq!(field, "volatility", "for {tiny:e}");
                    assert!(
                        reason.contains("floor"),
                        "the reason must name the floor, got {reason}"
                    );
                }
                Err(error) => panic!("expected a validation failure, got {error:?}"),
            }
        }
    }

    /// The worst strike an extreme skew and smile can produce still prices.
    ///
    /// The base volatility is not what a strike is priced at. Upstream scales it
    /// per strike and clamps the multiplier at a hundredth, so a base that
    /// passed a naive check could still hand `vomma` a `1e-11` and panic. This
    /// drives the skew and the smile to the edges of their validated ranges at
    /// the smallest base the guard admits, and asserts the chain builds with the
    /// full greek set — which is the code path that overflows.
    #[test]
    fn test_the_smallest_admitted_base_prices_under_an_extreme_skew_and_smile() {
        let floor = min_priceable_base_volatility();
        let volatility = match Positive::new_decimal(floor) {
            Ok(volatility) => volatility,
            Err(error) => panic!("the floor must be a valid volatility: {error}"),
        };

        for (skew, smile) in [
            (dec!(-1.0), dec!(-1.0)),
            (dec!(1.0), dec!(1.0)),
            (dec!(-1.0), dec!(1.0)),
            (dec!(1.0), dec!(-1.0)),
        ] {
            let mut parameters = parameters(request(4, brownian(0.18), 0.18));
            parameters.skew_slope = Some(skew);
            parameters.smile_curve = Some(smile);
            parameters.chain_size = Some(60);

            match build_chain(
                &parameters,
                pos_or_panic!(5000.0),
                volatility,
                ExpirationDate::Days(pos_or_panic!(7.0)),
                true,
            ) {
                Ok(_) => {}
                Err(error) => panic!("skew {skew} smile {smile} must still price: {error}"),
            }
        }
    }

    /// A base just under the floor is rejected, and the message explains why the
    /// bound is a hundred times the value that actually overflows.
    #[test]
    fn test_a_base_below_the_adjusted_floor_names_the_skew() {
        let parameters = parameters(request(4, brownian(0.18), 0.18));
        let too_small = match Positive::new_decimal(dec!(0.00000001)) {
            Ok(value) => value,
            Err(error) => panic!("the fixture must be positive: {error}"),
        };

        match reject_unpriceable_volatility(too_small, None, VolatilitySource::Model) {
            Ok(()) => panic!("a base of 1e-8 prices strikes at 1e-10 and must be rejected"),
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "volatility");
                assert!(reason.contains("skew"), "the reason must explain: {reason}");
            }
            Err(error) => panic!("expected a validation failure, got {error:?}"),
        }
        let _ = parameters;
    }

    /// The floor sits well below anything a real walk produces, so it rejects
    /// nothing a client could have used.
    ///
    /// Two witnesses: a chain at the reference configuration already serves no
    /// valid strike below about `5e-3`, six orders of magnitude above the
    /// floor; and the historical fixtures in this module estimate volatilities
    /// as low as `4.5e-7`, still 450 times above it, and still build.
    #[test]
    fn test_the_pricing_floor_admits_every_usable_volatility() {
        let parameters = parameters(request(4, brownian(0.005), 0.005));
        let chain = match build_chain(
            &parameters,
            pos_or_panic!(5000.0),
            pos_or_panic!(0.005),
            ExpirationDate::Days(pos_or_panic!(7.0)),
            true,
        ) {
            Ok(chain) => chain,
            Err(error) => panic!("a usable volatility must still price: {error}"),
        };

        assert!(
            chain.iter().count() > 0,
            "the lowest volatility above the floor must still quote a strike"
        );
    }

    // ---- the strike ladder (issue #91) ------------------------------------

    /// A pinned ladder quotes the same strikes however far the spot moves.
    ///
    /// The criterion the issue is about: a position opened at step 0 must still
    /// have its contracts at step 499. Driven at spots that move well past the
    /// pinned range in both directions, since that is precisely when a rolling
    /// ladder drops the strikes a defined-risk structure needs.
    #[test]
    fn test_a_pinned_ladder_quotes_one_strike_set_at_every_spot() {
        let mut request = request(4, brownian(0.2), 0.2);
        request.chain_size = Some(6);
        request.strike_interval = Some(25.0);
        request.strike_ladder = Some(StrikeLadder::Pinned);
        let parameters = parameters(request);

        let expected = strike_set(&parameters, 5000.0);
        assert_eq!(expected.len(), 13, "a half-width of six is 13 strikes");

        for spot in [5000.0, 5004.95, 5013.54, 4800.0, 5400.0, 4000.0, 6500.0] {
            assert_eq!(
                strike_set(&parameters, spot),
                expected,
                "the pinned ladder moved at a spot of {spot}"
            );
        }
    }

    /// A rolling ladder does move, which is what the pinned one exists to fix.
    ///
    /// Without this the test above could pass on a ladder that never moves for
    /// any configuration, and would be proving nothing.
    #[test]
    fn test_a_rolling_ladder_follows_the_spot() {
        let mut request = request(4, brownian(0.2), 0.2);
        request.chain_size = Some(6);
        request.strike_interval = Some(25.0);
        let parameters = parameters(request);

        let at_the_start = strike_set(&parameters, 5000.0);
        let after_a_move = strike_set(&parameters, 5013.54);

        assert_ne!(
            at_the_start, after_a_move,
            "a rolling ladder must follow the spot, which is the reported bug"
        );
        assert_eq!(at_the_start.len(), after_a_move.len(), "the width is fixed");
    }

    /// A pinned chain carries the same prices a rolling one gives those strikes.
    ///
    /// Pinning decides WHICH contracts are quoted, never what they are worth:
    /// the chain is built by upstream at the same spot either way, and the
    /// pinned one is that chain with the other strikes removed.
    #[test]
    fn test_pinning_does_not_change_a_quote() {
        let mut rolling = request(4, brownian(0.2), 0.2);
        rolling.chain_size = Some(6);
        rolling.strike_interval = Some(25.0);
        let mut pinned = rolling.clone();
        pinned.strike_ladder = Some(StrikeLadder::Pinned);

        // At the starting spot the two ladders agree on the strike set, so
        // every contract can be compared one to one.
        let rolling = built_chain(&parameters(rolling));
        let pinned = built_chain(&parameters(pinned));

        assert_eq!(
            rolling.options, pinned.options,
            "pinning must not touch a price"
        );
    }

    /// The strikes a chain quotes at one spot.
    fn strike_set(parameters: &SimulationParametersV2, spot: f64) -> Vec<String> {
        let chain = match build_chain(
            parameters,
            pos_or_panic!(spot),
            pos_or_panic!(0.2),
            ExpirationDate::Days(pos_or_panic!(30.0)),
            false,
        ) {
            Ok(chain) => chain,
            Err(error) => panic!("the chain must build at {spot}: {error}"),
        };
        chain
            .iter()
            .map(|contract| contract.strike_price.to_string())
            .collect()
    }

    // ---- the per-contract spread model (issue #80) ------------------------

    /// Two contracts of one chain carry DIFFERENT absolute spreads.
    ///
    /// The property a single scalar cannot express, and the reason the model
    /// exists: a dear in-the-money call and a five-cent wing do not cost the
    /// same to cross.
    #[test]
    fn test_two_contracts_of_one_chain_carry_different_spreads() {
        let mut request = request(4, brownian(0.2), 0.2);
        request.chain_size = Some(12);
        request.spread = Some(0.02);
        request.spread_proportional = Some(0.02);
        request.spread_moneyness_widening = Some(0.5);
        let chain = built_chain(&parameters(request));

        let spreads: Vec<Decimal> = chain
            .iter()
            .filter_map(|contract| match (contract.call_bid, contract.call_ask) {
                (Some(bid), Some(ask)) => Some(ask.to_dec() - bid.to_dec()),
                _ => None,
            })
            .collect();

        assert!(spreads.len() > 2, "the chain must quote several strikes");
        let first = spreads[0];
        assert!(
            spreads.iter().any(|spread| *spread != first),
            "every contract carries the same spread: {spreads:?}"
        );
    }

    /// Contracts cheaper than the spread are quoted, where they used to vanish.
    ///
    /// The assertion is on the COUNT, deliberately. "Every contract with a mid
    /// has a bid" passes on the old code too, because the old code cleared the
    /// mid along with the quote: the erased contracts were skipped rather than
    /// caught. What actually moved is how many contracts are quoted at all.
    #[test]
    fn test_contracts_cheaper_than_the_spread_are_quoted() {
        let mut request = request(4, brownian(0.2), 0.2);
        request.chain_size = Some(30);
        // A spread far above the cheap wings' mid, which is the configuration
        // that used to erase them.
        request.spread = Some(5.0);
        let chain = built_chain(&parameters(request));

        let quoted = chain
            .iter()
            .filter(|contract| contract.call_bid.is_some() && contract.call_ask.is_some())
            .count();
        // What the old path would have kept: only the contracts worth strictly
        // more than the whole spread.
        let above_the_spread = chain
            .iter()
            .filter(|contract| {
                contract
                    .call_middle
                    .is_some_and(|mid| mid.to_dec() > dec!(5))
            })
            .count();

        assert!(above_the_spread > 0, "the chain must quote dear contracts");
        assert!(
            quoted > above_the_spread,
            "the cheap wings must be quoted too: {quoted} quoted, {above_the_spread} above the spread"
        );

        for contract in chain.iter() {
            if contract.call_middle.is_some() {
                assert!(
                    contract.call_bid.is_some() && contract.call_ask.is_some(),
                    "a call with a mid must be quoted: {contract:?}"
                );
            }
            if contract.put_middle.is_some() {
                assert!(
                    contract.put_bid.is_some() && contract.put_ask.is_some(),
                    "a put with a mid must be quoted: {contract:?}"
                );
            }
        }
    }

    /// A far out-of-the-money wing at one day to expiry is still quoted.
    ///
    /// The exact case that broke: at 1 DTE the far wings are worth cents, so a
    /// two-cent spread erased them. The furthest strike of all can be worth
    /// less than a tick, and it is quoted `0.00 / 0.01` rather than bid a penny
    /// it is not worth — marking a worthless wing at the tick is the same bias,
    /// in the same direction, that the model exists to remove.
    #[test]
    fn test_a_far_wing_at_one_day_is_still_quoted() {
        let mut request = request(4, brownian(0.2), 0.2);
        request.chain_size = Some(40);
        request.spread = Some(0.02);
        let parameters = parameters(request);
        let chain = match build_chain(
            &parameters,
            pos_or_panic!(5000.0),
            pos_or_panic!(0.2),
            ExpirationDate::Days(pos_or_panic!(1.0)),
            false,
        ) {
            Ok(chain) => chain,
            Err(error) => panic!("a one-day chain must build: {error}"),
        };

        // The LAST strike of the chain, not the last one that happens to carry
        // a mid: picking the latter would select a contract the old code kept
        // anyway, and the test would pass without the fix.
        let wing = match chain.iter().last() {
            Some(contract) => contract,
            None => panic!("the chain must carry a highest strike"),
        };

        assert!(
            wing.call_middle.is_some(),
            "the furthest strike must still be priced: {wing:?}"
        );
        match (wing.call_bid, wing.call_ask, wing.call_middle) {
            (Some(bid), Some(ask), Some(mid)) => {
                assert!(
                    ask >= pos_or_panic!(0.01),
                    "the wing must be offered at or above the tick, got {ask}"
                );
                assert!(bid <= ask, "and the book must not be crossed");
                assert!(
                    bid.to_dec() <= mid.to_dec().round_dp(2),
                    "a wing must not be bid above what it is worth: {bid} for a mid of {mid}"
                );
            }
            other => panic!("the far wing must still be quoted, got {other:?}"),
        }

        // Every wing still worth a tick IS bid a tick: only sub-tick contracts
        // quote a zero bid, and they quote it rather than disappearing.
        for contract in chain.iter() {
            let (Some(mid), Some(bid)) = (contract.call_middle, contract.call_bid) else {
                continue;
            };
            if mid.to_dec() >= dec!(0.01) {
                assert!(
                    bid >= pos_or_panic!(0.01),
                    "a contract worth {mid} must be bid at least a tick, got {bid}"
                );
            }
        }
    }

    /// A cheap contract's spread is a LARGER fraction of its price than a dear
    /// contract's, once the proportional term is on.
    #[test]
    fn test_a_cheap_contract_pays_more_in_relative_terms() {
        let mut request = request(4, brownian(0.2), 0.2);
        request.chain_size = Some(20);
        request.spread = Some(0.02);
        request.spread_proportional = Some(0.05);
        let chain = built_chain(&parameters(request));

        let mut quotes: Vec<(Decimal, Decimal)> = chain
            .iter()
            .filter_map(|contract| {
                match (contract.call_bid, contract.call_ask, contract.call_middle) {
                    (Some(bid), Some(ask), Some(mid)) if mid.to_dec() > Decimal::ZERO => {
                        Some((mid.to_dec(), ask.to_dec() - bid.to_dec()))
                    }
                    _ => None,
                }
            })
            .collect();
        quotes.sort_by_key(|quote| quote.0);
        assert!(
            quotes.len() > 1,
            "the chain must quote at least two contracts"
        );

        let (cheap_mid, cheap_spread) = quotes[0];
        let (dear_mid, dear_spread) = quotes[quotes.len() - 1];

        assert!(
            dear_spread > cheap_spread,
            "the dear contract costs more to cross in absolute terms"
        );
        assert!(
            cheap_spread / cheap_mid > dear_spread / dear_mid,
            "and the cheap one more in relative terms: {cheap_spread}/{cheap_mid} vs {dear_spread}/{dear_mid}"
        );
    }

    /// A request carrying only the legacy scalar quotes exactly as it did.
    ///
    /// "As it did" means, for every contract upstream would have kept, the
    /// arithmetic upstream applied: half the spread each way around the mid,
    /// rounded to two places. The contracts upstream would have ERASED are the
    /// documented divergence and are asserted separately.
    #[test]
    fn test_the_legacy_scalar_reproduces_the_old_quotes() {
        let mut request = request(4, brownian(0.2), 0.2);
        request.chain_size = Some(20);
        request.spread = Some(0.02);
        let chain = built_chain(&parameters(request));

        let spread = dec!(0.02);
        let half = spread / Decimal::TWO;
        let mut compared = 0;

        for contract in chain.iter() {
            let (Some(mid), Some(bid), Some(ask)) =
                (contract.call_middle, contract.call_bid, contract.call_ask)
            else {
                continue;
            };
            // Upstream keeps a quote only above the full spread; below it the
            // service now floors the bid instead of withdrawing.
            if mid.to_dec() <= spread {
                continue;
            }

            assert_eq!(
                ask.to_dec(),
                (mid.to_dec() + half).round_dp(2),
                "the ask must be upstream's, at strike {}",
                contract.strike_price
            );
            assert_eq!(
                bid.to_dec(),
                (mid.to_dec() - half).round_dp(2),
                "the bid must be upstream's, at strike {}",
                contract.strike_price
            );
            compared += 1;
        }

        assert!(compared > 0, "the chain must contain a comparable quote");
    }

    /// The same parameters and seed still produce the same quotes.
    ///
    /// The model is a pure function of the contract, so it cannot move the
    /// tape; this pins that it does not move the CHAIN either.
    #[test]
    fn test_the_model_is_deterministic() {
        let mut request = request(4, brownian(0.2), 0.2);
        request.chain_size = Some(10);
        request.spread_proportional = Some(0.03);
        request.spread_moneyness_widening = Some(0.4);
        request.spread_tenor_widening = Some(0.1);
        let parameters = parameters(request);

        let first = built_chain(&parameters);
        let second = built_chain(&parameters);

        assert_eq!(
            first.options, second.options,
            "two builds of one configuration must quote identically"
        );
    }

    /// Builds a chain at the reference spot, volatility and tenor.
    fn built_chain(parameters: &SimulationParametersV2) -> OptionChain {
        match build_chain(
            parameters,
            pos_or_panic!(5000.0),
            pos_or_panic!(0.2),
            ExpirationDate::Days(pos_or_panic!(30.0)),
            false,
        ) {
            Ok(chain) => chain,
            Err(error) => panic!("the chain must build: {error}"),
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

    /// A horizon of fewer than three steps has one return at most, and one
    /// return has no dispersion. v1 would price it from the whole embedded
    /// series — data past the horizon — so v2 refuses instead, which issue #63
    /// lists as an accepted answer.
    #[test]
    fn test_a_horizon_too_short_to_estimate_is_refused() {
        let mut historical = request(2, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices: volatile_prices(60),
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        match FactorTape::build(&parameters, &parameters.method) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "method.prices");
                assert!(reason.contains("zero"), "{reason}");
                assert!(reason.contains("prices nothing"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A series that never moves has no volatility, and upstream refuses to
    /// price a chain at zero. It becomes one 400 naming the series rather than
    /// a 500 from inside the chain builder.
    #[test]
    fn test_a_series_without_dispersion_is_refused() {
        let mut historical = request(4, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices: vec![5000.0; 8],
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        match FactorTape::build(&parameters, &parameters.method) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "method.prices");
                assert!(reason.contains("zero-value options"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A series too turbulent to price is refused with advice that applies: the
    /// volatility comes from the series, so lowering the request's does nothing.
    #[test]
    fn test_a_series_above_the_priceable_volatility_is_refused() {
        let prices: Vec<f64> = (0..20)
            .map(|index| if index % 2 == 0 { 5000.0 } else { 5500.0 })
            .collect();
        let mut historical = request(10, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices,
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        match FactorTape::build(&parameters, &parameters.method) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "method.prices");
                assert!(reason.contains("above the 1.0 maximum"), "{reason}");
                assert!(reason.contains("the series that has to change"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// The resolved method is separately supplied and passes no boundary
    /// validation, so a zero close reaching it from the database has to be
    /// caught here — a log return would divide by it, and that division panics.
    #[test]
    fn test_a_zero_price_in_the_resolved_series_is_refused() {
        let mut historical = request(4, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices: volatile_prices(20),
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        let mut resolved = volatile_series(20);
        match resolved.get_mut(3) {
            Some(price) => *price = Positive::ZERO,
            None => panic!("the fixture must have a fourth price"),
        }
        let resolved = SimulationMethod::Historical {
            timeframe: TimeFrame::Day,
            prices: resolved,
            symbol: Some("SPX".to_string()),
        };

        match FactorTape::build(&parameters, &resolved) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "method.prices");
                assert!(reason.contains("index 3 is zero"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A historical walk prices itself: the volatility comes from the series,
    /// never from the `volatility` the request happened to carry.
    #[test]
    fn test_historical_ignores_the_requested_volatility() {
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
        // A near-linear ramp has almost no dispersion, so every estimate sits
        // far below the 0.18 the request asked for. The point is not the
        // number: it is that the request's value prices nothing.
        for row in tape.rows() {
            assert_ne!(row.base_volatility, parameters.volatility);
            assert!(
                row.base_volatility < parameters.volatility,
                "a flat series cannot be as volatile as {}, got {} at step {}",
                parameters.volatility,
                row.base_volatility,
                row.step
            );
        }
    }

    /// A series with a turbulent tail must not price its calm opening: the
    /// estimate at each step is the one the prefix alone produces.
    #[test]
    fn test_historical_volatility_has_no_look_ahead() {
        let series = volatile_series(40);
        let full = match expanding_window_volatilities(&series, TimeFrame::Day) {
            Ok(Some(volatilities)) => volatilities,
            other => panic!("the full series must yield estimates, got {other:?}"),
        };

        for cut in 3..=series.len() {
            let prefix = match series.get(..cut) {
                Some(prefix) => prefix,
                None => panic!("the cut must be within the series"),
            };
            let partial = match expanding_window_volatilities(prefix, TimeFrame::Day) {
                Ok(Some(volatilities)) => volatilities,
                other => panic!("the prefix of {cut} must yield estimates, got {other:?}"),
            };

            assert_eq!(partial.len(), cut);
            for (point, volatility) in partial.iter().enumerate() {
                assert_eq!(
                    Some(volatility),
                    full.get(point),
                    "point {point} moved when the series grew to {cut} observations"
                );
            }
        }
    }

    /// Fewer than three prices leave nothing to expand over, so the caller is
    /// told to fall back rather than handed a fabricated estimate.
    #[test]
    fn test_expanding_window_needs_three_prices() {
        let series = volatile_series(4);

        for length in 0..3 {
            let prefix = match series.get(..length) {
                Some(prefix) => prefix,
                None => panic!("the prefix must be within the series"),
            };
            assert!(
                matches!(
                    expanding_window_volatilities(prefix, TimeFrame::Day),
                    Ok(None)
                ),
                "{length} prices cannot yield an expanding window"
            );
        }

        assert!(matches!(
            expanding_window_volatilities(&series, TimeFrame::Day),
            Ok(Some(_))
        ));
    }

    /// The first two points have no window of their own, so they carry the
    /// first computable estimate — the vector stays aligned with the prices and
    /// has no holes.
    #[test]
    fn test_expanding_window_backfills_the_first_two_points() {
        let series = volatile_series(12);

        let volatilities = match expanding_window_volatilities(&series, TimeFrame::Day) {
            Ok(Some(volatilities)) => volatilities,
            other => panic!("the series must yield estimates, got {other:?}"),
        };

        assert_eq!(volatilities.len(), series.len());
        assert_eq!(volatilities.first(), volatilities.get(2));
        assert_eq!(volatilities.get(1), volatilities.get(2));
    }

    /// A constant log return has no dispersion. Upstream reports zero rather
    /// than refusing, and so does this: an answer, not an error or a panic.
    #[test]
    fn test_expanding_window_of_a_constant_return_is_zero() {
        let mut price = Positive::new(5000.0).unwrap_or(Positive::ONE);
        let mut series = Vec::with_capacity(10);
        for _ in 0..10 {
            series.push(price);
            price = price * Positive::new(1.01).unwrap_or(Positive::ONE);
        }

        let volatilities = match expanding_window_volatilities(&series, TimeFrame::Day) {
            Ok(Some(volatilities)) => volatilities,
            other => panic!("a constant-return series must still yield estimates, got {other:?}"),
        };

        assert_eq!(volatilities.len(), series.len());
        for (point, volatility) in volatilities.iter().enumerate() {
            assert!(
                *volatility < Positive::new(1e-12).unwrap_or(Positive::ONE),
                "point {point} of a constant-return series should be flat, got {volatility}"
            );
        }
    }

    /// The estimate moves with the series, which is the whole point: a tape
    /// that reported one number would be the constant this issue removed.
    #[test]
    fn test_historical_tape_volatility_varies_across_steps() {
        let parameters = parameters(volatile_historical_request(30));

        let tape = tape(&parameters);

        let first = match tape.row(0) {
            Some(row) => row.base_volatility,
            None => panic!("the tape must have a first row"),
        };
        assert!(
            tape.rows().iter().any(|row| row.base_volatility != first),
            "every step reported the same volatility, so nothing is being estimated"
        );
    }

    /// Rebuilding after eviction is the same call, so the estimates come back
    /// identical — the tape stays a pure function of the parameters.
    #[test]
    fn test_historical_tape_volatility_is_reproducible() {
        let parameters = parameters(volatile_historical_request(30));

        let first = tape(&parameters);
        let second = tape(&parameters);

        assert_eq!(first.rows(), second.rows());
    }

    /// The acceptance criterion of issue #63: v1 and v2 price a historical step
    /// at the same volatility.
    ///
    /// v1's per-step value is the one its driver hands the chain builder, so
    /// the comparison calls that driver — `walk_steps_par`, the exact function
    /// `generator_optionchain` uses — and records the volatility it passes.
    /// Reading it there rather than off a built chain keeps skew, smile and
    /// quote rounding out of the comparison and leaves the two estimates facing
    /// each other.
    ///
    /// Agreement is numeric, not bit-exact: upstream accumulates the window
    /// with prefix sums and `constant_volatility` centres in two passes. They
    /// are algebraically the same expression, so what is left is `Decimal`
    /// rounding. The largest deviation this series produces is 3.4e-23 on an
    /// annualised volatility; the tolerance below sits five orders above that
    /// and fifteen below anything an option price could notice.
    ///
    /// Step 0 is excluded, and that exclusion is the one real difference: v1
    /// never asks its estimator about step 0 (its driver starts at index 1 and
    /// its first chain is the seeding chain, priced at the request's constant),
    /// while v2 serves step 0 as a snapshot and prices it with the backfilled
    /// estimate. Pricing a served step at a volatility the series contradicts
    /// would be the worse answer.
    #[test]
    fn test_v1_and_v2_agree_on_historical_volatility() {
        /// Absolute, on an annualised volatility.
        const TOLERANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 18);

        let parameters = parameters(volatile_historical_request(30));
        let tape = tape(&parameters);

        let base_volatility = match resolve_base_volatility(&parameters, &parameters.method) {
            Ok(volatility) => volatility,
            Err(error) => panic!("the historical series must yield a volatility: {error}"),
        };
        let initial_chain = match build_initial_chain(&parameters, base_volatility) {
            Ok(chain) => chain,
            Err(error) => panic!("the seeding chain must build: {error}"),
        };
        let walk_params = WalkParams {
            size: parameters.steps,
            init_step: Step {
                x: Xstep::new(
                    Positive::ONE,
                    parameters.time_frame,
                    // Far enough out that the driver's per-step expiry decay
                    // cannot truncate the walk before the horizon ends; a
                    // historical path ignores it either way.
                    ExpirationDate::Days(Positive::new(3650.0).unwrap_or(Positive::ONE)),
                ),
                y: Ystep::new(0, initial_chain.clone()),
            },
            walk_type: parameters.method.clone(),
            walker: Box::new(Walker::new_with_seed(parameters.seed)),
        };

        // The driver builds its steps in parallel, so the volatilities are
        // recorded with the step index the driver itself assigns.
        let observed: Mutex<Vec<(i32, Option<Positive>)>> = Mutex::new(Vec::new());
        let walked = walk_steps_par::<OptionChain, SimulationError, _>(
            &walk_params,
            |_price, volatility, x_step| match observed.lock() {
                Ok(mut guard) => {
                    guard.push((*x_step.index(), volatility));
                    Ok(Some(initial_chain.clone()))
                }
                Err(_) => Err(SimulationError::walk_error("the recorder lock is poisoned")),
            },
        );
        if let Err(error) = walked {
            panic!("the v1 driver must walk the series: {error}");
        }

        let mut v1_volatilities = match observed.into_inner() {
            Ok(volatilities) => volatilities,
            Err(_) => panic!("the recorder lock must not be poisoned"),
        };
        v1_volatilities.sort_by_key(|(index, _)| *index);

        assert_eq!(
            v1_volatilities.len(),
            tape.len() - 1,
            "v1 prices every step but the seeding one"
        );

        for (index, volatility) in &v1_volatilities {
            let step = match usize::try_from(*index) {
                Ok(step) => step,
                Err(error) => panic!("the driver's step index must be non-negative: {error}"),
            };
            let v2_row = match tape.row(step) {
                Some(row) => row,
                None => panic!("the tape must have a row for step {step}"),
            };
            let v1_volatility = match volatility {
                Some(volatility) => *volatility,
                None => panic!("v1 must price historical step {step} with an estimate"),
            };

            let deviation = (v1_volatility.to_dec() - v2_row.base_volatility.to_dec()).abs();
            assert!(
                deviation <= TOLERANCE,
                "step {step} disagrees by {deviation}: v1 {v1_volatility}, v2 {}",
                v2_row.base_volatility
            );
        }
    }

    /// A price series with a calm opening and a turbulent tail, so an expanding
    /// window has something to move over.
    fn volatile_series(length: usize) -> Vec<Positive> {
        volatile_series_from(&volatile_prices(length))
    }

    fn volatile_series_from(prices: &[f64]) -> Vec<Positive> {
        prices
            .iter()
            .map(|price| match Positive::new(*price) {
                Ok(price) => price,
                Err(error) => panic!("the test price must be positive: {error}"),
            })
            .collect()
    }

    fn volatile_prices(length: usize) -> Vec<f64> {
        let mut prices = Vec::with_capacity(length);
        let mut price = 5000.0_f64;
        for index in 0..length {
            prices.push(price);
            // Calm for the first half, then a widening zig-zag: the expanding
            // window has to keep moving instead of settling.
            let shock = if index < length / 2 {
                1.0
            } else {
                20.0 + f64::from(u32::try_from(index).unwrap_or(0))
            };
            price += if index % 2 == 0 { shock } else { -shock };
        }
        prices
    }

    fn volatile_historical_request(steps: usize) -> CreateSimulationRequest {
        let mut historical = request(steps, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices: volatile_prices(steps * 2),
            symbol: Some("SPX".to_string()),
        };
        historical
    }

    /// A ratio too large to represent is a rejection, not a panic.
    ///
    /// Both prices are strictly positive and perfectly legal on their own; it
    /// is the jump between them that no `Decimal` can hold. Upstream's
    /// `calculate_log_returns` divides two `Positive`s, which aborts the
    /// process on exactly this, so the estimator computes the ratio itself.
    #[test]
    fn test_an_unrepresentable_price_jump_is_rejected() {
        let mut prices = vec![1e-28_f64; 4];
        prices.push(7e28);
        prices.extend(std::iter::repeat_n(7e28, 8));

        let mut historical = request(4, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices,
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        match FactorTape::build(&parameters, &parameters.method) {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "method.prices"),
            other => panic!("an unrepresentable ratio must be a 400, got {other:?}"),
        }
    }

    /// And the same in the other direction, where the ratio underflows to
    /// something whose log is not representable.
    #[test]
    fn test_an_unrepresentable_price_collapse_is_rejected() {
        let mut prices = vec![7e28_f64; 4];
        prices.push(1e-28);
        prices.extend(std::iter::repeat_n(1e-28, 8));

        let mut historical = request(4, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices,
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        match FactorTape::build(&parameters, &parameters.method) {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "method.prices"),
            other => panic!("an unrepresentable ratio must be a 400, got {other:?}"),
        }
    }

    /// A series a client could plausibly send still estimates, so the guard
    /// above rejects the unrepresentable rather than the merely volatile.
    ///
    /// The ceiling on a priceable volatility is a separate check with its own
    /// tests; this one only has to stay under it.
    #[test]
    fn test_a_violently_volatile_series_still_estimates() {
        // Two percent daily moves, which annualise to about a third — well
        // inside what a chain can be priced at, and far outside anything the
        // ratio guard should touch.
        let prices: Vec<f64> = (0..40)
            .map(|i| 5000.0 * (1.0 + 0.02 * f64::from(i).sin()))
            .collect();

        let mut historical = request(20, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices,
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        match FactorTape::build(&parameters, &parameters.method) {
            Ok(tape) => assert_eq!(tape.len(), 20),
            Err(error) => panic!("a two percent daily move is legal, got {error}"),
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

    /// Later observations cannot change an earlier step's base volatility.
    ///
    /// The property the estimator turns on (#63): whatever prices a historical
    /// step must be a function of that step and what came before it. It held
    /// when the answer was a constant and it holds now that it is an estimate,
    /// which is the point — it is the invariant, not the implementation, that
    /// this pins. The narrower claim, that no reduction reads past the horizon,
    /// is the next test's.
    #[test]
    fn test_a_longer_series_leaves_earlier_steps_untouched() {
        // The observation count is a `u32` so the index converts to `f64`
        // losslessly and infallibly — no conversion to swallow, which is what a
        // fallback of zero would have done, quietly flattening the series.
        let build = |steps: usize, observations: u32| {
            let prices: Vec<f64> = (0..observations)
                .map(|i| 5000.0 + (f64::from(i) * 7.0).sin() * 250.0)
                .collect();
            let mut historical = request(steps, brownian(0.18), 0.18);
            historical.method = ApiWalkType::Historical {
                timeframe: ApiTimeFrame::Day,
                prices,
                symbol: Some("SPX".to_string()),
            };
            tape(&parameters(historical))
        };

        let short = build(15, 20);
        let long = build(15, 400);

        for (early, later) in short.rows().iter().zip(long.rows()) {
            assert_eq!(
                early.base_volatility, later.base_volatility,
                "step {} must not depend on observations after it",
                early.step
            );
            assert_eq!(
                early.spot, later.spot,
                "step {} replayed differently",
                early.step
            );
        }
    }

    /// Turbulence past the horizon cannot refuse a request whose own horizon is
    /// calm.
    ///
    /// This is the case the reduction over the walked prefix exists for. The
    /// tail here annualises far above the 1.0 a chain can be priced at, so
    /// reducing the whole embedded series — which is what v1 does for its own
    /// fallback — would 400 the simulation over observations it never reaches.
    /// The calm prefix is priceable, so it builds.
    #[test]
    fn test_turbulence_past_the_horizon_cannot_refuse_a_calm_one() {
        let mut prices: Vec<f64> = (0..10).map(|index| 5000.0 + f64::from(index)).collect();
        prices.extend((0..10).map(|index| if index % 2 == 0 { 5000.0 } else { 5500.0 }));

        let mut historical = request(10, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices: prices.clone(),
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        let tape = tape(&parameters);
        assert_eq!(tape.len(), 10);

        // The guard is only meaningful if the tail really is unpriceable: a
        // whole-series reduction has to be the thing that would have failed.
        let whole_series = volatile_series_from(&prices);
        match historical_constant_volatility(&whole_series, TimeFrame::Day) {
            Ok(volatility) => assert!(
                volatility > Positive::ONE,
                "the fixture's tail must be unpriceable for this test to mean anything, got \
                 {volatility}"
            ),
            Err(error) => panic!("the fixture must reduce: {error}"),
        }
    }

    /// A flat opening is enough to refuse the simulation, because points 0 and 1
    /// carry the first computable estimate and three equal prices make that
    /// estimate zero. The rest of the horizon never gets a say.
    #[test]
    fn test_a_flat_opening_is_refused_even_when_the_rest_moves() {
        let mut prices = vec![5000.0, 5000.0, 5000.0];
        prices.extend((0..10).map(|index| if index % 2 == 0 { 5050.0 } else { 4980.0 }));

        let mut historical = request(8, brownian(0.18), 0.18);
        historical.method = ApiWalkType::Historical {
            timeframe: ApiTimeFrame::Day,
            prices,
            symbol: Some("SPX".to_string()),
        };
        let parameters = parameters(historical);

        match FactorTape::build(&parameters, &parameters.method) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "method.prices");
                assert!(reason.contains("zero at step 0"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
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

    /// A volatility above what a chain can be priced at is a 400 at tape build,
    /// not a 500 halfway through the run.
    ///
    /// Upstream refuses to build a chain above 1.0 annualised, so without this
    /// the first snapshot at the offending step would fail with an internal
    /// error, and every later one that also crosses.
    #[test]
    fn test_a_volatility_above_one_is_rejected_when_the_tape_is_built() {
        let mut request = request(30, brownian(1.5), 1.5);
        request.volatility = 1.5;

        let parameters = match SimulationParametersV2::try_from(request) {
            Ok(parameters) => parameters,
            Err(error) => panic!("the request must convert: {error}"),
        };

        match FactorTape::build(&parameters, &parameters.method) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "volatility");
                assert!(
                    reason.contains("1.0"),
                    "the reason must name the cap, got {reason}"
                );
            }
            other => panic!("a 1.5 volatility must be refused, got {other:?}"),
        }
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
