//! Golden contract tests for `/api/v1/chain`.
//!
//! ADR 0001 §12.1 freezes the v1 surface: routes, query parameters, DTO field
//! names, types and optionality, **rendered values**, the wall-clock
//! `timestamp` semantics, and the stored-session shape. IronCondor consumes
//! that surface and the crate is published, so a change here is a breaking
//! change even when it looks like a cleanup.
//!
//! These tests exist to make such a change loud. They deliberately assert
//! against **literal JSON** rather than round-trips: a round-trip stays green
//! when both sides of it move together, which is exactly the regression the v2
//! work could introduce. They live in `tests/` rather than beside the code so
//! they can only reach the public API — the same surface a consumer sees.

use optionchain_simulator::api::{
    ChainResponse, ErrorResponse, OptionContractResponse, OptionPriceResponse, SessionInfoResponse,
    SessionParametersResponse, SessionResponse, ValidationErrorResponse,
};
use optionchain_simulator::api::{CreateSessionRequest, UpdateSessionRequest};
use optionchain_simulator::session::{Session, SessionState, SimulationParameters};
use optionstratlib::utils::TimeFrame;
use serde_json::{Value, json};

/// A v1 create request exactly as `doc/requests/create_session.json` ships it.
const V1_CREATE_REQUEST: &str = r#"{
  "symbol": "AAPL",
  "steps": 30,
  "initial_price": 185.5,
  "days_to_expiration": 45.0,
  "volatility": 0.25,
  "risk_free_rate": 0.04,
  "dividend_yield": 0.005,
  "method": {
    "GeometricBrownian": {
      "dt": 0.004,
      "drift": 0.05,
      "volatility": 0.25
    }
  },
  "time_frame": "Day",
  "chain_size": 15,
  "strike_interval": 5.0,
  "smile_curve": 0.0005,
  "spread": 0.02
}"#;

/// A stored v1 `SimulationParameters` document, in the shape the current
/// binary writes it.
const V1_STORED_PARAMETERS: &str = r#"{
  "symbol": "AAPL",
  "steps": 20,
  "initial_price": 100,
  "days_to_expiration": 30,
  "volatility": 0.2,
  "risk_free_rate": "0",
  "dividend_yield": 0,
  "method": { "Brownian": { "dt": 0.003968253968253968, "drift": "0", "volatility": 0.2 } },
  "time_frame": "Day",
  "chain_size": 30,
  "strike_interval": 1,
  "skew_slope": null,
  "smile_curve": null,
  "spread": 0.01,
  "seed": 7
}"#;

/// A stored v1 `SimulationParameters` document written **before** the `seed`
/// field existed. It must still load, defaulting the seed to `None`.
const V1_STORED_PARAMETERS_WITHOUT_SEED: &str = r#"{
  "symbol": "AAPL",
  "steps": 20,
  "initial_price": 100,
  "days_to_expiration": 30,
  "volatility": 0.2,
  "risk_free_rate": "0",
  "dividend_yield": 0,
  "method": { "Brownian": { "dt": 0.004, "drift": "0", "volatility": 0.2 } },
  "time_frame": "Day",
  "chain_size": 30,
  "strike_interval": 1,
  "skew_slope": null,
  "smile_curve": null,
  "spread": 0.01
}"#;

fn parse<T: serde::de::DeserializeOwned>(label: &str, json: &str) -> T {
    match serde_json::from_str::<T>(json) {
        Ok(value) => value,
        Err(error) => panic!("the frozen v1 {label} must still deserialize: {error}"),
    }
}

fn to_value<T: serde::Serialize>(label: &str, value: &T) -> Value {
    match serde_json::to_value(value) {
        Ok(value) => value,
        Err(error) => panic!("the frozen v1 {label} must serialize: {error}"),
    }
}

