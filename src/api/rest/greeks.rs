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
//! Every emitted greek is **per one long contract**. Upstream builds the
//! snapshot through `get_option(Side::Long, style)`, and since optionstratlib
//! 0.20 the `Greeks` trait applies the `Side` sign in *every* greek rather than
//! only in `delta` — so a consumer that applies the position sign again would
//! double-count it. The client applies position sign and size exactly once, to
//! these values.
//!
//! An absent `rho`, `rho_d` or `alpha` means **not meaningful for these
//! inputs**, never zero; upstream normalises `alpha` to `None` where it would
//! otherwise be `Decimal::MAX`.

use crate::utils::ChainError;
use optionstratlib::chains::OptionData;
use optionstratlib::greeks::GreeksSnapshot;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// How much of the greek set a chain response should carry.
///
/// Parsed from the `greeks` query parameter by [`GreekLevel::parse`]; the
/// default is [`GreekLevel::None`], which is the pre-existing response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GreekLevel {
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FirstOrderGreeks {
    /// Sensitivity to the passage of time, per one long contract.
    pub theta: Decimal,
    /// Sensitivity to implied volatility, per one long contract.
    pub vega: Decimal,
    /// Sensitivity to the risk-free rate. `null` means not meaningful for
    /// these inputs, never zero.
    pub rho: Option<Decimal>,
    /// Sensitivity to the dividend yield. `null` means not meaningful for
    /// these inputs, never zero.
    pub rho_d: Option<Decimal>,
}

impl From<&GreeksSnapshot> for FirstOrderGreeks {
    fn from(snapshot: &GreeksSnapshot) -> Self {
        Self {
            theta: snapshot.theta,
            vega: snapshot.vega,
            rho: snapshot.rho,
            rho_d: snapshot.rho_d,
        }
    }
}

/// The greek payload one quoted side carries, shaped by the requested level.
///
/// Serialised untagged, so the `greeks` key is a plain object at both levels
/// and a client parses it by the fields it finds rather than by a discriminant.
/// The full variant is the upstream [`GreeksSnapshot`] itself rather than a
/// local mirror, so a value cannot drift between what upstream computed and
/// what this service serves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum GreeksResponse {
    /// `greeks=all`: the full twelve-value snapshot.
    ///
    /// Listed first so deserialisation prefers it — an untagged enum takes the
    /// first variant that matches, and the four-field variant would otherwise
    /// swallow a complete snapshot by ignoring the rest.
    Full(GreeksSnapshot),
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
            GreekLevel::All => Some(Self::Full(snapshot.clone())),
        }
    }
}

/// The greek payloads for both sides of one strike, at the requested level.
///
/// Upstream can compute the snapshots at chain build time, but only when the
/// build asks for them, and nothing in this crate does: `with_greek_snapshots`
/// is off by default, and turning it on globally would charge every client for
/// a payload most never request. So in practice **this function computes
/// them**, through upstream's own `calculate_greeks`; the `is_some` branch is
/// the guard for a chain that already carries them, not the normal path. No
/// greek mathematics lives in this crate either way.
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
/// Roughly 40 µs per contract in a release build. `first` and `all` cost the
/// same: upstream builds the snapshot whole and the level only decides what is
/// written out. Computing here rather than at build time is measurably more
/// expensive per contract, because `calculate_greeks` recomputes delta and
/// gamma that this response then reads from the mirrors instead; asking the
/// builder for snapshots costs about 1.5x a plain chain build. The trade is
/// deliberate — every client would pay the build-time cost, and only some ask
/// for the payload.
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
