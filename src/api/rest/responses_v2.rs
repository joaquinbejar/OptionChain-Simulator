//! Response DTOs for the v2 rolling-simulation API.
//!
//! Separate from [`crate::api::rest::responses`] because `/api/v1/chain` is
//! frozen (ADR 0001 §12.1): v1 serves one chain and stamps it with the wall
//! clock, v2 serves many chains stamped with the simulated one, and neither
//! shape can absorb the other without breaking a published contract.
//!
//! As in v1 the wire speaks `f64`, and the conversion from `Positive` /
//! `Decimal` happens here and nowhere else.
//!
//! # What is deliberately not on the wire
//!
//! Upstream's `OptionChain` carries a `YYYY-MM-DD` string it stamps from the
//! **host** clock, not the simulated one, and exposes no hook to change that
//! (see [`crate::domain::series`]). The expiration a client sees is
//! [`ExpiryChainResponse::expires_at`] — the planner's absolute instant, which
//! is deterministic. Surfacing the stamp would put a value in the contract that
//! changes between two otherwise-identical replays.

use crate::api::rest::greeks::{GreekLevel, GreeksResponse, greeks_for};
use crate::domain::series::SeriesSnapshot;
use crate::session::{ExpiryRule, ExpiryRuleKind, SessionV2, StrikeLadder};
use chrono::{DateTime, SecondsFormat, Utc};
use optionstratlib::chains::OptionData;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Renders an instant the way every v2 timestamp is rendered.
///
/// Whole seconds and a `Z` suffix. The effective start is normalised to a whole
/// second at creation and the step interval is an integer number of seconds, so
/// no v2 instant has a sub-second part to lose — and pinning the format is what
/// keeps a repeated export byte-comparable (ADR 0001 §3.1).
#[must_use]
#[inline]
fn render_instant(instant: DateTime<Utc>) -> String {
    instant.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Converts an optional decimal to the wire's `f64`.
#[must_use]
#[inline]
fn decimal_to_f64(value: Option<Decimal>) -> Option<f64> {
    value.and_then(|value| value.to_f64())
}

/// Where a simulation's cursor is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CursorResponse {
    /// The 0-based index of the next snapshot to serve.
    pub current_step: usize,
    /// The total number of snapshots the simulation serves.
    pub total_steps: usize,
}

/// One expiration rule, echoed in the normalised form that is a replay input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScheduleRuleResponse {
    /// The rule's stable identifier, which is also its label on every chain.
    pub rule_id: String,
    /// `daily`, `weekly`, `monthly` or `yearly`.
    pub kind: String,
    /// How many non-expired expirations the rule keeps available.
    pub target_count: usize,
    /// The weekdays a `weekly` rule expires on, deduplicated and Monday-first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekdays: Option<Vec<String>>,
    /// The weekday a `monthly` or `yearly` rule expires on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekday: Option<String>,
    /// The month a `yearly` rule expires in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month: Option<u32>,
}

