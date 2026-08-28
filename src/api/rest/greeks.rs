//! The `greeks` query parameter and the payload it selects.
//!
//! A chain response carries implied volatility, gamma and per-side delta by
//! default and nothing else. The full set is twelve values per option style,
//! and a chain can be 1 001 strikes wide across 512 expirations, so emitting it
//! unconditionally would multiply the payload a play-loop client pulls every
//! few hundred milliseconds. It is therefore opt-in, and the default is exactly
//! what clients get today.
//!
//! # Levels
//!
//! | `greeks` | What the quote carries |
//! |---|---|
//! | absent or `none` | Today's fields only. No `greeks` key at all |
//! | `first` | `theta`, `vega`, `rho`, `rho_d` |
//! | `all` | The full twelve-value [`GreeksSnapshot`] |
//!
//! # Sign and size
//!
//! Every emitted greek is **per one long contract**, with one exception. For
//! the eleven that scale, upstream builds the snapshot through
//! `get_option(Side::Long, style)`, and since optionstratlib 0.20 the `Greeks`
//! trait applies the `Side` sign in *every* greek rather than only in `delta` —
//! so a consumer that applies the position sign again would double-count it.
//! The client applies position sign and size exactly once, to those values.
//!
//! **`alpha` is not one of them.** It is the ratio `gamma / theta`, so a short
//! position negates both and the ratio is unchanged; upstream says so in as many
//! words. Scaling it by quantity, or flipping it with the position sign, gives a
//! client a number that means nothing. Carry it through as it arrives.
//!
//! An absent `rho`, `rho_d` or `alpha` means **not meaningful for these
//! inputs**, never zero; upstream normalises `alpha` to `None` where it would
//! otherwise be `Decimal::MAX`.

use crate::utils::ChainError;
use optionstratlib::chains::OptionData;
use optionstratlib::greeks::GreeksSnapshot;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use tokio::sync::Semaphore;
use utoipa::ToSchema;

/// How much of the greek set a chain response should carry.
///
/// Parsed from the `greeks` query parameter by [`GreekLevel::parse`]; the
/// default is [`GreekLevel::None`], which is the pre-existing response.
/// Serialised and schema-bearing so the OpenAPI document can publish the
/// parameter as a closed enum rather than an unconstrained string: a generated
/// client then cannot send `?greeks=second` at all, and a reader of the document
/// sees the three values without reading prose. The wire form is still parsed by
/// [`GreekLevel::parse`] from a raw string, so an unknown value stays a typed
/// `400` naming the field rather than actix's untyped rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GreekLevel {
    /// Implied volatility, gamma and per-side delta only: the default, and
    /// byte-identical to the response before the parameter existed.
    None,
    /// Adds the remaining first-order greeks: `theta`, `vega`, `rho`, `rho_d`.
    First,
    /// The full twelve-value snapshot per option style.
    All,
}

impl GreekLevel {
    /// Parses the raw `greeks` query value.
    ///
    /// An absent parameter is [`GreekLevel::None`], so a client that has never
    /// heard of it keeps its current payload.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming `greeks` for any other value.
    /// An unrecognised level is rejected rather than silently downgraded: a
    /// client that asked for `all` and quietly received the default would
    /// price a position against greeks it never got.
    pub(crate) fn parse(raw: Option<&str>) -> Result<Self, ChainError> {
        match raw {
            None => Ok(Self::None),
            Some(value) => match value.trim() {
                "none" => Ok(Self::None),
                "first" => Ok(Self::First),
                "all" => Ok(Self::All),
                other => Err(ChainError::Validation {
                    field: "greeks".to_string(),
                    reason: format!("unknown greek level '{other}'; expected none, first or all"),
                }),
            },
        }
    }

