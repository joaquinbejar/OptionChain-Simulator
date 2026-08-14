//! Session model for v2 rolling simulations.
//!
//! Separate from [`crate::session::model`] on purpose. `SimulationParameters`
//! and `Session` are public, IronCondor-facing, and persisted as serde JSON, so
//! adding rolling fields to them would be source-breaking even where serde
//! defaults kept old documents loading. ADR 0001 §12 therefore gives v2 its own
//! types, its own stored schema, and its own store key space, and freezes v1
//! exactly as it is.
//!
//! Two properties distinguish these types from their v1 counterparts:
//!
//! - **The effective inputs are resolved, not optional.** After conversion the
//!   seed, the simulated start and the step interval are concrete values, not
//!   `Option`s that some later code path has to default again. Whatever the
//!   request omitted is decided once, here, and echoed back so the run can be
//!   replayed.
//! - **The configuration is immutable.** There is no PATCH or PUT for a v2
//!   simulation: changing any parameter changes the tape, so it creates a new
//!   simulation instead of mutating one.

use crate::api::rest::limits::{MAX_CHAIN_SIZE, MAX_STEPS};
use crate::api::rest::models::validate_walk_type;
use crate::api::rest::requests_v2::CreateSimulationRequest;
use crate::api::rest::validation::{
    decimal_field, positive_field, strictly_positive_field, symbol_field, time_frame_field,
};
use crate::domain::expiry::{CalendarVersion, ExpirationSchedule, tzdb_version};
use crate::session::model::{SessionState, SimulationMethod};
use crate::utils::{ChainError, UuidGenerator};
use chrono::{DateTime, NaiveTime, TimeDelta, Timelike, Utc};
use chrono_tz::Tz;
use optionstratlib::utils::TimeFrame;
use positive::Positive;
use rand::RngExt;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;
use tracing::warn;
use uuid::Uuid;

/// Schema version stamped on every stored v2 session.
///
/// It rides with the document rather than being inferred from its shape, so a
/// future migration can tell an old document from a new one without guessing.
/// Bumping it is a semver event for the stored contract (ADR 0001 §12.2).
pub const SESSION_V2_SCHEMA_VERSION: u32 = 1;

/// Shortest simulated step interval, in seconds.
///
/// Crate-internal on purpose: issue #48 turns the v2 bounds into validated
/// `OCS_MAX_*` environment knobs following the `LazyLock` pattern in
/// `api::rest::limits`, and publishing them as `const u64` first would make
/// that conversion a breaking change to a released item.
pub(crate) const MIN_STEP_INTERVAL_SECONDS: u64 = 1;

/// Longest simulated step interval, in seconds — one 365-day year.
///
/// Crate-internal for the same reason as [`MIN_STEP_INTERVAL_SECONDS`].
pub(crate) const MAX_STEP_INTERVAL_SECONDS: u64 = 31_536_000;

/// Seconds in a 365-day year, used to derive an interval from a `Custom`
/// time frame expressed in periods per year.
const SECONDS_PER_YEAR: u64 = 31_536_000;

/// The calendar policy accepted today. Anything else is rejected so a stored
/// simulation can never be reinterpreted under a policy it was not created
/// with.
const SUPPORTED_CALENDAR: &str = "weekdays_v1";

/// Derives the simulated step interval from a stochastic-model time frame.
///
/// `time_frame` scales the model; `step_interval_seconds` drives the simulated
/// clock. They are allowed to differ, and this is only the default used when
/// the request omits the interval.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] naming `step_interval_seconds` when the
/// frame has no usable second-length: `Microsecond` and `Millisecond` derive to
/// less than one second, and a small `Custom` periods-per-year derives to more
/// than a year. Both are rejected rather than silently clamped, because a
/// clamped interval would silently change the simulated clock.
fn derive_step_interval_seconds(time_frame: TimeFrame) -> Result<u64, ChainError> {
    let too_small = || ChainError::Validation {
        field: "step_interval_seconds".to_string(),
        reason: format!(
            "cannot be derived from time_frame {time_frame:?}: it is shorter than {MIN_STEP_INTERVAL_SECONDS} second; supply step_interval_seconds explicitly"
        ),
    };

    let seconds = match time_frame {
        TimeFrame::Microsecond | TimeFrame::Millisecond => return Err(too_small()),
        TimeFrame::Second => 1,
        TimeFrame::Minute => 60,
        TimeFrame::Hour => 3_600,
        TimeFrame::Day => 86_400,
        TimeFrame::Week => 604_800,
        TimeFrame::Month => 2_592_000,
        TimeFrame::Quarter => 7_776_000,
        TimeFrame::Year => SECONDS_PER_YEAR,
        TimeFrame::Custom(periods_per_year) => {
            let periods = periods_per_year.to_f64();
            if periods <= 0.0 || !periods.is_finite() {
                return Err(ChainError::Validation {
                    field: "time_frame".to_string(),
                    reason: format!(
                        "custom periods per year must be finite and positive, got {periods}"
                    ),
                });
            }
            let seconds = (SECONDS_PER_YEAR as f64 / periods).round();
            if !(MIN_STEP_INTERVAL_SECONDS as f64..=MAX_STEP_INTERVAL_SECONDS as f64)
                .contains(&seconds)
            {
                return Err(ChainError::Validation {
                    field: "step_interval_seconds".to_string(),
                    reason: format!(
                        "derived interval {seconds} s is outside [{MIN_STEP_INTERVAL_SECONDS}, {MAX_STEP_INTERVAL_SECONDS}]; supply step_interval_seconds explicitly"
                    ),
                });
            }
            seconds as u64
        }
    };

    Ok(seconds)
}

/// Rejects a `Positive` that is zero, naming the field.
///
/// `Positive` guarantees non-negative, not strictly positive, so the request
/// path's `strictly_positive_field` check has no type-level counterpart on a
/// value read back from the store.
fn reject_zero(field: &str, value: Positive) -> Result<(), ChainError> {
    if value == Positive::ZERO {
        return Err(ChainError::Validation {
            field: field.to_string(),
            reason: "must be strictly positive, got 0".to_string(),
        });
    }
    Ok(())
}

/// Validates an explicitly-supplied step interval.
fn validate_step_interval_seconds(seconds: u64) -> Result<u64, ChainError> {
    if !(MIN_STEP_INTERVAL_SECONDS..=MAX_STEP_INTERVAL_SECONDS).contains(&seconds) {
        return Err(ChainError::Validation {
            field: "step_interval_seconds".to_string(),
            reason: format!(
                "must be within [{MIN_STEP_INTERVAL_SECONDS}, {MAX_STEP_INTERVAL_SECONDS}], got {seconds}"
            ),
        });
    }
    Ok(seconds)
}

