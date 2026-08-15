//! Configurable upper bounds for client-supplied simulation parameters.
//!
//! These caps protect the service from pathological requests (an enormous number of
//! steps, an oversized option chain, or a huge historical price series) that would
//! otherwise blow up memory or CPU. Each limit is read once from an environment
//! variable via [`std::sync::LazyLock`]; an unset or invalid value falls back to the
//! documented default and emits a `tracing::warn!`.
//!
//! | Env var                    | Default   | Meaning                                  |
//! |----------------------------|-----------|------------------------------------------|
//! | `OCS_MAX_STEPS`            | `10_000`  | Max simulation steps per session         |
//! | `OCS_MAX_CHAIN_SIZE`       | `500`     | Max option-chain size per request        |
//! | `OCS_MAX_HISTORICAL_PRICES`| `100_000` | Max historical prices in a walk request  |
//! | `OCS_MAX_SNAPSHOT_CONTRACTS`| `200_000`| Max contracts one v2 snapshot may price |

use std::sync::LazyLock;
use tracing::warn;

/// Default cap on the number of simulation steps per session.
pub(crate) const DEFAULT_MAX_STEPS: usize = 10_000;
/// Default cap on the option-chain size per request.
pub(crate) const DEFAULT_MAX_CHAIN_SIZE: usize = 500;
/// Default cap on the number of historical prices in a `Historical` walk request.
pub(crate) const DEFAULT_MAX_HISTORICAL_PRICES: usize = 100_000;
/// Default cap on the contracts one v2 snapshot may price.
///
/// A snapshot prices every strike of every live expiration, so its cost is the
/// product of two caps that each look reasonable alone: `OCS_MAX_CHAIN_SIZE`
/// of 500 is 1 001 strikes, and a schedule may keep 512 expirations alive, for
/// half a million contracts in one request. The default admits any realistic
/// configuration — ADR 0001's reference schedule prices about 500 contracts a
/// snapshot, and 200 000 still allows 200 expirations at 1 001 strikes — while
/// refusing the shapes that exist only to exhaust the service.
pub(crate) const DEFAULT_MAX_SNAPSHOT_CONTRACTS: usize = 200_000;

/// Maximum number of simulation steps a session may request (`OCS_MAX_STEPS`).
pub(crate) static MAX_STEPS: LazyLock<usize> =
    LazyLock::new(|| parse_limit(std::env::var("OCS_MAX_STEPS").ok(), DEFAULT_MAX_STEPS));

/// Maximum option-chain size a request may ask for (`OCS_MAX_CHAIN_SIZE`).
pub(crate) static MAX_CHAIN_SIZE: LazyLock<usize> = LazyLock::new(|| {
    parse_limit(
        std::env::var("OCS_MAX_CHAIN_SIZE").ok(),
        DEFAULT_MAX_CHAIN_SIZE,
    )
});

/// Maximum number of historical prices a `Historical` walk may carry
/// (`OCS_MAX_HISTORICAL_PRICES`).
pub(crate) static MAX_HISTORICAL_PRICES: LazyLock<usize> = LazyLock::new(|| {
    parse_limit(
        std::env::var("OCS_MAX_HISTORICAL_PRICES").ok(),
        DEFAULT_MAX_HISTORICAL_PRICES,
    )
});

/// Maximum number of contracts one v2 snapshot may price
/// (`OCS_MAX_SNAPSHOT_CONTRACTS`).
pub(crate) static MAX_SNAPSHOT_CONTRACTS: LazyLock<usize> = LazyLock::new(|| {
    parse_limit(
        std::env::var("OCS_MAX_SNAPSHOT_CONTRACTS").ok(),
        DEFAULT_MAX_SNAPSHOT_CONTRACTS,
    )
});

/// The number of strikes a chain of `chain_size` carries.
///
/// `chain_size` is upstream's per-side half-width — it counts strikes above
/// *and* below the money — so the ladder is `2n + 1` wide. Saturating, because
/// the caller may be validating a value that has not been range-checked yet and
/// an overflow here would be the wrong error to report.
#[must_use]
pub(crate) fn strikes_per_chain(chain_size: usize) -> usize {
    chain_size.saturating_mul(2).saturating_add(1)
}

/// Parses a raw environment value into a positive `usize` limit.
///
/// Returns `default` when `raw` is `None` (variable unset) or when it does not parse
/// into an integer `>= 1`. Invalid values are logged at `WARN` and never abort startup,
/// keeping the service resilient to misconfiguration.
///
/// # Examples
///
/// ```ignore
/// assert_eq!(parse_limit(None, 10), 10);
/// assert_eq!(parse_limit(Some("42".to_string()), 10), 42);
/// assert_eq!(parse_limit(Some("nope".to_string()), 10), 10);
/// ```
#[must_use]
pub(crate) fn parse_limit(raw: Option<String>, default: usize) -> usize {
    match raw {
        None => default,
        Some(value) => match value.trim().parse::<usize>() {
            Ok(parsed) if parsed >= 1 => parsed,
            _ => {
                warn!(
                    raw = %value,
                    default,
                    "invalid limit value; falling back to default"
                );
                default
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_limit_unset_uses_default() {
        assert_eq!(parse_limit(None, 10), 10);
    }

    #[test]
    fn test_parse_limit_valid_value_is_used() {
        assert_eq!(parse_limit(Some("42".to_string()), 10), 42);
    }

    #[test]
    fn test_parse_limit_trims_whitespace() {
        assert_eq!(parse_limit(Some("  25  ".to_string()), 10), 25);
    }

    #[test]
    fn test_parse_limit_non_numeric_falls_back() {
        assert_eq!(parse_limit(Some("not-a-number".to_string()), 10), 10);
    }

    #[test]
    fn test_parse_limit_zero_falls_back() {
        assert_eq!(parse_limit(Some("0".to_string()), 10), 10);
    }

    #[test]
    fn test_parse_limit_negative_falls_back() {
        assert_eq!(parse_limit(Some("-5".to_string()), 10), 10);
    }

    #[test]
    fn test_default_limits_match_documentation() {
        // Without env overrides the parsed limits equal the documented defaults.
        assert_eq!(*MAX_STEPS, DEFAULT_MAX_STEPS);
        assert_eq!(*MAX_STEPS, 10_000);
        assert_eq!(*MAX_CHAIN_SIZE, DEFAULT_MAX_CHAIN_SIZE);
        assert_eq!(*MAX_CHAIN_SIZE, 500);
        assert_eq!(*MAX_HISTORICAL_PRICES, DEFAULT_MAX_HISTORICAL_PRICES);
        assert_eq!(*MAX_HISTORICAL_PRICES, 100_000);
    }
}