    /// Whether this level needs the option's greek snapshots at all.
    #[must_use]
    #[inline]
    pub(crate) fn wants_greeks(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// The first-order greeks the default response does not already carry.
///
/// `delta` is deliberately absent: it is already on the quote, and repeating it
/// under a second key would let the two drift. `gamma` is likewise already on
/// the contract.
///
/// Closed with `deny_unknown_fields`, which does two jobs at once: it makes
/// serde's untagged resolution independent of variant order, and utoipa derives
/// `additionalProperties: false` from it. That is what keeps the published
/// `oneOf` satisfiable — without it a full twelve-value payload would satisfy
/// this four-field shape as well, and "exactly one branch" would fail for
/// every `greeks=all` response.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FirstOrderGreeks {
    /// Sensitivity to the passage of time, per one long contract.
    pub theta: Option<f64>,
    /// Sensitivity to implied volatility, per one long contract.
    pub vega: Option<f64>,
    /// Sensitivity to the risk-free rate. `null` means not meaningful for
    /// these inputs, never zero.
    pub rho: Option<f64>,
    /// Sensitivity to the dividend yield. `null` means not meaningful for
    /// these inputs, never zero.
    pub rho_d: Option<f64>,
}

impl From<&GreeksSnapshot> for FirstOrderGreeks {
    fn from(snapshot: &GreeksSnapshot) -> Self {
        Self {
            theta: to_f64(snapshot.theta),
            vega: to_f64(snapshot.vega),
            rho: snapshot.rho.and_then(to_f64),
            rho_d: snapshot.rho_d.and_then(to_f64),
        }
    }
}

/// The full twelve-value greek set of one option style.
///
/// A LOCAL type, not upstream's `GreeksSnapshot`. `CLAUDE.md` is binding that
/// the REST DTOs speak `f64` with the conversion happening exactly once at the
/// boundary; carrying the upstream struct would have made its `Decimal`
/// serialisation — JSON strings, and whatever field set the next release
/// carries — part of this service's public contract by accident rather than by
/// decision.
///
/// The cost is a twelve-field mirror that has to track upstream. That cost is
/// paid deliberately, and [`FullGreeks::from`] destructures the snapshot so a
/// thirteenth greek is a compile error here rather than a field the API
/// silently stops carrying.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FullGreeks {
    /// Sensitivity to the underlying price, per one long contract.
    pub delta: Option<f64>,
    /// Rate of change of delta, per one long contract.
    pub gamma: Option<f64>,
    /// Sensitivity to the passage of time, per one long contract.
    pub theta: Option<f64>,
    /// Sensitivity to implied volatility, per one long contract.
    pub vega: Option<f64>,
    /// Sensitivity to the risk-free rate. `null` means not meaningful.
    pub rho: Option<f64>,
    /// Sensitivity to the dividend yield. `null` means not meaningful.
    pub rho_d: Option<f64>,
    /// The ratio `gamma / theta`. **Does not scale with position sign or
    /// size** — a short position negates both terms and leaves the ratio
    /// unchanged. `null` means not meaningful. See the module docs.
    pub alpha: Option<f64>,
    /// Rate of change of delta with volatility, per one long contract.
    pub vanna: Option<f64>,
    /// Rate of change of vega with volatility, per one long contract.
    pub vomma: Option<f64>,
    /// Rate of change of vega with time, per one long contract.
    pub veta: Option<f64>,
    /// Rate of change of delta with time, per one long contract.
    pub charm: Option<f64>,
    /// Rate of change of gamma with time, per one long contract.
    pub color: Option<f64>,
}

impl From<&GreeksSnapshot> for FullGreeks {
    fn from(snapshot: &GreeksSnapshot) -> Self {
        // Destructured, not field-accessed: a thirteenth upstream greek is then
        // a COMPILE ERROR rather than a value this DTO silently stops carrying.
        // Same discipline `ApiWalkType` uses for a new `WalkType` variant.
        let GreeksSnapshot {
            delta,
            gamma,
            theta,
            vega,
            rho,
            rho_d,
            alpha,
            vanna,
            vomma,
            veta,
            charm,
            color,
        } = snapshot;
        Self {
            delta: to_f64(*delta),
            gamma: to_f64(*gamma),
            theta: to_f64(*theta),
            vega: to_f64(*vega),
            rho: rho.and_then(to_f64),
            rho_d: rho_d.and_then(to_f64),
            alpha: alpha.and_then(to_f64),
            vanna: to_f64(*vanna),
            vomma: to_f64(*vomma),
            veta: to_f64(*veta),
            charm: to_f64(*charm),
            color: to_f64(*color),
        }
    }
}

