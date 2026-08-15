//! Operational configuration for persisting v2 simulation snapshots.
//!
//! The snapshot tape is written to ClickHouse (issue #56) as two flat tables
//! rather than to MongoDB as nested documents. Everything an operator can tune
//! about that write path lives here: whether it happens at all, how large one
//! snapshot may be, how long the service waits for the insert, how much one
//! read may return, and how long the rows are kept.
//!
//! # Why this validates instead of falling back
//!
//! Same reasoning as [`crate::infrastructure::SimulationV2Config`]: a
//! silently-defaulted retention would delete a tape a client still expects to
//! be able to export, and a silently-defaulted row bound would change the
//! service's memory profile without anyone noticing. So an unset knob takes its
//! documented default and a **set-but-invalid** one fails startup with a
//! message naming the variable.
//!
//! # Why persistence is off by default
//!
//! ClickHouse is optional in this service — `Simulator::new` already tolerates
//! its absence — and a deployment without it must not log a failed write on
//! every advance. Persistence is therefore opt-in: an operator who has the
//! warehouse turns it on explicitly.

use crate::infrastructure::config::simulation_v2::{
    DEFAULT_MAX_SNAPSHOT_CONTRACTS, max_snapshot_contracts,
};
use crate::utils::ChainError;
use std::env;
use std::time::Duration;
use tracing::info;

/// Whether snapshot persistence is on when `OCS_SNAPSHOT_PERSISTENCE_ENABLED`
/// is unset.
///
/// Off: see the module docs.
pub const DEFAULT_SNAPSHOT_PERSISTENCE_ENABLED: bool = false;

/// Default cap on the quote rows one snapshot may carry, in rows.
///
/// A snapshot is `expirations × strikes` rows; ADR 0001's reference
/// configuration is a few hundred. This is the *storage* bound: a snapshot
/// larger than it is rejected before a single row is written, so one
/// pathological simulation cannot turn into an unbounded insert.
///
/// It defaults to the per-snapshot contract cap the API accepts, and not
/// lower, on purpose. A storage bound below what creation admits would make
/// every persist of a perfectly legal simulation fail — the API would say yes
/// and the warehouse would say no, for every step, forever. `from_env` refuses
/// that combination outright rather than leaving it to be discovered in the
/// logs.
pub const DEFAULT_SNAPSHOT_BATCH_ROWS: usize = DEFAULT_MAX_SNAPSHOT_CONTRACTS;

/// Default cap on the rows one read may return, in rows.
///
/// Bounds `stream_range` and `contract_series`, which #49's exports page
/// through. A range that would exceed it is a typed error naming the knob
/// rather than a truncated answer, because a silently short tape is
/// indistinguishable from a gap in the data.
pub const DEFAULT_SNAPSHOT_MAX_READ_ROWS: usize = 1_000_000;

/// Default insert timeout, in seconds.
///
/// Covers both sending the batch and waiting for the server to accept it. A
/// snapshot that cannot be written in half a minute is a degraded warehouse,
/// and the advance path must not block on it.
pub const DEFAULT_SNAPSHOT_INSERT_TIMEOUT_SECS: u64 = 30;

/// Default retention for persisted snapshot rows, in days.
///
/// Applied as a row-level `TTL` on the ingestion timestamp when the schema is
/// created — real time, not simulated time, because a simulation's clock may
/// sit years in the future.
pub const DEFAULT_SNAPSHOT_RETENTION_DAYS: u32 = 90;

/// The largest per-snapshot row bound that can be configured.
///
/// Five million rows is far past any snapshot the domain will price, so a
/// larger value is a typo rather than an intent.
const MAX_BATCH_ROWS: usize = 5_000_000;

/// The largest per-read row bound that can be configured.
const MAX_READ_ROWS_CEILING: usize = 10_000_000;

/// The longest insert timeout that can be configured, in seconds.
const MAX_INSERT_TIMEOUT_SECS: u64 = 600;

/// The longest retention that can be configured, in days — ten years.
const MAX_RETENTION_DAYS: u32 = 3_650;

/// Operational limits for the v2 snapshot tape in ClickHouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotPersistenceConfig {
    /// Whether generated snapshots are written to ClickHouse at all.
    ///
    /// Consumed at construction:
    /// [`crate::infrastructure::ClickHouseSnapshotRepository::from_env`] returns
    /// `None` when this is false, so a disabled deployment never holds a
    /// repository and never attempts a write.
    pub enabled: bool,
    /// The most quote rows one snapshot may carry.
    pub batch_rows: usize,
    /// The most rows one read may return.
    pub max_read_rows: usize,
    /// How long an insert may take, sending and server-side combined.
    pub insert_timeout: Duration,
    /// How long persisted rows are kept, in days, measured from ingestion.
    pub retention_days: u32,
}