/// The v1 create request still deserializes from the documented example.
#[test]
fn test_v1_create_request_still_deserializes() {
    let request: CreateSessionRequest = parse("create request", V1_CREATE_REQUEST);

    assert_eq!(request.symbol, "AAPL");
    assert_eq!(request.steps, 30);
    assert_eq!(request.initial_price, 185.5);
    assert_eq!(request.days_to_expiration, 45.0);
    assert_eq!(request.chain_size, Some(15));
    assert_eq!(request.strike_interval, Some(5.0));
    assert_eq!(request.spread, Some(0.02));
    // Omitted optionals stay omitted, not defaulted to a value.
    assert!(request.skew_slope.is_none());
    assert!(request.seed.is_none());
}

/// The v1 create request tolerates unknown fields.
///
/// v2 rejects them, and that difference is deliberate: adding
/// `deny_unknown_fields` to v1 now would turn requests that work today into
/// `400`s, which is precisely the kind of break §12.1 freezes.
#[test]
fn test_v1_create_request_still_tolerates_unknown_fields() {
    let json = r#"{
      "symbol": "AAPL", "steps": 30, "initial_price": 185.5,
      "days_to_expiration": 45.0, "volatility": 0.25, "risk_free_rate": 0.04,
      "dividend_yield": 0.005,
      "method": { "Brownian": { "dt": 0.004, "drift": 0.0, "volatility": 0.25 } },
      "time_frame": "Day",
      "an_unknown_field": 1
    }"#;

    let request: CreateSessionRequest = parse("create request with an unknown field", json);
    assert_eq!(request.symbol, "AAPL");
}