/// The effective parameters of a simulation.
///
/// This is exactly the replay-input list of ADR 0001 §8: a client that records
/// this object can recreate the run without having kept the request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SimulationParametersResponse {
    /// Ticker symbol of the underlying.
    pub symbol: String,
    /// Number of steps the simulation runs for.
    pub steps: usize,
    /// The resolved RNG seed. Never absent: a v2 simulation is always
    /// reproducible.
    pub seed: u64,
    /// The resolved simulated start, in whole-second UTC.
    pub effective_start: String,
    /// The resolved interval between simulated steps, in seconds.
    pub step_interval_seconds: u64,
    /// The time frame the stochastic model is scaled by.
    pub time_frame: String,
    /// The IANA zone the expiration time is expressed in.
    pub timezone: String,
    /// The calendar policy version the schedule is evaluated under.
    pub calendar: String,
    /// The IANA time-zone database release the expirations were resolved
    /// against. A replay against a different release is still a replay — it is
    /// just one the client can now detect.
    pub tzdb_version: String,
    /// The local time of day every expiration expires at.
    pub expiration_time: String,
    /// The normalised expiration rules, ordered by `rule_id`.
    pub schedules: Vec<ScheduleRuleResponse>,
    /// Initial price of the underlying.
    pub initial_price: f64,
    /// The volatility the simulation was created with, echoed back for replay.
    ///
    /// It is the one base volatility for every walk model that carries one. For
    /// `Historical` it prices nothing — that walk estimates a volatility per
    /// step from its own series (ADR 0001 §8.1) — and the values that did price
    /// the chains are each snapshot's `base_volatility`.
    pub volatility: f64,
    /// Annualised risk-free rate.
    pub risk_free_rate: f64,
    /// Annualised dividend yield.
    pub dividend_yield: f64,
    /// The stochastic model driving the underlying path.
    #[schema(value_type = Object)]
    pub method: serde_json::Value,
    /// Number of strikes per chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_size: Option<usize>,
    /// Interval between strikes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strike_interval: Option<f64>,
    /// Slope of the volatility skew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skew_slope: Option<f64>,
    /// Curvature of the volatility smile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smile_curve: Option<f64>,
    /// Which strikes the simulation quotes: `rolling` or `pinned`.
    pub strike_ladder: StrikeLadder,
    /// How far a pinned ladder may make a step widen the chain, in strikes per
    /// side, resolved once at creation.
    ///
    /// Echoed because it is a replay input like the seed: it decides the first
    /// step at which a pinned simulation refuses a drift, so a replay that
    /// resolved a different one is a different tape boundary. A client
    /// recreating a run on another instance can compare this value and know
    /// whether the boundary moved, rather than discovering it at step k.
    ///
    /// It is not accepted on a request. The number bounds what the service
    /// will build on a client's behalf, so letting a client raise it would
    /// turn a resource guard into a suggestion.
    pub pinned_width_ceiling: usize,
    /// The constant term of the spread model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread: Option<f64>,
    /// The proportional term of the spread model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_proportional: Option<f64>,
    /// The moneyness term of the spread model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_moneyness_widening: Option<f64>,
    /// The tenor term of the spread model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_tenor_widening: Option<f64>,
    /// The tick every quote is rounded and floored to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_tick: Option<f64>,
}

/// A simulation's metadata, with no market data attached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SimulationResponse {
    /// The simulation's unique identifier.
    pub id: String,
    /// `initialized`, `in_progress` or `completed`.
    ///
    /// Deliberately `snake_case`, unlike v1's `Display` rendering — which stays
    /// `"In Progress"`, with a space, because it is frozen.
    pub state: String,
    /// The optimistic-concurrency revision.
    pub version: u64,
    /// Where the cursor is.
    pub cursor: CursorResponse,
    /// When the simulation was created, in real time.
    pub created_at: String,
    /// When the simulation was last written, in real time.
    pub updated_at: String,
    /// The effective parameters — the replay inputs.
    pub parameters: SimulationParametersResponse,
}

/// The state of the underlying at one step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UnderlyingResponse {
    /// Ticker symbol.
    pub symbol: String,
    /// The simulated price at this step.
    pub price: f64,
    /// The base implied volatility every chain at this step is priced from,
    /// before skew and smile shape it per strike.
    pub base_volatility: f64,
}

/// A quoted side of one strike.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema, Default)]
pub struct OptionQuoteResponse {
    /// Bid price.
    pub bid: Option<f64>,
    /// Ask price.
    pub ask: Option<f64>,
    /// Mid price.
    pub mid: Option<f64>,
    /// Delta.
    pub delta: Option<f64>,
    /// The greeks selected by the `greeks` query parameter, per one long
    /// contract. Absent entirely at the default level, so a client that does
    /// not ask sees the response it has always seen; `first` carries the
    /// remaining first-order greeks and `all` the full twelve-value snapshot.
    ///
    /// Decimal-valued, so these arrive as JSON strings rather than numbers:
    /// they are the upstream values verbatim, not a lossy `f64` view of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub greeks: Option<GreeksResponse>,
}

/// One strike of one expiration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ContractResponse {
    /// The strike price.
    pub strike: f64,
    /// The per-strike implied volatility, shaped by skew and smile.
    pub implied_volatility: f64,
    /// Gamma, shared by the call and the put.
    pub gamma: Option<f64>,
    /// The call side.
    pub call: OptionQuoteResponse,
    /// The put side.
    pub put: OptionQuoteResponse,
}