/// The one `Decimal` to `f64` conversion on this boundary.
///
/// `None` only if a value were outside `f64`'s range, which no `Decimal` is —
/// its maximum is about `7.9e28`. Written as an `Option` rather than an
/// `unwrap` because `rules/global_rules.md` allows neither on a request path.
#[must_use]
#[inline]
fn to_f64(value: Decimal) -> Option<f64> {
    use rust_decimal::prelude::ToPrimitive;
    value.to_f64()
}

/// The greek payload one quoted side carries, shaped by the requested level.
///
/// Serialised untagged, so the `greeks` key is a plain object at both levels
/// and a client parses it by the fields it finds rather than by a discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum GreeksResponse {
    /// `greeks=all`: the full twelve-value set.
    ///
    /// Listed first so deserialisation prefers it — an untagged enum takes the
    /// first variant that matches, and the four-field variant would otherwise
    /// swallow a complete payload by ignoring the rest.
    Full(FullGreeks),
    /// `greeks=first`: the remaining first-order greeks.
    FirstOrder(FirstOrderGreeks),
}

impl GreeksResponse {
    /// Shapes an upstream snapshot to the requested level.
    ///
    /// Returns `None` at [`GreekLevel::None`], which is what keeps the default
    /// response free of the key entirely.
    #[must_use]
    fn from_snapshot(snapshot: &GreeksSnapshot, level: GreekLevel) -> Option<Self> {
        match level {
            GreekLevel::None => None,
            GreekLevel::First => Some(Self::FirstOrder(FirstOrderGreeks::from(snapshot))),
            GreekLevel::All => Some(Self::Full(FullGreeks::from(snapshot))),
        }
    }
}

/// Default number of greek-pricing jobs allowed to run at once.
///
/// Small on purpose. One job is up to `OCS_MAX_SNAPSHOT_CONTRACTS` contracts of
/// pricing plus the serialisation of the result, which is seconds of CPU at the
/// cap; letting every concurrent request start one turns a handful of peeks
/// into a machine with no cores left for anything else, including the requests
/// that never asked for greeks.
pub(crate) const DEFAULT_MAX_CONCURRENT_GREEK_RENDERS: usize = 4;

/// How many greek-pricing jobs may run at once
/// (`OCS_MAX_CONCURRENT_GREEK_RENDERS`).
///
/// The admission bound the whole service shares — v1 and v2, peek and step —
/// because they contend for the same cores. Requests above the bound WAIT on
/// the semaphore rather than being rejected, and they wait in async code, so a
/// client that disconnects while queued drops its future and never occupies a
/// thread at all. That is the part a bare `spawn_blocking` cannot do: its task
/// is not cancellable once started, so without admission a burst of peeks
/// commits the machine to every job in it.
static GREEK_RENDER_PERMITS: LazyLock<Semaphore> = LazyLock::new(|| {
    Semaphore::new(crate::api::rest::limits::parse_limit(
        std::env::var("OCS_MAX_CONCURRENT_GREEK_RENDERS").ok(),
        DEFAULT_MAX_CONCURRENT_GREEK_RENDERS,
    ))
});

/// Runs a rendering job off the runtime and under admission when the level
/// makes it expensive.
///
/// At [`GreekLevel::None`] there is nothing to price, so the work stays inline
/// and takes no permit — the default request must not queue behind a burst of
/// greek requests.
///
/// The job is the WHOLE unit of work, pricing and serialisation together.
/// Serialising afterwards on the worker would put a large document back on the
/// thread the job was moved off, which at `greeks=all` on a capped snapshot is
/// a stall on its own.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] if the blocking task panics or is dropped,
/// and whatever the job itself returns.
pub(crate) async fn admit_blocking<T, F>(level: GreekLevel, job: F) -> Result<T, ChainError>
where
    F: FnOnce() -> Result<T, ChainError> + Send + 'static,
    T: Send + 'static,
{
    if !level.wants_greeks() {
        return job();
    }

    let permit = GREEK_RENDER_PERMITS.acquire().await.map_err(|error| {
        ChainError::Internal(format!("the greek admission gate is closed: {error}"))
    })?;

    let outcome = tokio::task::spawn_blocking(job)
        .await
        .map_err(|error| ChainError::Internal(format!("greek rendering failed: {error}")))?;

    drop(permit);
    outcome
}

