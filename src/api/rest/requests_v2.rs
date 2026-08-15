//! Request DTOs for the v2 rolling-simulation API.
//!
//! Separate from [`crate::api::rest::requests`] on purpose: `/api/v1/chain` is
//! frozen (ADR 0001 §12.1), so the rolling contract ships as its own request
//! type rather than as new optional fields on `CreateSessionRequest`.
//!
//! As in v1, the DTO speaks `f64` for JSON ergonomics and the conversion into
//! `Positive` / `Decimal` / `Tz` happens exactly once, in
//! `TryFrom<CreateSimulationRequest> for SimulationParametersV2`
//! (`crate::session::model_v2`).

use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
use crate::session::ExpiryRule;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use utoipa::ToSchema;

/// Creates a deterministic rolling multi-expiration simulation.
///
/// Rejects unknown fields: a typo in a field name is a `400` naming the field
/// rather than a silently ignored parameter that changes the tape.
///
/// A v2 simulation is immutable after creation, so there is no update
/// counterpart to this type — changing any of these values means a new
/// simulation (ADR 0001 §6).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateSimulationRequest {
    /// Ticker symbol of the underlying being simulated.
    pub symbol: String,
    /// Number of steps the simulation runs for.
    pub steps: usize,
    /// Optional simulated start instant, RFC 3339. When omitted, one is
    /// generated at conversion, normalised to whole-second UTC, and returned as
    /// the effective start so the run can be replayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub start_at: Option<DateTime<Utc>>,
    /// Fixed interval between simulated steps, in seconds. When omitted it is
    /// derived from `time_frame`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_interval_seconds: Option<u64>,
    /// IANA time-zone name the expiration time is expressed in, e.g.
    /// `America/New_York`.
    pub timezone: String,
    /// Calendar policy version. Defaults to `weekdays_v1`, currently the only
    /// accepted value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<String>,
    /// Local time of day every expiration expires at, `HH:MM` or `HH:MM:SS`.
    pub expiration_time: String,
    /// The rolling expiration rules. Each is one flat object tagged by `kind`;
    /// see ADR 0001 §4.1.
    #[schema(value_type = Vec<Object>)]
    pub schedules: Vec<ExpiryRule>,
    /// Initial price of the underlying.
    pub initial_price: f64,
    /// Initial (and, for constant-volatility models, the only) volatility.
    ///
    /// Must equal the walk model's own `volatility` where the model carries
    /// one: a simulation has exactly one base volatility, and a disagreement is
    /// a `400` naming this field rather than a silent choice made later
    /// (ADR 0001 §4.4).
    ///
    /// `Historical` carries none and needs none — it prices every step from the
    /// realized volatility of the series up to that step, so this field prices
    /// nothing there (ADR 0001 §8.1). The values that did price the chains are
    /// the per-step ones in each snapshot's `base_volatility`.
    pub volatility: f64,
    /// Annualised risk-free rate, as a decimal fraction.
    pub risk_free_rate: f64,
    /// Annualised dividend yield, as a decimal fraction.
    pub dividend_yield: f64,
    /// The stochastic model driving the underlying path.
    pub method: ApiWalkType,
    /// Time frame the stochastic model is scaled by. Distinct from
    /// `step_interval_seconds`, which drives the simulated clock.
    pub time_frame: ApiTimeFrame,
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
    /// Bid-ask spread factor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread: Option<f64>,
    /// RNG seed. When omitted one is generated and returned as the effective
    /// seed, exactly as in v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