/// One live expiration at one step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ExpiryChainResponse {
    /// The absolute expiration instant, in UTC. This is the authoritative
    /// expiration: it comes from the planner and is fully deterministic.
    pub expires_at: String,
    /// Fractional days remaining, from the same pair the planner used. Always
    /// strictly positive — an expired chain is never served.
    pub days_to_expiration: f64,
    /// Every rule this expiration satisfies, sorted. A date claimed by two
    /// rules appears once, with both labels.
    pub labels: Vec<String>,
    /// The strikes, ascending.
    pub contracts: Vec<ContractResponse>,
}

/// The whole simulated market at one step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SnapshotResponse {
    /// The simulation's unique identifier.
    pub id: String,
    /// The lifecycle state at the time of the response.
    pub state: String,
    /// The optimistic-concurrency revision.
    pub version: u64,
    /// Where the cursor is.
    pub cursor: CursorResponse,
    /// The simulated instant of this snapshot — derived from the effective
    /// start and the cursor, never from the wall clock.
    pub simulated_at: String,
    /// The underlying's state at this step.
    pub underlying: UnderlyingResponse,
    /// The live chains, ordered by expiration.
    pub chains: Vec<ExpiryChainResponse>,
}

/// Views one strike at the requested greek level.
///
/// A free function rather than a `From` impl because the level has to reach it:
/// the same `OptionData` renders three different payloads depending on what the
/// caller asked for. `implied_volatility`, `gamma` and the per-side `delta`
/// keep reading the convenience mirrors on `OptionData` — computed
/// independently of the snapshots, and defined at expiry and at zero
/// volatility where the full set is not — so the default response is
/// unchanged at every strike, degenerate ones included.
#[must_use]
pub(crate) fn contract_response(data: &OptionData, level: GreekLevel) -> ContractResponse {
    let (call_greeks, put_greeks) = greeks_for(data, level);
    ContractResponse {
        strike: data.strike_price.to_f64(),
        implied_volatility: data.implied_volatility.to_f64(),
        gamma: decimal_to_f64(data.gamma),
        call: OptionQuoteResponse {
            bid: data.call_bid.map(|value| value.to_f64()),
            ask: data.call_ask.map(|value| value.to_f64()),
            mid: data.call_middle.map(|value| value.to_f64()),
            delta: decimal_to_f64(data.delta_call),
            greeks: call_greeks,
        },
        put: OptionQuoteResponse {
            bid: data.put_bid.map(|value| value.to_f64()),
            ask: data.put_ask.map(|value| value.to_f64()),
            mid: data.put_middle.map(|value| value.to_f64()),
            delta: decimal_to_f64(data.delta_put),
            greeks: put_greeks,
        },
    }
}

impl From<&ExpiryRule> for ScheduleRuleResponse {
    fn from(rule: &ExpiryRule) -> Self {
        let (kind, weekdays, weekday, month) = match rule.kind() {
            ExpiryRuleKind::Daily => ("daily", None, None, None),
            ExpiryRuleKind::Weekly { weekdays } => (
                "weekly",
                Some(weekdays.iter().map(ToString::to_string).collect()),
                None,
                None,
            ),
            ExpiryRuleKind::Monthly { weekday } => {
                ("monthly", None, Some(weekday.to_string()), None)
            }
            ExpiryRuleKind::Yearly { weekday, month } => {
                ("yearly", None, Some(weekday.to_string()), Some(*month))
            }
        };

        Self {
            rule_id: rule.rule_id().to_string(),
            kind: kind.to_string(),
            target_count: rule.target_count().get(),
            weekdays,
            weekday,
            month,
        }
    }
}