/// Renders one response body under [`admit_blocking`].
///
/// # Errors
///
/// As [`admit_blocking`], plus [`ChainError::Internal`] when the response
/// cannot be serialised.
pub(crate) async fn render_body<T, F>(level: GreekLevel, render: F) -> Result<Vec<u8>, ChainError>
where
    F: FnOnce() -> T + Send + 'static,
    T: serde::Serialize + Send + 'static,
{
    admit_blocking(level, move || serialize_body(&render())).await
}

/// Serialises a response body, reporting a failure as an internal error rather
/// than panicking on a request path.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] when the value cannot be serialised.
pub(crate) fn serialize_body<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ChainError> {
    serde_json::to_vec(value)
        .map_err(|error| ChainError::Internal(format!("failed to encode the response: {error}")))
}

/// The greek payloads for both sides of one strike, at the requested level.
///
/// Either branch can be the normal one, depending on the deployment, and the
/// difference is issue #74's:
///
/// * **v2 with a warehouse registered** builds its chains with the snapshots
///   already on, because a filed step has to carry what a replayed one does.
///   This function then just *reads* them, and costs nothing.
/// * **v2 without a warehouse, and v1 always**, build without them, so this
///   function computes them through upstream's own `calculate_greeks` — and
///   only for the requests that ask.
///
/// No greek mathematics lives in this crate either way.
///
/// # An absent payload is not a zero
///
/// Upstream returns no snapshot for a strike whose option cannot be built, and
/// only logs it at `debug`. Such a strike arrives with **no `greeks` key at
/// all**, which on the wire is indistinguishable from the default level. A
/// client that asked for a level and found the key missing on some strikes is
/// looking at degenerate strikes, not at a downgraded response — the existing
/// `implied_volatility`, `gamma` and `delta` mirrors are still there, because
/// they are defined where the full set is not.
///
/// # Cost
///
/// Nothing, when the chain already carries the snapshots. Roughly 40 µs per
/// contract in a release build when it does not. `first` and `all` cost the
/// same either way: upstream builds the snapshot whole and the level only
/// decides what is written out.
///
/// Computing here is measurably more expensive per contract than asking the
/// builder for it — `calculate_greeks` recomputes the delta and gamma this
/// response then reads from the mirrors instead, where a build with snapshots
/// on costs about 1.54x a plain one. Which is cheaper overall depends entirely
/// on how often the payload is actually read, which is why the decision sits
/// with the caller that knows: `SeriesBuilder::with_greek_snapshots`.
///
/// Callers must keep this off the async runtime. Both versions render above the
/// default level inside `spawn_blocking`, because
/// `DEFAULT_MAX_SNAPSHOT_CONTRACTS` is 200 000 and seconds of uninterrupted CPU
/// on a worker would stall every other request that worker holds.
///
/// # Why a clone
///
/// `calculate_greeks` takes `&mut self` and this function only has `&OptionData`
/// — the DTO layer has no business mutating the snapshot it was handed to
/// render. The clone is one option, not the chain, and is unmeasurable next to
/// the pricing itself.
#[must_use]
pub(crate) fn greeks_for(
    data: &OptionData,
    level: GreekLevel,
) -> (Option<GreeksResponse>, Option<GreeksResponse>) {
    if !level.wants_greeks() {
        return (None, None);
    }

    if data.greeks_call.is_some() || data.greeks_put.is_some() {
        return (
            data.greeks_call
                .as_ref()
                .and_then(|snapshot| GreeksResponse::from_snapshot(snapshot, level)),
            data.greeks_put
                .as_ref()
                .and_then(|snapshot| GreeksResponse::from_snapshot(snapshot, level)),
        );
    }

    let mut priced = data.clone();
    priced.calculate_greeks();
    (
        priced
            .greeks_call
            .as_ref()
            .and_then(|snapshot| GreeksResponse::from_snapshot(snapshot, level)),
        priced
            .greeks_put
            .as_ref()
            .and_then(|snapshot| GreeksResponse::from_snapshot(snapshot, level)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use optionstratlib::ExpirationDate;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::chains::{OptionChainBuildParams, utils::OptionDataPriceParams};
    use positive::{Positive, pos_or_panic};
    use rust_decimal_macros::dec;

    /// A one-strike chain, optionally built with the greek snapshots already
    /// populated. `with_greek_snapshots(true)` is the path no handler takes
    /// today, which is exactly why the branch that reads it needs a test.
    fn fixture_chain(prepopulated: bool) -> OptionChain {
        let price_params = OptionDataPriceParams::new(
            Some(Box::new(pos_or_panic!(100.0))),
            Some(ExpirationDate::Days(pos_or_panic!(30.0))),
            Some(dec!(0.04)),
            Some(pos_or_panic!(0.015)),
            Some("AAPL".to_string()),
        );
        let build_params = OptionChainBuildParams::new(
            "AAPL".to_string(),
            Some(Positive::ONE),
            1,
            Some(pos_or_panic!(5.0)),
            dec!(-0.2),
            dec!(0.5),
            pos_or_panic!(0.02),
            2,
            price_params,
            pos_or_panic!(0.2),
        )
        .with_greek_snapshots(prepopulated);

        match OptionChain::build_chain(&build_params) {
            Ok(chain) => chain,
            Err(error) => panic!("the fixture chain must build: {error}"),
        }
    }

    /// The first strike of a fixture chain.
    fn fixture_option(prepopulated: bool) -> optionstratlib::chains::OptionData {
        let chain = fixture_chain(prepopulated);
        match chain.iter().next() {
            Some(data) => data.clone(),
            None => panic!("the fixture chain must carry a strike"),
        }
    }

    /// The default level does no work at all: no clone, no pricing, no key.
    #[test]
    fn test_greeks_for_returns_nothing_at_the_default_level() {
        let data = fixture_option(false);

        assert_eq!(greeks_for(&data, GreekLevel::None), (None, None));
    }

    /// The path every request actually takes: the chain carries no snapshots,
    /// so this function prices them.
    #[test]
    fn test_greeks_for_prices_an_option_that_carries_no_snapshots() {
        let data = fixture_option(false);
        assert!(
            data.greeks_call.is_none() && data.greeks_put.is_none(),
            "the fixture must start without snapshots, or this tests nothing"
        );

        let (call, put) = greeks_for(&data, GreekLevel::All);

        assert!(matches!(call, Some(GreeksResponse::Full(_))));
        assert!(matches!(put, Some(GreeksResponse::Full(_))));
        // Pricing happens on a clone: the caller's option is untouched, which
        // is what lets a shared snapshot be rendered at different levels by
        // different clients.
        assert!(data.greeks_call.is_none() && data.greeks_put.is_none());
    }

    /// The other branch: a chain built WITH snapshots is read rather than
    /// repriced, and yields the same values.
    #[test]
    fn test_greeks_for_reads_snapshots_a_chain_already_carries() {
        let prepopulated = fixture_option(true);
        assert!(
            prepopulated.greeks_call.is_some(),
            "with_greek_snapshots must populate the call snapshot"
        );

        let (read_call, read_put) = greeks_for(&prepopulated, GreekLevel::All);
        let (priced_call, priced_put) = greeks_for(&fixture_option(false), GreekLevel::All);

        assert_eq!(read_call, priced_call, "reading must equal pricing");
        assert_eq!(read_put, priced_put, "reading must equal pricing");
    }

    /// The level selects the shape, not the computation: `first` projects the
    /// same snapshot the `all` payload carries in full.
    #[test]
    fn test_greeks_for_projects_the_first_order_subset() {
        let data = fixture_option(false);

        let (first, _) = greeks_for(&data, GreekLevel::First);
        let (all, _) = greeks_for(&data, GreekLevel::All);

        match (first, all) {
            (Some(GreeksResponse::FirstOrder(subset)), Some(GreeksResponse::Full(full))) => {
                assert_eq!(subset.theta, full.theta);
                assert_eq!(subset.vega, full.vega);
                assert_eq!(subset.rho, full.rho);
                assert_eq!(subset.rho_d, full.rho_d);
            }
            other => panic!("each level must yield its own variant, got {other:?}"),
        }
    }

    /// The admission bound is what the documentation says it is.
    ///
    /// The number itself is a judgement call, but `.env.example` and the crate
    /// docs quote it, so it may not drift from them silently. It also may not
    /// be zero, which `parse_limit` already enforces for the configured value:
    /// a bound of zero would deadlock every greek request.
    #[test]
    fn test_the_greek_admission_default_matches_the_documentation() {
        assert_eq!(DEFAULT_MAX_CONCURRENT_GREEK_RENDERS, 4);
    }

    /// The default level never queues.
    ///
    /// `admit_blocking` runs it inline and takes no permit, so a burst of
    /// `greeks=all` requests cannot make a plain peek wait behind them. Proven
    /// by exhausting the permits first: this call still completes.
    #[tokio::test]
    async fn test_the_default_level_takes_no_permit() {
        let held = match GREEK_RENDER_PERMITS
            .acquire_many(u32::try_from(DEFAULT_MAX_CONCURRENT_GREEK_RENDERS).unwrap_or(1))
            .await
        {
            Ok(permits) => permits,
            Err(error) => panic!("the semaphore must hand out its permits: {error}"),
        };

        let outcome = admit_blocking(GreekLevel::None, || Ok(7_usize)).await;

        drop(held);
        match outcome {
            Ok(value) => assert_eq!(value, 7),
            Err(error) => panic!("the default level must not wait for a permit: {error}"),
        }
    }

    #[test]
    fn test_parse_absent_parameter_is_none() {
        match GreekLevel::parse(None) {
            Ok(level) => assert_eq!(level, GreekLevel::None),
            Err(error) => panic!("an absent parameter must parse: {error}"),
        }
    }

    #[test]
    fn test_parse_accepts_the_three_documented_levels() {
        for (raw, expected) in [
            ("none", GreekLevel::None),
            ("first", GreekLevel::First),
            ("all", GreekLevel::All),
        ] {
            match GreekLevel::parse(Some(raw)) {
                Ok(level) => assert_eq!(level, expected, "for {raw}"),
                Err(error) => panic!("{raw} must parse: {error}"),
            }
        }
    }

    #[test]
    fn test_parse_trims_surrounding_whitespace() {
        match GreekLevel::parse(Some("  all  ")) {
            Ok(level) => assert_eq!(level, GreekLevel::All),
            Err(error) => panic!("a padded value must parse: {error}"),
        }
    }

    /// An unknown level is a rejection, not a silent downgrade: a client that
    /// asked for `all` and got the default would price against greeks it never
    /// received.
    #[test]
    fn test_parse_rejects_an_unknown_level_naming_the_field() {
        match GreekLevel::parse(Some("second")) {
            Ok(level) => panic!("an unknown level must be rejected, got {level:?}"),
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "greeks");
                assert!(
                    reason.contains("second"),
                    "the reason must quote the offending value, got {reason}"
                );
            }
            Err(other) => panic!("expected a validation failure, got {other:?}"),
        }
    }

    /// Case matters: the parameter is a fixed vocabulary, not a free-form
    /// string, and accepting `ALL` here would leave `All` and `all` to diverge
    /// the day the set grows.
    #[test]
    fn test_parse_rejects_a_differently_cased_level() {
        assert!(GreekLevel::parse(Some("ALL")).is_err());
        assert!(GreekLevel::parse(Some("First")).is_err());
    }

    #[test]
    fn test_wants_greeks_is_false_only_for_none() {
        assert!(!GreekLevel::None.wants_greeks());
        assert!(GreekLevel::First.wants_greeks());
        assert!(GreekLevel::All.wants_greeks());
    }
}
