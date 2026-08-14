//! Operational configuration for v2 rolling simulations.
//!
//! Everything here is about **real** time and **real** memory: how long an idle
//! simulation is kept, how often expired ones are reaped, and how much of the
//! expensive derived state stays resident. None of it touches the *simulated*
//! clock, which may span years regardless.
//!
//! That separation is the point of the module. A simulation whose simulated
//! horizon is three years is still walked one request at a time, and losing it
//! to a thirty-minute idle timeout would make the horizon unusable — so v2 gets
//! its own retention window rather than inheriting v1's.
//!
//! # Why this validates instead of falling back
//!
//! `api::rest::limits` reads its caps with a warn-and-default, because a bad
//! `OCS_MAX_STEPS` degrades one request. These knobs are different: a retention
//! window silently reset to a default would expire simulations a client is
//! still walking, and a cache capacity silently reset would change the
//! service's memory profile without anyone noticing. So an invalid value fails
//! startup with a message naming the variable, which is what
//! `rules/global_rules.md` asks of a configuration knob.

use crate::utils::ChainError;
use std::env;
use std::time::Duration;
use tracing::info;

/// Default idle retention for a v2 simulation, in seconds.
///
/// An hour rather than v1's thirty minutes: a v2 simulation is walked one
/// request at a time over a long simulated horizon, and a client pausing
/// between steps must not lose it.
pub const DEFAULT_RETENTION_SECS: u64 = 3_600;

/// Default interval between cleanup passes, in seconds.
///
/// Frequent enough that an expired simulation's caches are reclaimed promptly,
/// rare enough that the pass itself is not a load.
pub const DEFAULT_CLEANUP_INTERVAL_SECS: u64 = 60;

/// Default number of factor tapes held resident.
///
/// A tape is `O(steps)` four-field rows, so this is generous relative to the
/// snapshot bound.
pub const DEFAULT_MAX_CACHED_TAPES: usize = 64;

/// Default number of snapshots held resident.
///
/// A snapshot holds every strike of every live expiration, so the bound is
/// deliberately small. Issue #62 tracks bounding it by weight rather than by
/// entry count.
pub const DEFAULT_MAX_CACHED_SNAPSHOTS: usize = 256;

/// The longest retention window that can be configured, in seconds — thirty
/// days.
///
/// Not a technical limit but an operational one: a window longer than this is
/// almost certainly a typo (seconds entered as milliseconds, say), and the
/// consequence would be simulations pinning memory for weeks.
const MAX_RETENTION_SECS: u64 = 30 * 24 * 3_600;

/// The longest cleanup interval that can be configured, in seconds — one hour.
const MAX_CLEANUP_INTERVAL_SECS: u64 = 3_600;

/// The largest cache bound that can be configured.
const MAX_CACHE_CAPACITY: usize = 1_000_000;

/// Default cap on the steps one export request may cover.
///
/// Bounds the *work*, not the memory: streaming already keeps the response
/// bounded, but an `option_chains` export is `steps × expirations × strikes`
/// priced contracts, so an unbounded range is minutes of CPU on a blocking
/// thread. A client wanting more pages the range.
pub const DEFAULT_MAX_EXPORT_ROWS: usize = 100_000;

/// The largest export range that can be configured.
const MAX_EXPORT_ROWS_CEILING: usize = 10_000_000;

/// Operational limits for the v2 rolling-simulation surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulationV2Config {
    /// How long an idle simulation is kept before it is reaped.
    ///
    /// Measured from the last **write**, not the last read: a peek persists
    /// nothing (ADR 0001 §6), so a client that only peeks does not refresh the
    /// window. Both stores behave the same way.
    pub retention: Duration,
    /// How often the cleanup pass runs.
    pub cleanup_interval: Duration,
    /// How many factor tapes stay resident.
    pub max_cached_tapes: usize,
    /// How many snapshots stay resident.
    pub max_cached_snapshots: usize,
    /// How many steps one export request may cover.
    pub max_export_rows: usize,
}

impl Default for SimulationV2Config {
    fn default() -> Self {
        Self {
            retention: Duration::from_secs(DEFAULT_RETENTION_SECS),
            cleanup_interval: Duration::from_secs(DEFAULT_CLEANUP_INTERVAL_SECS),
            max_cached_tapes: DEFAULT_MAX_CACHED_TAPES,
            max_cached_snapshots: DEFAULT_MAX_CACHED_SNAPSHOTS,
            max_export_rows: DEFAULT_MAX_EXPORT_ROWS,
        }
    }
}