impl Default for SnapshotPersistenceConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_SNAPSHOT_PERSISTENCE_ENABLED,
            batch_rows: DEFAULT_SNAPSHOT_BATCH_ROWS,
            max_read_rows: DEFAULT_SNAPSHOT_MAX_READ_ROWS,
            insert_timeout: Duration::from_secs(DEFAULT_SNAPSHOT_INSERT_TIMEOUT_SECS),
            retention_days: DEFAULT_SNAPSHOT_RETENTION_DAYS,
        }
    }
}

impl SnapshotPersistenceConfig {
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
            enabled: parse_bool(
                "OCS_SNAPSHOT_PERSISTENCE_ENABLED",
                read("OCS_SNAPSHOT_PERSISTENCE_ENABLED").as_deref(),
                DEFAULT_SNAPSHOT_PERSISTENCE_ENABLED,
            )?,
            batch_rows: parse_bounded_usize(
                "OCS_SNAPSHOT_BATCH_ROWS",
                read("OCS_SNAPSHOT_BATCH_ROWS").as_deref(),
                DEFAULT_SNAPSHOT_BATCH_ROWS,
                MAX_BATCH_ROWS,
            )?,
            max_read_rows: parse_bounded_usize(
                "OCS_SNAPSHOT_MAX_READ_ROWS",
                read("OCS_SNAPSHOT_MAX_READ_ROWS").as_deref(),
                DEFAULT_SNAPSHOT_MAX_READ_ROWS,
                MAX_READ_ROWS_CEILING,
            )?,
            insert_timeout: Duration::from_secs(parse_bounded_u64(
                "OCS_SNAPSHOT_INSERT_TIMEOUT_SECS",
                read("OCS_SNAPSHOT_INSERT_TIMEOUT_SECS").as_deref(),
                DEFAULT_SNAPSHOT_INSERT_TIMEOUT_SECS,
                MAX_INSERT_TIMEOUT_SECS,
            )?),
            retention_days: parse_bounded_u32(
                "OCS_SNAPSHOT_RETENTION_DAYS",
                read("OCS_SNAPSHOT_RETENTION_DAYS").as_deref(),
                DEFAULT_SNAPSHOT_RETENTION_DAYS,
                MAX_RETENTION_DAYS,
            )?,
        };

        // The two bounds have to agree, or the service accepts simulations it
        // can never persist. Checked here rather than at the write, because the
        // failure is a configuration mistake and belongs at startup with both
        // variables named.
        let accepted = max_snapshot_contracts();
        if config.enabled && config.batch_rows < accepted {
            return Err(ChainError::Validation {
                field: "OCS_SNAPSHOT_BATCH_ROWS".to_string(),
                reason: format!(
                    "is {} but OCS_MAX_SNAPSHOT_CONTRACTS accepts snapshots of up to \
                     {accepted} rows, so every persist of a snapshot above {} would fail; \
                     raise this or lower that",
                    config.batch_rows, config.batch_rows
                ),
            });
        }

        info!(
            enabled = config.enabled,
            batch_rows = config.batch_rows,
            max_read_rows = config.max_read_rows,
            insert_timeout_secs = config.insert_timeout.as_secs(),
            retention_days = config.retention_days,
            "Loaded the v2 snapshot persistence configuration"
        );
        Ok(config)
    }
}

/// Parses a boolean knob.
///
/// Accepts the spellings an operator actually writes in a `.env` file, in
/// either case. Anything else fails rather than being read as `false`: a
/// misspelled `OCS_SNAPSHOT_PERSISTENCE_ENABLED=treu` that silently disabled
/// persistence would be discovered days later, by its absence.
fn parse_bool(variable: &str, raw: Option<&str>, default: bool) -> Result<bool, ChainError> {
    let Some(raw) = raw else {
        return Ok(default);
    };

    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ChainError::Validation {
            field: variable.to_string(),
            reason: format!("must be a boolean (true/false), got {raw:?}"),
        }),
    }
}