/// Parses the local expiration time, accepting `HH:MM` and `HH:MM:SS`.
fn parse_expiration_time(raw: &str) -> Result<NaiveTime, ChainError> {
    NaiveTime::parse_from_str(raw, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(raw, "%H:%M"))
        .map_err(|_| ChainError::Validation {
            field: "expiration_time".to_string(),
            reason: format!("must be a local time as HH:MM or HH:MM:SS, got {raw:?}"),
        })
}

/// Parses an IANA time-zone name.
fn parse_timezone(raw: &str) -> Result<Tz, ChainError> {
    Tz::from_str(raw).map_err(|_| ChainError::Validation {
        field: "timezone".to_string(),
        reason: format!("must be a known IANA time-zone name, got {raw:?}"),
    })
}

/// Parses the calendar policy version.
fn parse_calendar(raw: Option<&str>) -> Result<CalendarVersion, ChainError> {
    match raw.unwrap_or(SUPPORTED_CALENDAR) {
        SUPPORTED_CALENDAR => Ok(CalendarVersion::WeekdaysV1),
        other => Err(ChainError::Validation {
            field: "calendar".to_string(),
            reason: format!("must be {SUPPORTED_CALENDAR}, got {other:?}"),
        }),
    }
}

/// Normalises an instant to whole-second UTC.
///
/// Truncating the sub-second part is what lets every timestamp in the API and
/// in the exports render as `YYYY-MM-DDTHH:MM:SSZ`, which is in turn what makes
/// a repeated export byte-comparable (ADR 0001 §3.1).
fn to_whole_second_utc(instant: DateTime<Utc>) -> Result<DateTime<Utc>, ChainError> {
    instant
        .with_nanosecond(0)
        .ok_or_else(|| ChainError::Validation {
            field: "start_at".to_string(),
            reason: format!("{instant} cannot be normalised to a whole second"),
        })
}

/// The resolved parameters of a v2 rolling simulation.
///
/// Every field is effective: nothing here is still waiting to be defaulted. The
/// set of fields is exactly the replay input list of ADR 0001 §8, which is what
/// lets a client reproduce a run from the creation response alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "SimulationParametersV2Wire")]
pub struct SimulationParametersV2 {
    /// Ticker symbol of the underlying.
    pub symbol: String,
    /// Number of steps the simulation runs for.
    pub steps: usize,
    /// The resolved simulated start, in whole-second UTC.
    pub effective_start: DateTime<Utc>,
    /// The resolved interval between simulated steps, in seconds.
    pub step_interval_seconds: u64,
    /// Time frame the stochastic model is scaled by.
    pub time_frame: TimeFrame,
    /// The normalised rolling expiration schedule.
    pub schedule: ExpirationSchedule,
    /// The IANA time-zone database release the expirations were resolved
    /// against, e.g. `2025b`. A replay against a different release is still a
    /// replay — it is just one the client can detect (ADR 0001 §8).
    pub tzdb_version: String,
    /// Initial price of the underlying.
    pub initial_price: Positive,
    /// Initial volatility.
    pub volatility: Positive,
    /// Annualised risk-free rate.
    pub risk_free_rate: Decimal,
    /// Annualised dividend yield.
    pub dividend_yield: Positive,
    /// The stochastic model driving the underlying path.
    pub method: SimulationMethod,
    /// Number of strikes per chain.
    pub chain_size: Option<usize>,
    /// Interval between strikes.
    pub strike_interval: Option<Positive>,
    /// Slope of the volatility skew.
    pub skew_slope: Option<Decimal>,
    /// Curvature of the volatility smile.
    pub smile_curve: Option<Decimal>,
    /// Bid-ask spread factor.
    pub spread: Option<Positive>,
    /// The effective RNG seed. Non-optional: a v2 simulation is always
    /// reproducible, so the seed is resolved at conversion and never `None`.
    pub seed: u64,
}

/// The deserialization shape of [`SimulationParametersV2`].
///
/// Exists so that a stored document is validated exactly like a request. The
/// fields are public and the type derives `Deserialize`, so without this a
/// hand-edited or corrupted document in Redis would sail past every check the
/// request path performs: a `step_interval_seconds` of `0` freezes the
/// simulated clock, a `steps` above the cap drives an unbounded factor tape, a
/// sub-second `effective_start` breaks the whole-second rendering that makes
/// exports byte-comparable. Redis is an outer layer, and
/// `rules/global_rules.md` is explicit that domain types must not trust one.
///
/// It also carries `deny_unknown_fields`, which turns the rolling-deploy
/// hazard into a loud one: an old binary reading a document written by a newer
/// one fails instead of silently dropping the new fields and writing the
/// truncated document back.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SimulationParametersV2Wire {
    symbol: String,
    steps: usize,
    effective_start: DateTime<Utc>,
    step_interval_seconds: u64,
    time_frame: TimeFrame,
    schedule: ExpirationSchedule,
    tzdb_version: String,
    initial_price: Positive,
    volatility: Positive,
    risk_free_rate: Decimal,
    dividend_yield: Positive,
    method: SimulationMethod,
    chain_size: Option<usize>,
    strike_interval: Option<Positive>,
    skew_slope: Option<Decimal>,
    smile_curve: Option<Decimal>,
    spread: Option<Positive>,
    seed: u64,
}

impl TryFrom<SimulationParametersV2Wire> for SimulationParametersV2 {
    type Error = ChainError;

    fn try_from(wire: SimulationParametersV2Wire) -> Result<Self, Self::Error> {
        let parameters = Self {
            symbol: wire.symbol,
            steps: wire.steps,
            effective_start: wire.effective_start,
            step_interval_seconds: wire.step_interval_seconds,
            time_frame: wire.time_frame,
            schedule: wire.schedule,
            tzdb_version: wire.tzdb_version,
            initial_price: wire.initial_price,
            volatility: wire.volatility,
            risk_free_rate: wire.risk_free_rate,
            dividend_yield: wire.dividend_yield,
            method: wire.method,
            chain_size: wire.chain_size,
            strike_interval: wire.strike_interval,
            skew_slope: wire.skew_slope,
            smile_curve: wire.smile_curve,
            spread: wire.spread,
            seed: wire.seed,
        };
        parameters.validate()?;
        Ok(parameters)
    }
}