impl SimulationV2Config {
    /// Reads the configuration from the environment.
    ///
    /// An unset variable takes its documented default. A set-but-invalid one
    /// fails, rather than falling back — see the module docs for why.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming the environment variable when
    /// its value does not parse, is zero, or exceeds its documented bound.
    pub fn from_env() -> Result<Self, ChainError> {
        let config = Self {
            retention: Duration::from_secs(parse_secs(
                "OCS_V2_RETENTION_SECS",
                read("OCS_V2_RETENTION_SECS").as_deref(),
                DEFAULT_RETENTION_SECS,
                MAX_RETENTION_SECS,
            )?),
            cleanup_interval: Duration::from_secs(parse_secs(
                "OCS_V2_CLEANUP_INTERVAL_SECS",
                read("OCS_V2_CLEANUP_INTERVAL_SECS").as_deref(),
                DEFAULT_CLEANUP_INTERVAL_SECS,
                MAX_CLEANUP_INTERVAL_SECS,
            )?),
            max_cached_tapes: parse_capacity(
                "OCS_MAX_CACHED_TAPES",
                read("OCS_MAX_CACHED_TAPES").as_deref(),
                DEFAULT_MAX_CACHED_TAPES,
            )?,
            max_cached_snapshots: parse_capacity(
                "OCS_MAX_CACHED_SNAPSHOTS",
                read("OCS_MAX_CACHED_SNAPSHOTS").as_deref(),
                DEFAULT_MAX_CACHED_SNAPSHOTS,
            )?,
            max_export_rows: parse_bounded(
                "OCS_MAX_EXPORT_ROWS",
                read("OCS_MAX_EXPORT_ROWS").as_deref(),
                DEFAULT_MAX_EXPORT_ROWS,
                MAX_EXPORT_ROWS_CEILING,
            )?,
        };

        info!(
            retention_secs = config.retention.as_secs(),
            cleanup_interval_secs = config.cleanup_interval.as_secs(),
            max_cached_tapes = config.max_cached_tapes,
            max_cached_snapshots = config.max_cached_snapshots,
            max_export_rows = config.max_export_rows,
            "Loaded the v2 simulation configuration"
        );
        Ok(config)
    }

    /// The retention window in seconds, for the stores that take one.
    #[must_use]
    pub fn retention_secs(&self) -> u64 {
        self.retention.as_secs()
    }
}

/// Parses a duration knob, in seconds.
///
/// Takes the raw value rather than reading it, so the parsing and the bounds
/// can be tested without mutating the process environment — which would need
/// `unsafe` in this edition and would race every other test in the binary.
fn parse_secs(
    variable: &str,
    raw: Option<&str>,
    default: u64,
    max: u64,
) -> Result<u64, ChainError> {
    let Some(raw) = raw else {
        return Ok(default);
    };

    let seconds = raw.parse::<u64>().map_err(|_| invalid(variable, raw))?;
    if seconds == 0 {
        return Err(ChainError::Validation {
            field: variable.to_string(),
            reason: "must be at least 1 second".to_string(),
        });
    }
    if seconds > max {
        return Err(ChainError::Validation {
            field: variable.to_string(),
            reason: format!("must not exceed {max} seconds, got {seconds}"),
        });
    }
    Ok(seconds)
}

/// Parses a cache-capacity knob.
///
/// Takes the raw value for the same reason as [`parse_secs`].
fn parse_capacity(variable: &str, raw: Option<&str>, default: usize) -> Result<usize, ChainError> {
    parse_bounded(variable, raw, default, MAX_CACHE_CAPACITY)
}

/// Parses a positive count with an explicit ceiling.
///
/// Takes the raw value for the same reason as [`parse_secs`].
fn parse_bounded(
    variable: &str,
    raw: Option<&str>,
    default: usize,
    max: usize,
) -> Result<usize, ChainError> {
    let Some(raw) = raw else {
        return Ok(default);
    };

    let value = raw.parse::<usize>().map_err(|_| invalid(variable, raw))?;
    if value == 0 {
        return Err(ChainError::Validation {
            field: variable.to_string(),
            reason: "must be at least 1".to_string(),
        });
    }
    if value > max {
        return Err(ChainError::Validation {
            field: variable.to_string(),
            reason: format!("must not exceed {max}, got {value}"),
        });
    }
    Ok(value)
}