impl fmt::Display for CreateSimulationRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let json = serde_json::to_string(self).map_err(|_| fmt::Error)?;
        write!(f, "{json}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference configuration from ADR 0001 §14.1 deserializes.
    const REFERENCE_REQUEST: &str = r#"{
        "symbol": "SPX",
        "steps": 500,
        "start_at": "2026-01-05T14:30:00Z",
        "step_interval_seconds": 86400,
        "timezone": "America/New_York",
        "calendar": "weekdays_v1",
        "expiration_time": "17:00",
        "schedules": [
            { "rule_id": "zero_dte", "kind": "daily", "target_count": 1 },
            { "rule_id": "weeklies", "kind": "weekly", "target_count": 3,
              "weekdays": ["Mon", "Wed", "Fri"] },
            { "rule_id": "monthlies", "kind": "monthly", "target_count": 12,
              "weekday": "Fri" }
        ],
        "initial_price": 5000.0,
        "volatility": 0.18,
        "risk_free_rate": 0.04,
        "dividend_yield": 0.012,
        "method": { "GeometricBrownian": { "dt": 0.004, "drift": 0.05, "volatility": 0.18 } },
        "time_frame": "Day",
        "chain_size": 15,
        "strike_interval": 25.0,
        "skew_slope": -0.2,
        "smile_curve": 0.4,
        "spread": 0.02,
        "seed": 42
    }"#;

    #[test]
    fn test_reference_request_deserializes() {
        match serde_json::from_str::<CreateSimulationRequest>(REFERENCE_REQUEST) {
            Ok(request) => {
                assert_eq!(request.symbol, "SPX");
                assert_eq!(request.steps, 500);
                assert_eq!(request.step_interval_seconds, Some(86_400));
                assert_eq!(request.schedules.len(), 3);
                assert_eq!(request.seed, Some(42));
            }
            Err(error) => panic!("the reference request must deserialize: {error}"),
        }
    }

    /// Every optional field may be omitted.
    #[test]
    fn test_optional_fields_may_be_omitted() {
        let json = r#"{
            "symbol": "SPX",
            "steps": 10,
            "timezone": "America/New_York",
            "expiration_time": "17:00",
            "schedules": [ { "rule_id": "zero_dte", "kind": "daily", "target_count": 1 } ],
            "initial_price": 5000.0,
            "volatility": 0.18,
            "risk_free_rate": 0.04,
            "dividend_yield": 0.0,
            "method": { "Brownian": { "dt": 0.004, "drift": 0.0, "volatility": 0.18 } },
            "time_frame": "Day"
        }"#;

        match serde_json::from_str::<CreateSimulationRequest>(json) {
            Ok(request) => {
                assert!(request.start_at.is_none());
                assert!(request.step_interval_seconds.is_none());
                assert!(request.calendar.is_none());
                assert!(request.seed.is_none());
            }
            Err(error) => panic!("must deserialize without optional fields: {error}"),
        }
    }

    /// An unknown top-level field is rejected rather than ignored.
    #[test]
    fn test_unknown_field_is_rejected() {
        let json = r#"{
            "symbol": "SPX",
            "steps": 10,
            "timezone": "America/New_York",
            "expiration_time": "17:00",
            "schedules": [ { "rule_id": "zero_dte", "kind": "daily", "target_count": 1 } ],
            "initial_price": 5000.0,
            "volatility": 0.18,
            "risk_free_rate": 0.04,
            "dividend_yield": 0.0,
            "method": { "Brownian": { "dt": 0.004, "drift": 0.0, "volatility": 0.18 } },
            "time_frame": "Day",
            "days_to_expiration": 30.0
        }"#;

        let error = match serde_json::from_str::<CreateSimulationRequest>(json) {
            Ok(_) => panic!("an unknown field must be rejected"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("days_to_expiration"),
            "the error must name the offending field, got {error}"
        );
    }

    /// An unknown field inside a schedule rule is rejected too.
    #[test]
    fn test_unknown_field_inside_a_rule_is_rejected() {
        let json = r#"{
            "symbol": "SPX",
            "steps": 10,
            "timezone": "America/New_York",
            "expiration_time": "17:00",
            "schedules": [
                { "rule_id": "zero_dte", "kind": "daily", "target_count": 1, "typo": 1 }
            ],
            "initial_price": 5000.0,
            "volatility": 0.18,
            "risk_free_rate": 0.04,
            "dividend_yield": 0.0,
            "method": { "Brownian": { "dt": 0.004, "drift": 0.0, "volatility": 0.18 } },
            "time_frame": "Day"
        }"#;

        assert!(serde_json::from_str::<CreateSimulationRequest>(json).is_err());
    }

    /// A field that belongs to another rule kind is rejected rather than
    /// silently dropped.
    #[test]
    fn test_field_not_valid_for_the_rule_kind_is_rejected() {
        let json = r#"{
            "symbol": "SPX",
            "steps": 10,
            "timezone": "America/New_York",
            "expiration_time": "17:00",
            "schedules": [
                { "rule_id": "zero_dte", "kind": "daily", "target_count": 1,
                  "weekdays": ["Mon"] }
            ],
            "initial_price": 5000.0,
            "volatility": 0.18,
            "risk_free_rate": 0.04,
            "dividend_yield": 0.0,
            "method": { "Brownian": { "dt": 0.004, "drift": 0.0, "volatility": 0.18 } },
            "time_frame": "Day"
        }"#;

        let error = match serde_json::from_str::<CreateSimulationRequest>(json) {
            Ok(_) => panic!("weekdays on a daily rule must be rejected"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("weekdays"),
            "the error must name the offending field, got {error}"
        );
    }

    /// The request round-trips through JSON, so a logged or echoed request
    /// reads back identically.
    #[test]
    fn test_request_round_trips_through_json() {
        let request = match serde_json::from_str::<CreateSimulationRequest>(REFERENCE_REQUEST) {
            Ok(request) => request,
            Err(error) => panic!("must deserialize: {error}"),
        };

        let json = request.to_string();
        match serde_json::from_str::<CreateSimulationRequest>(&json) {
            Ok(round_tripped) => {
                assert_eq!(round_tripped.symbol, request.symbol);
                assert_eq!(round_tripped.schedules, request.schedules);
                assert_eq!(round_tripped.start_at, request.start_at);
            }
            Err(error) => panic!("must round-trip: {error}"),
        }
    }
}