/// Parses a positive row count with an explicit ceiling.
///
/// Takes the raw value rather than reading it, so the parsing and the bounds
/// can be tested without mutating the process environment — which would need
/// `unsafe` in this edition and would race every other test in the binary.
fn parse_bounded_usize(
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
        return Err(zero(variable));
    }
    if value > max {
        return Err(too_large(variable, &value.to_string(), &max.to_string()));
    }
    Ok(value)
}

/// Parses a positive duration in seconds with an explicit ceiling.
fn parse_bounded_u64(
    variable: &str,
    raw: Option<&str>,
    default: u64,
    max: u64,
) -> Result<u64, ChainError> {
    let Some(raw) = raw else {
        return Ok(default);
    };

    let value = raw.parse::<u64>().map_err(|_| invalid(variable, raw))?;
    if value == 0 {
        return Err(zero(variable));
    }
    if value > max {
        return Err(too_large(variable, &value.to_string(), &max.to_string()));
    }
    Ok(value)
}

/// Parses a positive day count with an explicit ceiling.
fn parse_bounded_u32(
    variable: &str,
    raw: Option<&str>,
    default: u32,
    max: u32,
) -> Result<u32, ChainError> {
    let Some(raw) = raw else {
        return Ok(default);
    };

    let value = raw.parse::<u32>().map_err(|_| invalid(variable, raw))?;
    if value == 0 {
        return Err(zero(variable));
    }
    if value > max {
        return Err(too_large(variable, &value.to_string(), &max.to_string()));
    }
    Ok(value)
}

/// The error for a count of zero.
///
/// Every knob here is a count, and zero disables something the operator meant
/// to configure — a zero batch bound rejects every snapshot, a zero retention
/// deletes the tape as it is written.
#[cold]
fn zero(variable: &str) -> ChainError {
    ChainError::Validation {
        field: variable.to_string(),
        reason: "must be at least 1".to_string(),
    }
}