impl From<&SessionV2> for SimulationParametersResponse {
    fn from(simulation: &SessionV2) -> Self {
        let parameters = &simulation.parameters;
        let schedule = &parameters.schedule;

        Self {
            symbol: parameters.symbol.clone(),
            steps: parameters.steps,
            seed: parameters.seed,
            effective_start: render_instant(parameters.effective_start),
            step_interval_seconds: parameters.step_interval_seconds,
            time_frame: parameters.time_frame.to_string(),
            timezone: schedule.timezone().name().to_string(),
            calendar: schedule.calendar().as_str().to_string(),
            tzdb_version: parameters.tzdb_version.clone(),
            expiration_time: schedule.expiration_time().format("%H:%M:%S").to_string(),
            schedules: schedule.rules().iter().map(Into::into).collect(),
            initial_price: parameters.initial_price.to_f64(),
            volatility: parameters.volatility.to_f64(),
            risk_free_rate: parameters.risk_free_rate.to_f64().unwrap_or_default(),
            dividend_yield: parameters.dividend_yield.to_f64(),
            // The walk model is upstream's type; it is echoed as the JSON it
            // serialises to rather than mirrored into a second enum, because
            // there is nothing v2-specific to say about it.
            method: serde_json::to_value(&parameters.method).unwrap_or(serde_json::Value::Null),
            chain_size: parameters.chain_size,
            strike_interval: parameters.strike_interval.map(|value| value.to_f64()),
            skew_slope: parameters.skew_slope.and_then(|value| value.to_f64()),
            smile_curve: parameters.smile_curve.and_then(|value| value.to_f64()),
            strike_ladder: parameters.strike_ladder,
            pinned_width_ceiling: parameters.pinned_width_ceiling,
            spread: parameters.spread.map(|value| value.to_f64()),
            spread_proportional: parameters
                .spread_proportional
                .and_then(|value| value.to_f64()),
            spread_moneyness_widening: parameters
                .spread_moneyness_widening
                .and_then(|value| value.to_f64()),
            spread_tenor_widening: parameters
                .spread_tenor_widening
                .and_then(|value| value.to_f64()),
            spread_tick: parameters.spread_tick.map(|value| value.to_f64()),
        }
    }
}

impl From<&SessionV2> for SimulationResponse {
    fn from(simulation: &SessionV2) -> Self {
        Self {
            id: simulation.id.to_string(),
            state: render_state(simulation),
            version: simulation.version,
            cursor: CursorResponse {
                current_step: simulation.current_step,
                total_steps: simulation.total_steps,
            },
            created_at: render_system_time(simulation.created_at),
            updated_at: render_system_time(simulation.updated_at),
            parameters: simulation.into(),
        }
    }
}

/// Renders a simulation's lifecycle state in v2's `snake_case`.
///
/// v1 renders the `Display` form and must keep doing so; v2 does not inherit
/// that spelling (ADR 0001 §7).
#[must_use]
fn render_state(simulation: &SessionV2) -> String {
    use crate::session::SessionState;

    match simulation.state {
        SessionState::Initialized => "initialized",
        SessionState::InProgress => "in_progress",
        SessionState::Completed => "completed",
        SessionState::Error => "error",
        // Unreachable for a v2 simulation, which is immutable after creation
        // and whose stored form rejects both states on load. Rendered rather
        // than panicked on, because a response is not the place to discover it.
        SessionState::Modified => "modified",
        SessionState::Reinitialized => "reinitialized",
    }
    .to_string()
}

/// Renders a real-time timestamp the same way as every other v2 instant.
#[must_use]
fn render_system_time(time: std::time::SystemTime) -> String {
    render_instant(DateTime::<Utc>::from(time))
}

/// Builds a snapshot response from a simulation and the snapshot it served.
///
/// A free function rather than a `From` impl because it needs both, and the
/// pairing is the point: the cursor and state come from the simulation as it
/// was when the snapshot was taken.
#[must_use]
pub(crate) fn snapshot_response(
    simulation: &SessionV2,
    snapshot: &SeriesSnapshot,
    level: GreekLevel,
) -> SnapshotResponse {
    SnapshotResponse {
        id: simulation.id.to_string(),
        state: render_state(simulation),
        version: simulation.version,
        cursor: CursorResponse {
            current_step: simulation.current_step,
            total_steps: simulation.total_steps,
        },
        simulated_at: render_instant(snapshot.simulated_at),
        underlying: UnderlyingResponse {
            symbol: simulation.parameters.symbol.clone(),
            price: snapshot.spot.to_f64(),
            base_volatility: snapshot.base_volatility.to_f64(),
        },
        chains: snapshot
            .chains
            .iter()
            .map(|chain| ExpiryChainResponse {
                expires_at: render_instant(chain.expires_at),
                days_to_expiration: chain.days_to_expiration.to_f64(),
                labels: chain.labels.clone(),
                contracts: chain
                    .chain
                    .iter()
                    .map(|data| contract_response(data, level))
                    .collect(),
            })
            .collect(),
    }
}