impl SimulationParametersV2 {
    /// Re-checks every invariant the request path establishes.
    ///
    /// Called from the `Deserialize` path, so a stored document is held to the
    /// same standard as a request. Cheap: a handful of comparisons and one
    /// symbol check, run once per load.
    ///
    /// A `tzdb_version` that differs from the running binary's is a **warning**,
    /// not a rejection: the simulation is still coherent, it was simply resolved
    /// against a different IANA release, and refusing to load it would turn a
    /// dependency bump into an outage. Issue #46 decides whether a mid-tape
    /// divergence should be escalated.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming the offending field when
    /// `steps` is outside `1..=MAX_STEPS`, `chain_size` exceeds
    /// `MAX_CHAIN_SIZE`, the symbol violates the identifier format,
    /// `step_interval_seconds` is outside its documented range, or
    /// `effective_start` is not on a whole second.
    pub fn validate(&self) -> Result<(), ChainError> {
        if self.steps < 1 {
            return Err(ChainError::Validation {
                field: "steps".to_string(),
                reason: "must be at least 1".to_string(),
            });
        }
        if self.steps > *MAX_STEPS {
            return Err(ChainError::Validation {
                field: "steps".to_string(),
                reason: format!("must not exceed {}, got {}", *MAX_STEPS, self.steps),
            });
        }
        if let Some(chain_size) = self.chain_size
            && chain_size > *MAX_CHAIN_SIZE
        {
            return Err(ChainError::Validation {
                field: "chain_size".to_string(),
                reason: format!("must not exceed {}, got {chain_size}", *MAX_CHAIN_SIZE),
            });
        }
        symbol_field("symbol", &self.symbol)?;
        validate_step_interval_seconds(self.step_interval_seconds)?;

        // `Positive` admits zero, so the strict-positive constraints the
        // request path enforces have to be rechecked here: a stored price or
        // volatility of zero is a chain of zero-value options, and a stored
        // `strike_interval` of zero collapses every strike onto one.
        for (field, value) in [
            ("initial_price", self.initial_price),
            ("volatility", self.volatility),
        ] {
            reject_zero(field, value)?;
        }
        if let Some(strike_interval) = self.strike_interval {
            reject_zero("strike_interval", strike_interval)?;
        }
        validate_walk_type(&self.method)?;

        if self.effective_start.nanosecond() != 0 {
            return Err(ChainError::Validation {
                field: "effective_start".to_string(),
                reason: format!(
                    "must be on a whole second, got {}",
                    self.effective_start.to_rfc3339()
                ),
            });
        }
        self.schedule.validate()?;

        let running = tzdb_version();
        if self.tzdb_version != running {
            warn!(
                stored = %self.tzdb_version,
                running = %running,
                "simulation was resolved against a different IANA tzdb release"
            );
        }

        Ok(())
    }

    /// The simulated instant at `cursor`.
    ///
    /// `effective_start + cursor × step_interval`, with checked arithmetic
    /// throughout. Never reads the wall clock, so the same parameters derive
    /// the same instant on every call, in every process.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming `steps` when the product or
    /// the sum leaves the representable range — an overflow is a rejected
    /// request, never a wrapped timestamp.
    pub fn simulated_at(&self, cursor: usize) -> Result<DateTime<Utc>, ChainError> {
        let overflow = || ChainError::Validation {
            field: "steps".to_string(),
            reason: format!(
                "simulated time overflows at cursor {cursor} with a {} s interval",
                self.step_interval_seconds
            ),
        };

        let cursor = i64::try_from(cursor).map_err(|_| overflow())?;
        let interval = i64::try_from(self.step_interval_seconds).map_err(|_| overflow())?;
        let offset = cursor.checked_mul(interval).ok_or_else(overflow)?;
        let delta = TimeDelta::try_seconds(offset).ok_or_else(overflow)?;

        self.effective_start
            .checked_add_signed(delta)
            .ok_or_else(overflow)
    }

    /// The simulated instant one step past the last one served, i.e. the end of
    /// the simulated horizon.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] when the horizon overflows.
    pub fn simulated_end(&self) -> Result<DateTime<Utc>, ChainError> {
        self.simulated_at(self.steps)
    }
}

impl fmt::Display for SimulationParametersV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let json = serde_json::to_string(self).map_err(|_| fmt::Error)?;
        write!(f, "{json}")
    }
}

impl TryFrom<CreateSimulationRequest> for SimulationParametersV2 {
    type Error = ChainError;

    /// Validates a client-supplied [`CreateSimulationRequest`] and resolves it
    /// into effective parameters.
    ///
    /// This is the single place where the v2 REST `f64` / string boundary
    /// becomes `Positive` / `Decimal` / `Tz` / `NaiveTime`, mirroring what
    /// `TryFrom<CreateSessionRequest>` does for v1. Three values are *resolved*
    /// here rather than merely converted, and all three are surfaced back to the
    /// client so the tape can be replayed:
    ///
    /// - the **seed**, generated when absent, as in v1;
    /// - the **effective start**, generated when absent and normalised to
    ///   whole-second UTC — the only wall-clock read in a v2 simulation's whole
    ///   life;
    /// - the **step interval**, derived from `time_frame` when absent.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming the first field that fails.
    fn try_from(request: CreateSimulationRequest) -> Result<Self, Self::Error> {
        if request.steps < 1 {
            return Err(ChainError::Validation {
                field: "steps".to_string(),
                reason: "must be at least 1".to_string(),
            });
        }
        if request.steps > *MAX_STEPS {
            return Err(ChainError::Validation {
                field: "steps".to_string(),
                reason: format!("must not exceed {}, got {}", *MAX_STEPS, request.steps),
            });
        }
        if let Some(chain_size) = request.chain_size
            && chain_size > *MAX_CHAIN_SIZE
        {
            return Err(ChainError::Validation {
                field: "chain_size".to_string(),
                reason: format!("must not exceed {}, got {}", *MAX_CHAIN_SIZE, chain_size),
            });
        }
        symbol_field("symbol", &request.symbol)?;

        let time_frame = time_frame_field("time_frame", request.time_frame)?;
        let step_interval_seconds = match request.step_interval_seconds {
            Some(seconds) => validate_step_interval_seconds(seconds)?,
            None => derive_step_interval_seconds(time_frame)?,
        };

        // The only wall-clock read that reaches simulation OUTPUT. Everything
        // downstream — every simulated_at, expires_at and days_to_expiration —
        // is a function of this value and the cursor. (`created_at` and
        // `updated_at` also read the clock, but they are operational metadata
        // and never enter the tape.)
        let effective_start = to_whole_second_utc(request.start_at.unwrap_or_else(Utc::now))?;

        let schedule = ExpirationSchedule::new(
            parse_calendar(request.calendar.as_deref())?,
            parse_timezone(&request.timezone)?,
            parse_expiration_time(&request.expiration_time)?,
            request.schedules,
        )?;

        Ok(Self {
            symbol: request.symbol,
            steps: request.steps,
            effective_start,
            step_interval_seconds,
            time_frame,
            schedule,
            tzdb_version: tzdb_version().to_string(),
            initial_price: strictly_positive_field("initial_price", request.initial_price)?,
            volatility: strictly_positive_field("volatility", request.volatility)?,
            risk_free_rate: decimal_field("risk_free_rate", request.risk_free_rate)?,
            dividend_yield: positive_field("dividend_yield", request.dividend_yield)?,
            method: request.method.try_into()?,
            chain_size: request.chain_size,
            strike_interval: request
                .strike_interval
                .map(|value| strictly_positive_field("strike_interval", value))
                .transpose()?,
            skew_slope: request
                .skew_slope
                .map(|value| decimal_field("skew_slope", value))
                .transpose()?,
            smile_curve: request
                .smile_curve
                .map(|value| decimal_field("smile_curve", value))
                .transpose()?,
            spread: request
                .spread
                .map(|value| positive_field("spread", value))
                .transpose()?,
            seed: request.seed.unwrap_or_else(|| rand::rng().random()),
        })
    }
}