/// The error for a count above its documented ceiling.
///
/// Takes the two numbers already rendered, so one message serves the three
/// integer widths without widening arithmetic that would only exist for the
/// error path.
#[cold]
fn too_large(variable: &str, value: &str, max: &str) -> ChainError {
    ChainError::Validation {
        field: variable.to_string(),
        reason: format!("must not exceed {max}, got {value}"),
    }
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
    /// The storage bound defaults to what creation accepts, so the default
    /// configuration cannot accept a simulation it could never persist.
    #[test]
    fn test_the_default_storage_bound_covers_what_creation_accepts() {
        const {
            assert!(
                DEFAULT_SNAPSHOT_BATCH_ROWS >= DEFAULT_MAX_SNAPSHOT_CONTRACTS,
                "a storage bound below the accepted snapshot size fails every persist"
            );
        }
    }

    use super::*;

    /// The defaults are the documented ones.
    #[test]
    fn test_the_defaults_are_the_documented_values() {
        let config = SnapshotPersistenceConfig::default();

        assert_eq!(config.enabled, DEFAULT_SNAPSHOT_PERSISTENCE_ENABLED);
        assert_eq!(config.batch_rows, DEFAULT_SNAPSHOT_BATCH_ROWS);
        assert_eq!(config.max_read_rows, DEFAULT_SNAPSHOT_MAX_READ_ROWS);
        assert_eq!(
            config.insert_timeout.as_secs(),
            DEFAULT_SNAPSHOT_INSERT_TIMEOUT_SECS
        );
        assert_eq!(config.retention_days, DEFAULT_SNAPSHOT_RETENTION_DAYS);
    }

    /// Persistence is opt-in, so a deployment without ClickHouse is unaffected.
    #[test]
    fn test_persistence_is_off_by_default() {
        assert!(!SnapshotPersistenceConfig::default().enabled);
    }

    /// Every spelling an operator is likely to write is accepted.
    #[test]
    fn test_boolean_spellings_are_accepted() {
        for raw in ["1", "true", "TRUE", "Yes", "on"] {
            match parse_bool("OCS_SNAPSHOT_PERSISTENCE_ENABLED", Some(raw), false) {
                Ok(value) => assert!(value, "{raw} must enable persistence"),
                Err(error) => panic!("{raw} must parse: {error}"),
            }
        }
        for raw in ["0", "false", "FALSE", "No", "off"] {
            match parse_bool("OCS_SNAPSHOT_PERSISTENCE_ENABLED", Some(raw), true) {
                Ok(value) => assert!(!value, "{raw} must disable persistence"),
                Err(error) => panic!("{raw} must parse: {error}"),
            }
        }
    }

    /// A misspelled boolean fails by name rather than silently disabling the
    /// write path.
    #[test]
    fn test_an_unparseable_boolean_fails_by_name() {
        match parse_bool("OCS_SNAPSHOT_PERSISTENCE_ENABLED", Some("treu"), false) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "OCS_SNAPSHOT_PERSISTENCE_ENABLED");
                assert!(reason.contains("boolean"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// An unset boolean takes its default.
    #[test]
    fn test_an_unset_boolean_takes_its_default() {
        match parse_bool("OCS_SNAPSHOT_PERSISTENCE_ENABLED", None, true) {
            Ok(value) => assert!(value),
            Err(error) => panic!("an unset knob must take its default: {error}"),
        }
    }

    /// An unset count takes its default; a valid one is accepted verbatim.
    #[test]
    fn test_counts_take_their_default_and_accept_valid_values() {
        match parse_bounded_usize("OCS_SNAPSHOT_BATCH_ROWS", None, 64, 128) {
            Ok(value) => assert_eq!(value, 64),
            Err(error) => panic!("an unset knob must take its default: {error}"),
        }
        match parse_bounded_usize("OCS_SNAPSHOT_BATCH_ROWS", Some("100"), 64, 128) {
            Ok(value) => assert_eq!(value, 100),
            Err(error) => panic!("a valid count must be accepted: {error}"),
        }
    }

    /// A zero row bound would make every snapshot unpersistable.
    #[test]
    fn test_a_zero_count_is_rejected() {
        match parse_bounded_usize("OCS_SNAPSHOT_BATCH_ROWS", Some("0"), 64, 128) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "OCS_SNAPSHOT_BATCH_ROWS");
                assert!(reason.contains("at least 1"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A count beyond its ceiling is rejected.
    #[test]
    fn test_a_count_beyond_its_ceiling_is_rejected() {
        match parse_bounded_usize("OCS_SNAPSHOT_MAX_READ_ROWS", Some("999999999"), 64, 128) {
            Err(ChainError::Validation { reason, .. }) => {
                assert!(reason.contains("must not exceed"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A count that does not parse fails by name.
    #[test]
    fn test_an_unparseable_count_fails_by_name() {
        match parse_bounded_usize("OCS_SNAPSHOT_BATCH_ROWS", Some("lots"), 64, 128) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "OCS_SNAPSHOT_BATCH_ROWS");
                assert!(reason.contains("whole number"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// The timeout knob obeys the same rules as the counts.
    #[test]
    fn test_the_timeout_is_bounded() {
        match parse_bounded_u64("OCS_SNAPSHOT_INSERT_TIMEOUT_SECS", Some("45"), 30, 600) {
            Ok(value) => assert_eq!(value, 45),
            Err(error) => panic!("a valid timeout must be accepted: {error}"),
        }
        match parse_bounded_u64("OCS_SNAPSHOT_INSERT_TIMEOUT_SECS", Some("6000"), 30, 600) {
            Err(ChainError::Validation { field, .. }) => {
                assert_eq!(field, "OCS_SNAPSHOT_INSERT_TIMEOUT_SECS");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// The retention knob obeys the same rules, and its ceiling is ten years.
    #[test]
    fn test_the_retention_is_bounded() {
        match parse_bounded_u32(
            "OCS_SNAPSHOT_RETENTION_DAYS",
            Some("30"),
            90,
            MAX_RETENTION_DAYS,
        ) {
            Ok(value) => assert_eq!(value, 30),
            Err(error) => panic!("a valid retention must be accepted: {error}"),
        }
        match parse_bounded_u32(
            "OCS_SNAPSHOT_RETENTION_DAYS",
            Some("40000"),
            90,
            MAX_RETENTION_DAYS,
        ) {
            Err(ChainError::Validation { reason, .. }) => {
                assert!(reason.contains("must not exceed"), "{reason}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// The configuration loads from the ambient environment.
    ///
    /// The service is deployed with these unset far more often than not, so the
    /// path that has to work is the one that takes every default.
    #[test]
    fn test_it_loads_from_the_environment() {
        match SnapshotPersistenceConfig::from_env() {
            Ok(config) => {
                assert!(config.batch_rows >= 1);
                assert!(config.max_read_rows >= 1);
                assert!(config.retention_days >= 1);
                assert!(config.insert_timeout.as_secs() >= 1);
            }
            Err(error) => panic!("the ambient environment must load: {error}"),
        }
    }
}