/// Reads a variable, treating an empty or whitespace-only value as unset.
///
/// A blank value in a `.env` file is how a knob gets "commented out" in
/// practice; treating it as unset is friendlier than failing startup over it,
/// and unambiguous either way.
fn read(variable: &str) -> Option<String> {
    let raw = env::var(variable).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The error for a value that does not parse.
#[cold]
fn invalid(variable: &str, raw: &str) -> ChainError {
    ChainError::Validation {
        field: variable.to_string(),
        reason: format!("must be a whole number, got {raw:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults are the documented ones.
    #[test]
    fn test_the_defaults_are_the_documented_values() {
        let config = SimulationV2Config::default();

        assert_eq!(config.retention.as_secs(), DEFAULT_RETENTION_SECS);
        assert_eq!(
            config.cleanup_interval.as_secs(),
            DEFAULT_CLEANUP_INTERVAL_SECS
        );
        assert_eq!(config.max_cached_tapes, DEFAULT_MAX_CACHED_TAPES);
        assert_eq!(config.max_cached_snapshots, DEFAULT_MAX_CACHED_SNAPSHOTS);
        assert_eq!(config.retention_secs(), DEFAULT_RETENTION_SECS);
    }

    /// The v2 retention outlasts v1's thirty minutes, which is the whole reason
    /// it is a separate knob.
    ///
    /// v1's window is not a constant this crate can reference — it is the
    /// literal `1800` default inside `InRedisSessionStore::new` — so the
    /// comparison is written against that number directly.
    #[test]
    fn test_the_v2_retention_outlasts_the_v1_default() {
        let v1_default_secs: u64 = 1_800;

        assert!(
            SimulationV2Config::default().retention_secs() > v1_default_secs,
            "a v2 simulation is walked one request at a time over a long horizon"
        );
    }

    /// An unset knob takes its default.
    #[test]
    fn test_an_unset_duration_takes_its_default() {
        match parse_secs("OCS_V2_RETENTION_SECS", None, 900, 3_600) {
            Ok(seconds) => assert_eq!(seconds, 900),
            Err(error) => panic!("an unset knob must take its default: {error}"),
        }
    }

    /// A valid duration is accepted verbatim.
    #[test]
    fn test_a_valid_duration_is_accepted() {
        match parse_secs("OCS_V2_RETENTION_SECS", Some("120"), 900, 3_600) {
            Ok(seconds) => assert_eq!(seconds, 120),
            Err(error) => panic!("a valid duration must be accepted: {error}"),
        }
    }

    /// A duration that does not parse fails, naming the variable — rather than
    /// falling back and silently changing how long simulations live.
    #[test]
    fn test_an_unparseable_duration_fails_by_name() {
        match parse_secs("OCS_V2_RETENTION_SECS", Some("an hour"), 900, 3_600) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "OCS_V2_RETENTION_SECS");
                assert!(reason.contains("whole number"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A zero retention would expire every simulation instantly.
    #[test]
    fn test_a_zero_duration_is_rejected() {
        match parse_secs("OCS_V2_RETENTION_SECS", Some("0"), 900, 3_600) {
            Err(ChainError::Validation { reason, .. }) => {
                assert!(reason.contains("at least 1 second"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A retention beyond the operational ceiling is almost certainly a typo,
    /// and would pin memory for weeks.
    #[test]
    fn test_a_duration_beyond_its_ceiling_is_rejected() {
        match parse_secs("OCS_V2_RETENTION_SECS", Some("999999999"), 900, 3_600) {
            Err(ChainError::Validation { reason, .. }) => {
                assert!(reason.contains("must not exceed"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// An unset capacity takes its default; a valid one is accepted.
    #[test]
    fn test_capacity_parsing_accepts_valid_values() {
        match parse_capacity("OCS_MAX_CACHED_TAPES", None, 64) {
            Ok(capacity) => assert_eq!(capacity, 64),
            Err(error) => panic!("an unset knob must take its default: {error}"),
        }
        match parse_capacity("OCS_MAX_CACHED_TAPES", Some("8"), 64) {
            Ok(capacity) => assert_eq!(capacity, 8),
            Err(error) => panic!("a valid capacity must be accepted: {error}"),
        }
    }

    /// A zero capacity would make the cache inert, so it fails rather than
    /// silently reverting to the default.
    #[test]
    fn test_a_zero_capacity_is_rejected() {
        match parse_capacity("OCS_MAX_CACHED_SNAPSHOTS", Some("0"), 256) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "OCS_MAX_CACHED_SNAPSHOTS");
                assert!(reason.contains("at least 1"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A capacity beyond the ceiling is rejected.
    #[test]
    fn test_a_capacity_beyond_its_ceiling_is_rejected() {
        match parse_capacity("OCS_MAX_CACHED_SNAPSHOTS", Some("99999999"), 256) {
            Err(ChainError::Validation { reason, .. }) => {
                assert!(reason.contains("must not exceed"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A capacity that does not parse fails by name.
    #[test]
    fn test_an_unparseable_capacity_fails_by_name() {
        match parse_capacity("OCS_MAX_CACHED_TAPES", Some("lots"), 64) {
            Err(ChainError::Validation { field, .. }) => {
                assert_eq!(field, "OCS_MAX_CACHED_TAPES");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// Both backends apply the same default retention.
    ///
    /// ADR 0001 §9.1 requires the in-memory and Redis stores to agree on
    /// expiry, and two independently-named defaults are exactly how that
    /// agreement drifts. There is now one number, and this asserts every path
    /// still reaches it.
    #[test]
    fn test_both_stores_default_to_the_configured_retention() {
        use crate::session::{DEFAULT_V2_RETENTION_SECS, InMemorySimulationStore};

        assert_eq!(DEFAULT_V2_RETENTION_SECS, DEFAULT_RETENTION_SECS);
        assert_eq!(
            InMemorySimulationStore::new().idle_retention(),
            SimulationV2Config::default().retention,
            "the in-memory store must apply the configured window"
        );
    }

    /// The configuration loads from the ambient environment.
    ///
    /// The service is deployed with these unset far more often than not, so the
    /// path that has to work is the one that takes every default.
    #[test]
    fn test_it_loads_from_the_environment() {
        match SimulationV2Config::from_env() {
            Ok(config) => {
                assert!(config.retention.as_secs() >= 1);
                assert!(config.max_cached_tapes >= 1);
                assert!(config.max_cached_snapshots >= 1);
            }
            Err(error) => panic!("the ambient environment must load: {error}"),
        }
    }
}