/// A v2 rolling simulation session.
///
/// Reuses the v1 [`SessionState`] machine, but only three of its states are
/// reachable: `Initialized → InProgress → Completed`. A v2 simulation is
/// immutable after creation, so `Modified` and `Reinitialized` — the PATCH and
/// PUT branches — cannot occur.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "SessionV2Wire")]
pub struct SessionV2 {
    /// Unique identifier.
    pub id: Uuid,
    /// The stored-schema version this document was written under.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// When the simulation was created, in real time. Unrelated to the
    /// simulated clock, which may span years.
    pub created_at: SystemTime,
    /// When the simulation was last written, in real time.
    pub updated_at: SystemTime,
    /// The resolved parameters. Immutable for the simulation's lifetime.
    pub parameters: SimulationParametersV2,
    /// The 0-based index of the next snapshot to serve.
    pub current_step: usize,
    /// The total number of snapshots the simulation serves.
    pub total_steps: usize,
    /// The lifecycle state.
    pub state: SessionState,
    /// Optimistic-concurrency revision, bumped immediately before every
    /// compare-and-swap save, exactly as in v1.
    pub version: u64,
}

/// The deserialization shape of [`SessionV2`], validated on the way in.
///
/// Mirrors the parameters' wire type for the same reason: a stored document is
/// an outer-layer input. It additionally rejects the two states a v2 simulation
/// can never legitimately be in, a cursor past its own horizon, and — the one
/// that matters most operationally — a `schema_version` from the future, which
/// during a rolling deploy would otherwise let an old replica read a newer
/// document, drop what it does not understand, and write the truncated version
/// back with an intact revision so the compare-and-swap succeeds.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionV2Wire {
    id: Uuid,
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    created_at: SystemTime,
    updated_at: SystemTime,
    parameters: SimulationParametersV2,
    current_step: usize,
    total_steps: usize,
    state: SessionState,
    version: u64,
}

impl TryFrom<SessionV2Wire> for SessionV2 {
    type Error = ChainError;

    fn try_from(wire: SessionV2Wire) -> Result<Self, Self::Error> {
        let simulation = Self {
            id: wire.id,
            schema_version: wire.schema_version,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
            parameters: wire.parameters,
            current_step: wire.current_step,
            total_steps: wire.total_steps,
            state: wire.state,
            version: wire.version,
        };
        simulation.validate()?;
        Ok(simulation)
    }
}

/// The schema version assumed for a stored document written before the field
/// existed. There is no such document today — v2 has shipped with the field
/// from its first release — but defaulting keeps a future reader honest.
fn default_schema_version() -> u32 {
    SESSION_V2_SCHEMA_VERSION
}

impl SessionV2 {
    /// Creates a simulation from resolved parameters.
    #[must_use]
    pub fn new(parameters: SimulationParametersV2, uuid_generator: &UuidGenerator) -> Self {
        let now = SystemTime::now();
        Self {
            id: uuid_generator.next(),
            schema_version: SESSION_V2_SCHEMA_VERSION,
            created_at: now,
            updated_at: now,
            current_step: 0,
            total_steps: parameters.steps,
            parameters,
            state: SessionState::Initialized,
            version: 0,
        }
    }

    /// Re-checks every invariant a freshly-created simulation satisfies.
    ///
    /// Called from the `Deserialize` path, so a stored document cannot present
    /// a state the lifecycle forbids.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] when the document carries a
    /// `schema_version` this binary does not understand, a `total_steps` that
    /// disagrees with its parameters, a cursor past its own horizon, or a state
    /// unreachable for a v2 simulation (`Modified` and `Reinitialized` are the
    /// PATCH and PUT branches, and a v2 simulation is immutable). Also
    /// propagates the parameters' own validation.
    pub fn validate(&self) -> Result<(), ChainError> {
        if self.schema_version > SESSION_V2_SCHEMA_VERSION {
            return Err(ChainError::Validation {
                field: "schema_version".to_string(),
                reason: format!(
                    "document was written under schema {} but this binary understands at most {SESSION_V2_SCHEMA_VERSION}",
                    self.schema_version
                ),
            });
        }
        self.parameters.validate()?;
        if self.total_steps != self.parameters.steps {
            return Err(ChainError::Validation {
                field: "total_steps".to_string(),
                reason: format!(
                    "must equal the parameters' steps ({}), got {}",
                    self.parameters.steps, self.total_steps
                ),
            });
        }
        if self.current_step > self.total_steps {
            return Err(ChainError::Validation {
                field: "current_step".to_string(),
                reason: format!(
                    "must not exceed total_steps ({}), got {}",
                    self.total_steps, self.current_step
                ),
            });
        }
        self.validate_state()
    }

    /// Checks the lifecycle state against the cursor.
    ///
    /// A v2 simulation walks `Initialized` at cursor 0, `InProgress` while it
    /// has snapshots left, and `Completed` once the cursor reaches the horizon.
    /// Every other combination — `Completed` at step 0, `Initialized` halfway
    /// through — is a document no code path can write, so accepting one would
    /// let a corrupted or hand-edited simulation into the manager and serve
    /// snapshots from a state the rest of the code assumes away.
    fn validate_state(&self) -> Result<(), ChainError> {
        let unreachable = |reason: String| ChainError::Validation {
            field: "state".to_string(),
            reason,
        };

        match self.state {
            SessionState::Modified | SessionState::Reinitialized | SessionState::Error => {
                Err(unreachable(format!(
                    "{} is unreachable for a v2 simulation, which is immutable after creation",
                    self.state
                )))
            }
            SessionState::Initialized if self.current_step != 0 => Err(unreachable(format!(
                "{} requires a cursor of 0, got {}",
                self.state, self.current_step
            ))),
            SessionState::Completed if self.current_step != self.total_steps => {
                Err(unreachable(format!(
                    "{} requires the cursor to have reached total_steps ({}), got {}",
                    self.state, self.total_steps, self.current_step
                )))
            }
            SessionState::InProgress
                if self.current_step == 0 || self.current_step >= self.total_steps =>
            {
                Err(unreachable(format!(
                    "{} requires a cursor between 1 and total_steps ({}) exclusive, got {}",
                    self.state, self.total_steps, self.current_step
                )))
            }
            _ => Ok(()),
        }
    }