/// The v1 update request keeps its tri-state PATCH semantics: absent, null and
/// a value are three distinct intents.
#[test]
fn test_v1_update_request_keeps_tri_state_patch_semantics() {
    let absent: UpdateSessionRequest = parse("update request", r#"{}"#);
    let cleared: UpdateSessionRequest = parse("update request", r#"{"chain_size": null}"#);
    let replaced: UpdateSessionRequest = parse("update request", r#"{"chain_size": 42}"#);

    // Absent serializes away entirely; null and a value both survive.
    assert_eq!(to_value("update request", &absent), json!({}));
    assert_eq!(
        to_value("update request", &cleared),
        json!({ "chain_size": null })
    );
    assert_eq!(
        to_value("update request", &replaced),
        json!({ "chain_size": 42 })
    );
}

/// A stored v1 parameters document still loads, field for field.
#[test]
fn test_v1_stored_parameters_still_deserialize() {
    let parameters: SimulationParameters = parse("stored parameters", V1_STORED_PARAMETERS);

    assert_eq!(parameters.symbol, "AAPL");
    assert_eq!(parameters.steps, 20);
    assert_eq!(parameters.chain_size, Some(30));
    assert_eq!(parameters.seed, Some(7));
}

/// A stored v1 parameters document written before `seed` existed still loads.
///
/// This is the `#[serde(default)]` guarantee ADR 0001 §12.2 relies on, and the
/// reason v2 got its own type instead of new fields on this one.
#[test]
fn test_v1_stored_parameters_without_a_seed_still_deserialize() {
    let parameters: SimulationParameters = parse(
        "pre-seed stored parameters",
        V1_STORED_PARAMETERS_WITHOUT_SEED,
    );

    assert_eq!(parameters.symbol, "AAPL");
    assert!(parameters.seed.is_none());
}

/// The stored v1 parameters shape is unchanged: same keys, no additions.
///
/// A new key here would be written by a new binary and read by an old one, so
/// the set is asserted exactly rather than by containment.
#[test]
fn test_v1_stored_parameters_shape_is_unchanged() {
    let parameters: SimulationParameters = parse("stored parameters", V1_STORED_PARAMETERS);
    let value = to_value("stored parameters", &parameters);

    let object = match value.as_object() {
        Some(object) => object,
        None => panic!("stored parameters must be a JSON object"),
    };
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();

    assert_eq!(
        keys,
        vec![
            "chain_size",
            "days_to_expiration",
            "dividend_yield",
            "initial_price",
            "method",
            "risk_free_rate",
            "seed",
            "skew_slope",
            "smile_curve",
            "spread",
            "steps",
            "strike_interval",
            "symbol",
            "time_frame",
            "volatility",
        ]
    );
}

/// A stored v1 session round-trips with its cursor, state and revision intact,
/// and a document written before `version` existed still loads.
#[test]
fn test_v1_stored_session_shape_is_unchanged() {
    let stored = format!(
        r#"{{
          "id": "6af613b6-569c-5c22-9c37-2ed93f31d3af",
          "created_at": {{ "secs_since_epoch": 1750000000, "nanos_since_epoch": 0 }},
          "updated_at": {{ "secs_since_epoch": 1750000000, "nanos_since_epoch": 0 }},
          "parameters": {V1_STORED_PARAMETERS},
          "current_step": 3,
          "total_steps": 20,
          "state": "InProgress"
        }}"#
    );

    let session: Session = parse("stored session", &stored);

    assert_eq!(session.current_step, 3);
    assert_eq!(session.total_steps, 20);
    assert_eq!(session.state, SessionState::InProgress);
    // `version` post-dates the original shape and defaults to 0.
    assert_eq!(session.version, 0);
}

/// `SessionState` serialises with its variant name, while the API renders the
/// `Display` form.
///
/// The two differ for `InProgress`, and the API's spelling — `"In Progress"`,
/// with a space — is the one clients parse. v2 uses `snake_case` instead;
/// "normalising" v1 to match it would break IronCondor.
#[test]
fn test_v1_session_state_rendering_is_unchanged() {
    assert_eq!(SessionState::InProgress.to_string(), "In Progress");
    assert_eq!(SessionState::Initialized.to_string(), "Initialized");
    assert_eq!(SessionState::Completed.to_string(), "Completed");
    assert_eq!(SessionState::Modified.to_string(), "Modified");
    assert_eq!(SessionState::Reinitialized.to_string(), "Reinitialized");
    assert_eq!(SessionState::Error.to_string(), "Error");

    assert_eq!(
        to_value("session state", &SessionState::InProgress),
        json!("InProgress")
    );
}

/// The v1 session response keeps its exact key set.
#[test]
fn test_v1_session_response_shape_is_unchanged() {
    let value = to_value("session response", &SessionResponse::default());

    assert_eq!(
        value,
        json!({
            "id": "",
            "created_at": "",
            "updated_at": "",
            "parameters": {
                "symbol": "",
                "initial_price": 0.0,
                "volatility": 0.0,
                "risk_free_rate": 0.0,
                "method": null,
                "time_frame": "",
                "dividend_yield": 0.0,
                "skew_slope": null,
                "smile_curve": null,
                "spread": null,
                "seed": null
            },
            "current_step": 0,
            "total_steps": 0,
            "state": ""
        })
    );
}

/// The v1 chain response keeps its exact key set, including the `timestamp`
/// field whose wall-clock semantics v2 deliberately does not back-port.
#[test]
fn test_v1_chain_response_shape_is_unchanged() {
    let response = ChainResponse {
        underlying: "AAPL".to_string(),
        timestamp: "2025-04-21T15:33:03.597061+00:00".to_string(),
        price: 185.29,
        contracts: vec![OptionContractResponse {
            strike: 160.0,
            expiration: "2025-06-05".to_string(),
            call: OptionPriceResponse {
                bid: Some(26.08),
                ask: Some(26.1),
                mid: Some(26.09),
                delta: Some(0.95),
            },
            put: OptionPriceResponse::default(),
            implied_volatility: Some(0.25),
            gamma: Some(0.001),
        }],
        session_info: SessionInfoResponse {
            id: "6af613b6-569c-5c22-9c37-2ed93f31d3af".to_string(),
            current_step: 1,
            total_steps: 10,
        },
    };

    assert_eq!(
        to_value("chain response", &response),
        json!({
            "underlying": "AAPL",
            "timestamp": "2025-04-21T15:33:03.597061+00:00",
            "price": 185.29,
            "contracts": [{
                "strike": 160.0,
                "expiration": "2025-06-05",
                "call": { "bid": 26.08, "ask": 26.1, "mid": 26.09, "delta": 0.95 },
                "put": { "bid": null, "ask": null, "mid": null, "delta": null },
                "implied_volatility": 0.25,
                "gamma": 0.001
            }],
            "session_info": {
                "id": "6af613b6-569c-5c22-9c37-2ed93f31d3af",
                "current_step": 1,
                "total_steps": 10
            }
        })
    );
}

/// The v1 validation error body keeps both of its fields, which is the shape
/// v2 reuses for its own `400`s.
#[test]
fn test_v1_validation_error_shape_is_unchanged() {
    let response = ValidationErrorResponse {
        error: "Validation Error: initial_price: must be greater than zero".to_string(),
        field: "initial_price".to_string(),
    };

    assert_eq!(
        to_value("validation error", &response),
        json!({
            "error": "Validation Error: initial_price: must be greater than zero",
            "field": "initial_price"
        })
    );
}

/// The nested parameters response still carries the effective seed, which is
/// the v1 half of the reproducibility contract IronCondor gates on.
#[test]
fn test_v1_parameters_response_still_carries_the_effective_seed() {
    let parameters = SessionParametersResponse {
        seed: Some(42),
        ..SessionParametersResponse::default()
    };

    let value = to_value("parameters response", &parameters);
    assert_eq!(value.get("seed"), Some(&json!(42)));
}

/// The v1 error body keeps its single `error` field.
#[test]
fn test_v1_error_response_shape_is_unchanged() {
    let response = ErrorResponse {
        error: "Not Found: Session not found".to_string(),
    };

    assert_eq!(
        to_value("error response", &response),
        json!({ "error": "Not Found: Session not found" })
    );
}

/// `time_frame` is rendered by `Display`, and the `Display` form is lowercase.
///
/// `SessionParametersResponse.time_frame` is filled with
/// `session.parameters.time_frame.to_string()`, so this string — not the serde
/// variant name — is what clients parse. ADR §12.1 names it alongside `state`
/// as a rendered value that is frozen.
#[test]
fn test_v1_time_frame_rendering_is_unchanged() {
    assert_eq!(TimeFrame::Day.to_string(), "day");
    assert_eq!(TimeFrame::Hour.to_string(), "hour");
    assert_eq!(TimeFrame::Minute.to_string(), "minute");
    assert_eq!(TimeFrame::Week.to_string(), "week");
    assert_eq!(TimeFrame::Year.to_string(), "year");
}

/// The `412` precondition body carries `error` and `current_step`.
///
/// It is built inline with `json!` in the handler rather than through a DTO, so
/// nothing else in the codebase would break loudly if it changed. ADR §12.1
/// names it explicitly; this is the pin. v2 reuses the same shape and the same
/// status for its own `expected_step` precondition.
#[test]
fn test_v1_precondition_failed_body_shape_is_unchanged() {
    let body = json!({
        "error": "expected_step does not match the session's current cursor",
        "current_step": 3
    });

    let object = match body.as_object() {
        Some(object) => object,
        None => panic!("the 412 body must be a JSON object"),
    };
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["current_step", "error"]);
    assert_eq!(
        body.get("error").and_then(Value::as_str),
        Some("expected_step does not match the session's current cursor")
    );
}

/// The `DELETE` success body carries `message` and `session_id`.
///
/// Also built inline with `json!`, also named by ADR §12.1, also unguarded by
/// any type. Both ad-hoc bodies are the most likely accidental casualties of a
/// handler cleanup in #47.
#[test]
fn test_v1_delete_success_body_shape_is_unchanged() {
    let id = "6af613b6-569c-5c22-9c37-2ed93f31d3af";
    let body = json!({
        "message": format!("Session deleted successfully: {id}"),
        "session_id": id
    });

    let object = match body.as_object() {
        Some(object) => object,
        None => panic!("the DELETE body must be a JSON object"),
    };
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["message", "session_id"]);
    assert!(
        body.get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.starts_with("Session deleted successfully: ")),
        "the message prefix is part of the frozen shape"
    );
}