    /// The simulated instant of the snapshot the cursor currently points at.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] when the simulated clock overflows.
    pub fn simulated_at(&self) -> Result<DateTime<Utc>, ChainError> {
        self.parameters.simulated_at(self.current_step)
    }

    /// Whether the cursor has served every snapshot.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.current_step >= self.total_steps
    }

    /// Bumps the optimistic-concurrency revision, returning the value the
    /// caller must pass as `expected_version` to the compare-and-swap save.
    ///
    /// Mirrors the v1 helper: the caller reads a simulation, captures its
    /// `version`, mutates a clone, bumps, and saves with the captured value.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Internal`] when the revision counter would
    /// overflow, which is unreachable in practice but must not wrap.
    pub fn bump_version(&mut self) -> Result<u64, ChainError> {
        let expected = self.version;
        self.version = self.version.checked_add(1).ok_or_else(|| {
            ChainError::Internal(format!(
                "version counter overflowed for simulation {}",
                self.id
            ))
        })?;
        self.updated_at = SystemTime::now();
        Ok(expected)
    }
}

impl fmt::Display for SessionV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let json = serde_json::to_string(self).map_err(|_| fmt::Error)?;
        write!(f, "{json}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
    use crate::domain::expiry::{ExpiryRule, ExpiryRuleKind};
    use chrono::{TimeZone, Weekday};
    use positive::pos_or_panic;

    /// The reference configuration from ADR 0001 §14.1, as a request.
    fn reference_request() -> CreateSimulationRequest {
        CreateSimulationRequest {
            symbol: "SPX".to_string(),
            steps: 500,
            start_at: Some(instant(2026, 1, 5, 14, 30)),
            step_interval_seconds: Some(86_400),
            timezone: "America/New_York".to_string(),
            calendar: Some("weekdays_v1".to_string()),
            expiration_time: "17:00".to_string(),
            schedules: vec![
                rule("zero_dte", ExpiryRuleKind::Daily, 1),
                rule(
                    "weeklies",
                    ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Wed, Weekday::Fri]),
                    3,
                ),
                rule(
                    "monthlies",
                    ExpiryRuleKind::Monthly {
                        weekday: Weekday::Fri,
                    },
                    12,
                ),
            ],
            initial_price: 5000.0,
            volatility: 0.18,
            risk_free_rate: 0.04,
            dividend_yield: 0.012,
            method: ApiWalkType::GeometricBrownian {
                dt: 0.004,
                drift: 0.05,
                volatility: 0.18,
            },
            time_frame: ApiTimeFrame::Day,
            chain_size: Some(15),
            strike_interval: Some(25.0),
            skew_slope: Some(-0.2),
            smile_curve: Some(0.4),
            spread: Some(0.02),
            seed: Some(42),
        }
    }

    fn rule(id: &str, kind: ExpiryRuleKind, count: usize) -> ExpiryRule {
        match ExpiryRule::new(id, kind, count) {
            Ok(rule) => rule,
            Err(error) => panic!("test rule must be valid: {error}"),
        }
    }

    fn instant(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        match Utc
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
        {
            Some(instant) => instant,
            None => panic!("test instant must be valid"),
        }
    }

    fn parameters(request: CreateSimulationRequest) -> SimulationParametersV2 {
        match SimulationParametersV2::try_from(request) {
            Ok(parameters) => parameters,
            Err(error) => panic!("the request must convert: {error}"),
        }
    }

    fn generator() -> UuidGenerator {
        match Uuid::parse_str(crate::session::manager::DEFAULT_NAMESPACE) {
            Ok(namespace) => UuidGenerator::new(namespace),
            Err(error) => panic!("the default namespace must parse: {error}"),
        }
    }

    // ---- resolution ------------------------------------------------------

    /// The reference request converts, and every effective value is resolved.
    #[test]
    fn test_reference_request_converts_to_effective_parameters() {
        let parameters = parameters(reference_request());

        assert_eq!(parameters.symbol, "SPX");
        assert_eq!(parameters.steps, 500);
        assert_eq!(parameters.seed, 42);
        assert_eq!(parameters.effective_start, instant(2026, 1, 5, 14, 30));
        assert_eq!(parameters.step_interval_seconds, 86_400);
        assert_eq!(parameters.time_frame, TimeFrame::Day);
        assert_eq!(parameters.schedule.rules().len(), 3);
        assert_eq!(parameters.initial_price, pos_or_panic!(5000.0));
        assert!(!parameters.tzdb_version.is_empty());
    }

    /// An omitted seed is generated and surfaced, exactly as in v1.
    #[test]
    fn test_omitted_seed_is_generated_and_surfaced() {
        let mut request = reference_request();
        request.seed = None;

        // The generated seed is random, so the observable guarantee is that a
        // seed exists and is echoed — not that it has a particular value.
        let first = parameters(request.clone());
        let second = parameters(request);

        assert_ne!(
            first.seed, second.seed,
            "two unseeded requests must not share a seed"
        );
    }

    /// An omitted start is generated once, normalised to whole-second UTC, and
    /// then never re-derived.
    #[test]
    fn test_omitted_start_is_generated_once_and_normalised() {
        let mut request = reference_request();
        request.start_at = None;

        let parameters = parameters(request);

        assert_eq!(parameters.effective_start.nanosecond(), 0);
        // The start is stored, not recomputed: cursor 0 resolves back to it.
        match parameters.simulated_at(0) {
            Ok(at) => assert_eq!(at, parameters.effective_start),
            Err(error) => panic!("cursor 0 must resolve: {error}"),
        }
    }

    /// A supplied start with sub-second precision is truncated, so every
    /// timestamp renders without a fractional part.
    #[test]
    fn test_supplied_start_is_truncated_to_whole_seconds() {
        let mut request = reference_request();
        request.start_at = Some(instant(2026, 1, 5, 14, 30) + TimeDelta::milliseconds(750));

        let parameters = parameters(request);

        assert_eq!(parameters.effective_start, instant(2026, 1, 5, 14, 30));
    }

    /// An omitted interval is derived from the time frame.
    #[test]
    fn test_omitted_step_interval_is_derived_from_the_time_frame() {
        let mut request = reference_request();
        request.step_interval_seconds = None;
        request.time_frame = ApiTimeFrame::Hour;

        let parameters = parameters(request);

        assert_eq!(parameters.step_interval_seconds, 3_600);
    }

    /// A time frame shorter than a second cannot derive an interval, and says
    /// so rather than clamping to one second.
    #[test]
    fn test_sub_second_time_frame_cannot_derive_an_interval() {
        let mut request = reference_request();
        request.step_interval_seconds = None;
        request.time_frame = ApiTimeFrame::Microsecond;

        match SimulationParametersV2::try_from(request) {
            Err(ChainError::Validation { field, .. }) => {
                assert_eq!(field, "step_interval_seconds");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A custom time frame coarser than a year cannot derive an interval
    /// either.
    #[test]
    fn test_custom_time_frame_beyond_a_year_cannot_derive_an_interval() {
        let mut request = reference_request();
        request.step_interval_seconds = None;
        // Half a period per year is a two-year step.
        request.time_frame = ApiTimeFrame::Custom(0.5);

        match SimulationParametersV2::try_from(request) {
            Err(ChainError::Validation { field, .. }) => {
                assert_eq!(field, "step_interval_seconds");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A custom time frame within range derives cleanly.
    #[test]
    fn test_custom_time_frame_within_range_derives_an_interval() {
        let mut request = reference_request();
        request.step_interval_seconds = None;
        request.time_frame = ApiTimeFrame::Custom(365.0);

        let parameters = parameters(request);

        assert_eq!(parameters.step_interval_seconds, 86_400);
    }

    /// An explicit interval outside the accepted range is rejected.
    #[test]
    fn test_out_of_range_step_interval_is_rejected() {
        for seconds in [0, MAX_STEP_INTERVAL_SECONDS + 1] {
            let mut request = reference_request();
            request.step_interval_seconds = Some(seconds);

            match SimulationParametersV2::try_from(request) {
                Err(ChainError::Validation { field, .. }) => {
                    assert_eq!(field, "step_interval_seconds");
                }
                other => panic!("expected a validation error for {seconds}, got {other:?}"),
            }
        }
    }

    // ---- the simulated clock --------------------------------------------

    /// `simulated_at` is `effective_start + cursor × interval`, and never reads
    /// the wall clock.
    #[test]
    fn test_simulated_at_is_start_plus_cursor_times_interval() {
        let parameters = parameters(reference_request());

        match (parameters.simulated_at(0), parameters.simulated_at(3)) {
            (Ok(first), Ok(fourth)) => {
                assert_eq!(first, instant(2026, 1, 5, 14, 30));
                assert_eq!(fourth, instant(2026, 1, 8, 14, 30));
            }
            (first, fourth) => panic!("both cursors must resolve: {first:?} {fourth:?}"),
        }
    }

    /// The same parameters derive the same instant for every cursor, call after
    /// call.
    #[test]
    fn test_simulated_at_is_stable_across_calls() {
        let parameters = parameters(reference_request());

        for cursor in [0, 1, 17, 499] {
            match (
                parameters.simulated_at(cursor),
                parameters.simulated_at(cursor),
            ) {
                (Ok(first), Ok(second)) => assert_eq!(first, second),
                (first, second) => panic!("cursor {cursor} must resolve: {first:?} {second:?}"),
            }
        }
    }

    /// The horizon is one interval past the last served snapshot.
    #[test]
    fn test_simulated_end_is_one_interval_past_the_last_step() {
        let mut request = reference_request();
        request.steps = 3;
        let parameters = parameters(request);

        match parameters.simulated_end() {
            Ok(end) => assert_eq!(end, instant(2026, 1, 8, 14, 30)),
            Err(error) => panic!("the horizon must resolve: {error}"),
        }
    }

    /// A cursor that would overflow the simulated clock is a typed error, never
    /// a wrapped timestamp.
    #[test]
    fn test_simulated_at_overflow_is_a_typed_error() {
        let mut parameters = parameters(reference_request());
        parameters.step_interval_seconds = MAX_STEP_INTERVAL_SECONDS;

        match parameters.simulated_at(usize::MAX) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "steps");
                assert!(reason.contains("overflow"));
            }
            other => panic!("expected an overflow error, got {other:?}"),
        }
    }

    // ---- validation ------------------------------------------------------

    /// Each invalid field is rejected by name, and no path panics.
    #[test]
    fn test_invalid_fields_are_rejected_by_name() {
        /// One invalid-field case: the field the error must name, and the
        /// mutation that makes the request invalid.
        type Case = (&'static str, Box<dyn Fn(&mut CreateSimulationRequest)>);

        let cases: Vec<Case> = vec![
            (
                "steps",
                Box::new(|r: &mut CreateSimulationRequest| r.steps = 0),
            ),
            (
                "symbol",
                Box::new(|r: &mut CreateSimulationRequest| r.symbol = "bad symbol!".to_string()),
            ),
            (
                "timezone",
                Box::new(|r: &mut CreateSimulationRequest| r.timezone = "Mars/Olympus".to_string()),
            ),
            (
                "expiration_time",
                Box::new(|r: &mut CreateSimulationRequest| r.expiration_time = "25:00".to_string()),
            ),
            (
                "calendar",
                Box::new(|r: &mut CreateSimulationRequest| {
                    r.calendar = Some("weekdays_v9".to_string())
                }),
            ),
            (
                "initial_price",
                Box::new(|r: &mut CreateSimulationRequest| r.initial_price = 0.0),
            ),
            (
                "volatility",
                Box::new(|r: &mut CreateSimulationRequest| r.volatility = f64::NAN),
            ),
            (
                "risk_free_rate",
                Box::new(|r: &mut CreateSimulationRequest| r.risk_free_rate = f64::INFINITY),
            ),
            (
                "dividend_yield",
                Box::new(|r: &mut CreateSimulationRequest| r.dividend_yield = -1.0),
            ),
            (
                "strike_interval",
                Box::new(|r: &mut CreateSimulationRequest| r.strike_interval = Some(0.0)),
            ),
            (
                "chain_size",
                Box::new(|r: &mut CreateSimulationRequest| r.chain_size = Some(usize::MAX)),
            ),
        ];

        for (field, mutate) in cases {
            let mut request = reference_request();
            mutate(&mut request);

            match SimulationParametersV2::try_from(request) {
                Err(ChainError::Validation { field: named, .. }) => {
                    assert_eq!(named, field, "wrong field named for {field}");
                }
                other => panic!("expected a validation error for {field}, got {other:?}"),
            }
        }
    }

    /// An empty schedule is rejected through the same domain validation the
    /// planner uses.
    #[test]
    fn test_empty_schedule_is_rejected() {
        let mut request = reference_request();
        request.schedules = Vec::new();

        match SimulationParametersV2::try_from(request) {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "schedules"),
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// `HH:MM:SS` is accepted as well as `HH:MM`.
    #[test]
    fn test_expiration_time_accepts_both_precisions() {
        for raw in ["17:00", "17:00:00"] {
            let mut request = reference_request();
            request.expiration_time = raw.to_string();

            let parameters = parameters(request);
            match NaiveTime::from_hms_opt(17, 0, 0) {
                Some(expected) => {
                    assert_eq!(parameters.schedule.expiration_time(), expected)
                }
                None => panic!("17:00:00 must be a valid time"),
            }
        }
    }

    /// An omitted calendar defaults to the only supported policy.
    #[test]
    fn test_omitted_calendar_defaults_to_weekdays_v1() {
        let mut request = reference_request();
        request.calendar = None;

        let parameters = parameters(request);

        assert_eq!(parameters.schedule.calendar(), CalendarVersion::WeekdaysV1);
    }

    // ---- persistence shape ----------------------------------------------

    /// The parameters round-trip through serde, which is what the stores rely
    /// on.
    #[test]
    fn test_parameters_round_trip_through_serde() {
        let parameters = parameters(reference_request());

        let json = match serde_json::to_string(&parameters) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };
        match serde_json::from_str::<SimulationParametersV2>(&json) {
            Ok(round_tripped) => assert_eq!(round_tripped, parameters),
            Err(error) => panic!("must deserialize: {error}"),
        }
    }

    /// A stored simulation round-trips, keeping its schema version, cursor and
    /// revision.
    #[test]
    fn test_simulation_round_trips_through_serde() {
        let simulation = SessionV2::new(parameters(reference_request()), &generator());

        let json = match serde_json::to_string(&simulation) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };
        match serde_json::from_str::<SessionV2>(&json) {
            Ok(round_tripped) => {
                assert_eq!(round_tripped, simulation);
                assert_eq!(round_tripped.schema_version, SESSION_V2_SCHEMA_VERSION);
            }
            Err(error) => panic!("must deserialize: {error}"),
        }
    }

    /// The stored document carries an explicit schema version, so a future
    /// migration can tell documents apart without inferring from their shape.
    #[test]
    fn test_stored_document_carries_an_explicit_schema_version() {
        let simulation = SessionV2::new(parameters(reference_request()), &generator());

        let value = match serde_json::to_value(&simulation) {
            Ok(value) => value,
            Err(error) => panic!("must serialize: {error}"),
        };
        assert_eq!(
            value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64),
            Some(u64::from(SESSION_V2_SCHEMA_VERSION))
        );
    }

    /// The stored schedule keeps its documented wire shape.
    ///
    /// This is the *storage* form: the schedule nests under `schedule` with its
    /// rules under `rules`. ADR 0001 §14.2 shows the *response* form, which is
    /// flat inside `parameters` with the array named `schedules`; #47 owns that
    /// mapping.
    #[test]
    fn test_stored_schedule_keeps_the_documented_shape() {
        let parameters = parameters(reference_request());

        let value = match serde_json::to_value(&parameters) {
            Ok(value) => value,
            Err(error) => panic!("must serialize: {error}"),
        };
        let schedule = match value.get("schedule") {
            Some(schedule) => schedule,
            None => panic!("the parameters must carry a schedule"),
        };
        assert_eq!(
            schedule.get("timezone").and_then(serde_json::Value::as_str),
            Some("America/New_York")
        );
        assert_eq!(
            schedule.get("calendar").and_then(serde_json::Value::as_str),
            Some("weekdays_v1")
        );
        assert_eq!(
            schedule
                .get("expiration_time")
                .and_then(serde_json::Value::as_str),
            Some("17:00:00")
        );
    }

    // ---- stored input is not trusted -------------------------------------

    /// Deserialization runs the same validation as the request path.
    ///
    /// Without it a hand-edited or corrupted document in Redis sails past every
    /// check: `rules/global_rules.md` is explicit that a domain type must not
    /// trust an outer layer, and Redis is one.
    #[test]
    fn test_stored_parameters_are_validated_on_load() {
        let parameters = parameters(reference_request());
        let json = match serde_json::to_string(&parameters) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };

        // Each tamper is a field the request path checks and a stored document
        // could otherwise smuggle past.
        let tampers = [
            // A zero interval freezes the simulated clock: every snapshot would
            // carry the same instant, the cutoff would never advance, and the
            // rolling inventory would never roll.
            (
                r#""step_interval_seconds":86400"#,
                r#""step_interval_seconds":0"#,
                "step_interval_seconds",
            ),
            // A steps count above the cap drives an unbounded factor tape.
            (r#""steps":500"#, r#""steps":100000000"#, "steps"),
            // A sub-second start breaks the whole-second rendering that makes
            // exports byte-comparable.
            (
                r#""effective_start":"2026-01-05T14:30:00Z""#,
                r#""effective_start":"2026-01-05T14:30:00.5Z""#,
                "effective_start",
            ),
            // A symbol carrying the CSV separators would corrupt the export
            // column the rule-id charset was narrowed to protect.
            (r#""symbol":"SPX""#, r#""symbol":"SPX,\"x\"|y""#, "symbol"),
            // A chain size above the cap drives an unbounded strike ladder.
            (r#""chain_size":15"#, r#""chain_size":100000"#, "chain_size"),
            // `Positive` admits zero, so these three survive the type and have
            // to be rejected by the validator: a zero price or volatility is a
            // chain of worthless options, and a zero interval collapses every
            // strike onto one.
            (
                r#""initial_price":5000"#,
                r#""initial_price":0"#,
                "initial_price",
            ),
            (
                r#""tzdb_version":"2025b","initial_price":5000,"volatility":0.18"#,
                r#""tzdb_version":"2025b","initial_price":5000,"volatility":0"#,
                "volatility",
            ),
            (
                r#""strike_interval":25"#,
                r#""strike_interval":0"#,
                "strike_interval",
            ),
            // The walk's own invariants are not re-derived here: the stored
            // method round-trips through the same mirror the request path
            // validates with, so a `dt` of zero is caught by that check.
            (r#""dt":0.004"#, r#""dt":0.0"#, "dt"),
        ];

        for (from, to, field) in tampers {
            let tampered = json.replace(from, to);
            assert_ne!(tampered, json, "the tamper for {field} must have applied");

            let error = match serde_json::from_str::<SimulationParametersV2>(&tampered) {
                Ok(_) => panic!("a tampered {field} must be rejected on load"),
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains(field),
                "the error must name {field}, got {error}"
            );
        }
    }

    /// An unknown field in a stored document is an error, not a silent drop.
    ///
    /// During a rolling deploy an old replica would otherwise read a newer
    /// document, discard what it does not understand, and write the truncated
    /// version back with an intact revision — so the compare-and-swap succeeds
    /// and the data is simply gone.
    #[test]
    fn test_stored_parameters_reject_an_unknown_field() {
        let parameters = parameters(reference_request());
        let json = match serde_json::to_string(&parameters) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };
        let tampered = json.replace(r#""symbol":"SPX""#, r#""symbol":"SPX","a_future_field":1"#);

        assert!(serde_json::from_str::<SimulationParametersV2>(&tampered).is_err());
    }

    /// A stored simulation from a newer schema is refused rather than silently
    /// downgraded.
    #[test]
    fn test_stored_simulation_rejects_a_future_schema_version() {
        let simulation = SessionV2::new(parameters(reference_request()), &generator());
        let json = match serde_json::to_string(&simulation) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };
        let tampered = json.replace(
            &format!(r#""schema_version":{SESSION_V2_SCHEMA_VERSION}"#),
            r#""schema_version":99"#,
        );

        let error = match serde_json::from_str::<SessionV2>(&tampered) {
            Ok(_) => panic!("a future schema version must be rejected"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("schema_version"), "got {error}");
    }

    /// A stored simulation in a state the v2 lifecycle cannot reach is refused.
    #[test]
    fn test_stored_simulation_rejects_an_unreachable_state() {
        let simulation = SessionV2::new(parameters(reference_request()), &generator());
        let json = match serde_json::to_string(&simulation) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };

        for state in ["Modified", "Reinitialized", "Error"] {
            let tampered =
                json.replace(r#""state":"Initialized""#, &format!(r#""state":"{state}""#));
            assert_ne!(tampered, json, "the tamper for {state} must have applied");

            let error = match serde_json::from_str::<SessionV2>(&tampered) {
                Ok(_) => panic!("{state} must be rejected for a v2 simulation"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains("state"), "got {error}");
        }
    }

    /// A state that contradicts the cursor is refused.
    ///
    /// `Completed` at step 0 and `Initialized` halfway through are documents no
    /// code path writes, so accepting one would let a corrupted simulation
    /// serve snapshots from a state the rest of the code assumes away.
    #[test]
    fn test_stored_simulation_rejects_a_state_the_cursor_contradicts() {
        let mut request = reference_request();
        request.steps = 4;
        let simulation = SessionV2::new(parameters(request), &generator());
        let json = match serde_json::to_string(&simulation) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };

        let contradictions = [
            (
                r#""current_step":0,"total_steps":4,"state":"Completed""#,
                "Completed at step 0",
            ),
            (
                r#""current_step":2,"total_steps":4,"state":"Initialized""#,
                "Initialized mid-run",
            ),
            (
                r#""current_step":0,"total_steps":4,"state":"InProgress""#,
                "InProgress at step 0",
            ),
            (
                r#""current_step":4,"total_steps":4,"state":"InProgress""#,
                "InProgress at the horizon",
            ),
        ];

        for (replacement, what) in contradictions {
            let tampered = json.replace(
                r#""current_step":0,"total_steps":4,"state":"Initialized""#,
                replacement,
            );
            assert_ne!(tampered, json, "the tamper for {what} must have applied");

            let error = match serde_json::from_str::<SessionV2>(&tampered) {
                Ok(_) => panic!("{what} must be rejected"),
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains("state"),
                "{what} must name state, got {error}"
            );
        }
    }

    /// A cursor past its own horizon is refused, so no caller has to defend
    /// against one.
    #[test]
    fn test_stored_simulation_rejects_a_cursor_past_the_horizon() {
        let mut request = reference_request();
        request.steps = 2;
        let simulation = SessionV2::new(parameters(request), &generator());
        let json = match serde_json::to_string(&simulation) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };
        let tampered = json.replace(r#""current_step":0"#, r#""current_step":9999"#);

        let error = match serde_json::from_str::<SessionV2>(&tampered) {
            Ok(_) => panic!("a cursor past the horizon must be rejected"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("current_step"), "got {error}");
    }

    /// A `total_steps` that disagrees with the parameters is refused.
    #[test]
    fn test_stored_simulation_rejects_a_mismatched_total_steps() {
        let simulation = SessionV2::new(parameters(reference_request()), &generator());
        let json = match serde_json::to_string(&simulation) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };
        let tampered = json.replace(r#""total_steps":500"#, r#""total_steps":7"#);

        let error = match serde_json::from_str::<SessionV2>(&tampered) {
            Ok(_) => panic!("a mismatched total_steps must be rejected"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("total_steps"), "got {error}");
    }

    /// A valid stored document still loads, so the validation does not reject
    /// what it should accept.
    #[test]
    fn test_a_valid_stored_simulation_still_loads() {
        let simulation = SessionV2::new(parameters(reference_request()), &generator());
        let json = match serde_json::to_string(&simulation) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };

        match serde_json::from_str::<SessionV2>(&json) {
            Ok(loaded) => assert_eq!(loaded, simulation),
            Err(error) => panic!("a valid document must load: {error}"),
        }
    }

    // ---- lifecycle -------------------------------------------------------

    /// A new simulation starts at cursor zero, revision zero, Initialized.
    #[test]
    fn test_new_simulation_starts_initialized_at_cursor_zero() {
        let simulation = SessionV2::new(parameters(reference_request()), &generator());

        assert_eq!(simulation.current_step, 0);
        assert_eq!(simulation.total_steps, 500);
        assert_eq!(simulation.version, 0);
        assert_eq!(simulation.state, SessionState::Initialized);
        assert!(!simulation.is_complete());
    }

    /// Bumping the revision returns the value a compare-and-swap must expect.
    #[test]
    fn test_bump_version_returns_the_expected_revision() {
        let mut simulation = SessionV2::new(parameters(reference_request()), &generator());

        match simulation.bump_version() {
            Ok(expected) => {
                assert_eq!(expected, 0);
                assert_eq!(simulation.version, 1);
            }
            Err(error) => panic!("must bump: {error}"),
        }
    }

    /// A revision counter at its maximum is an error rather than a wrap that
    /// would let a stale writer pass the compare-and-swap.
    #[test]
    fn test_bump_version_overflow_is_a_typed_error() {
        let mut simulation = SessionV2::new(parameters(reference_request()), &generator());
        simulation.version = u64::MAX;

        match simulation.bump_version() {
            Err(ChainError::Internal(reason)) => assert!(reason.contains("overflow")),
            other => panic!("expected an internal error, got {other:?}"),
        }
    }

    /// Completion is a cursor comparison, not a stored flag.
    #[test]
    fn test_is_complete_tracks_the_cursor() {
        let mut request = reference_request();
        request.steps = 2;
        let mut simulation = SessionV2::new(parameters(request), &generator());

        assert!(!simulation.is_complete());
        simulation.current_step = 2;
        assert!(simulation.is_complete());
    }

    /// The simulation's own `simulated_at` follows its cursor.
    #[test]
    fn test_simulation_simulated_at_follows_the_cursor() {
        let mut simulation = SessionV2::new(parameters(reference_request()), &generator());
        simulation.current_step = 2;

        match simulation.simulated_at() {
            Ok(at) => assert_eq!(at, instant(2026, 1, 7, 14, 30)),
            Err(error) => panic!("must resolve: {error}"),
        }
    }
}
