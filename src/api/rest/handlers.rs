use crate::api::rest::error::map_error;
use crate::api::rest::limits::{MAX_CHAIN_SIZE, MAX_STEPS};
use crate::api::rest::models::SessionId;
use crate::api::rest::patch::Patch;
use crate::api::rest::requests::{CreateSessionRequest, UpdateSessionRequest};
use crate::api::rest::responses::{
    ChainResponse, ErrorResponse, OptionContractResponse, OptionPriceResponse, SessionInfoResponse,
    SessionParametersResponse, SessionResponse, ValidationErrorResponse,
};
use crate::api::rest::validation::{self, decimal_field, positive_field, strictly_positive_field};
use crate::infrastructure::{MetricsCollector, MongoDBRepository};
use crate::session::{Session, SessionManager, SimulationParameters};
use crate::utils::ChainError;
use actix_web::{HttpRequest, HttpResponse, Responder, web};
use chrono::{DateTime, Utc};
use optionstratlib::chains::OptionChain;
use rand::RngExt;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info};
use utoipa::ToSchema;
use uuid::Uuid;

/// Builds the `ChainResponse` DTO shared by the advance (`POST /api/v1/chain/step`) and
/// peek (`GET /api/v1/chain`) endpoints from a session and its current option-chain
/// snapshot. Kept as a single place so both surfaces emit an identical response shape.
fn build_chain_response(session: &Session, option_chain: &OptionChain) -> ChainResponse {
    let expiration = option_chain.get_expiration_date();
    ChainResponse {
        underlying: option_chain.symbol.clone(),
        timestamp: Utc::now().to_rfc3339(),
        price: option_chain.underlying_price.into(),
        contracts: option_chain
            .iter()
            .map(|contract| {
                let (call_delta, put_delta) = contract.current_deltas();
                let call_ask = contract.get_call_buy_price();
                let put_ask = contract.get_put_buy_price();
                let call_bid = contract.get_call_sell_price();
                let put_bid = contract.get_put_sell_price();
                let volatility = contract.get_volatility();
                OptionContractResponse {
                    strike: contract.strike().into(),
                    expiration: expiration.clone(),
                    call: OptionPriceResponse {
                        bid: call_bid.map(|b| b.into()),
                        ask: call_ask.map(|a| a.into()),
                        mid: contract.call_middle.map(|m| m.into()),
                        delta: call_delta.map(|d| d.to_f64().unwrap_or(0.0)),
                    },
                    put: OptionPriceResponse {
                        bid: put_bid.map(|b| b.into()),
                        ask: put_ask.map(|a| a.into()),
                        mid: contract.put_middle.map(|m| m.into()),
                        delta: put_delta.map(|d| d.to_f64().unwrap_or(0.0)),
                    },
                    implied_volatility: Some(volatility.into()),
                    gamma: contract.current_gamma().map(|g| g.to_f64().unwrap_or(0.0)),
                }
            })
            .collect(),
        session_info: SessionInfoResponse {
            id: session.id.to_string(),
            current_step: session.current_step,
            total_steps: session.total_steps,
        },
    }
}

/// Merges a partial [`UpdateSessionRequest`] into existing [`SimulationParameters`]
/// in place, applying the tri-state PATCH semantics and validating every
/// user-supplied numeric with the same helpers as the create/replace conversions
/// (so a bad float yields a `ChainError::Validation` instead of panicking).
///
/// Per-field behavior:
/// - domain-required fields (`symbol`, `steps`, `initial_price`,
///   `days_to_expiration`, `volatility`, `risk_free_rate`, `dividend_yield`,
///   `method`, `time_frame`) are `Option`: absent keeps the current value, a
///   value replaces it after validation;
/// - domain-optional fields (`chain_size`, `strike_interval`, `skew_slope`,
///   `smile_curve`, `spread`) are [`Patch`]: [`Patch::Absent`] keeps,
///   [`Patch::Null`] clears to `None`, [`Patch::Value`] replaces after validation;
/// - `seed` is [`Patch`] but its invariant forbids `None`: [`Patch::Value`] sets
///   the given seed, [`Patch::Null`] re-seeds with a fresh random seed, and
///   [`Patch::Absent`] keeps the current seed — `params.seed` stays `Some` so the
///   session remains reproducible and its effective seed is always reportable.
///
/// # Errors
///
/// Returns [`ChainError::Validation`] naming the first field that fails
/// validation.
pub(crate) fn apply_update(
    params: &mut SimulationParameters,
    req: &UpdateSessionRequest,
) -> Result<(), ChainError> {
    // Domain-required fields: absent = keep, value = validate + replace.
    if let Some(symbol) = &req.symbol {
        params.symbol = symbol.clone();
    }

    if let Some(steps) = req.steps {
        if steps < 1 {
            return Err(ChainError::Validation {
                field: "steps".to_string(),
                reason: "must be at least 1".to_string(),
            });
        }
        if steps > *MAX_STEPS {
            return Err(ChainError::Validation {
                field: "steps".to_string(),
                reason: format!("must not exceed {}, got {}", *MAX_STEPS, steps),
            });
        }
        params.steps = steps;
    }

    if let Some(initial_price) = req.initial_price {
        params.initial_price = positive_field("initial_price", initial_price)?;
    }

    if let Some(days_to_expiration) = req.days_to_expiration {
        params.days_to_expiration = positive_field("days_to_expiration", days_to_expiration)?;
    }

    if let Some(volatility) = req.volatility {
        params.volatility = positive_field("volatility", volatility)?;
    }

    if let Some(risk_free_rate) = req.risk_free_rate {
        params.risk_free_rate = decimal_field("risk_free_rate", risk_free_rate)?;
    }

    if let Some(dividend_yield) = req.dividend_yield {
        params.dividend_yield = positive_field("dividend_yield", dividend_yield)?;
    }

    if let Some(method) = &req.method {
        params.method = method.clone().try_into()?;
    }

    if let Some(time_frame) = req.time_frame {
        params.time_frame = validation::time_frame_field("time_frame", time_frame)?;
    }

    // Domain-optional fields: absent = keep, null = clear, value = validate + replace.
    match &req.chain_size {
        Patch::Absent => {}
        Patch::Null => params.chain_size = None,
        Patch::Value(chain_size) => {
            let chain_size = *chain_size;
            if chain_size > *MAX_CHAIN_SIZE {
                return Err(ChainError::Validation {
                    field: "chain_size".to_string(),
                    reason: format!("must not exceed {}, got {}", *MAX_CHAIN_SIZE, chain_size),
                });
            }
            params.chain_size = Some(chain_size);
        }
    }

    match &req.strike_interval {
        Patch::Absent => {}
        Patch::Null => params.strike_interval = None,
        Patch::Value(value) => {
            params.strike_interval = Some(strictly_positive_field("strike_interval", *value)?);
        }
    }

    match &req.skew_slope {
        Patch::Absent => {}
        Patch::Null => params.skew_slope = None,
        Patch::Value(value) => params.skew_slope = Some(decimal_field("skew_slope", *value)?),
    }

    match &req.smile_curve {
        Patch::Absent => {}
        Patch::Null => params.smile_curve = None,
        Patch::Value(value) => params.smile_curve = Some(decimal_field("smile_curve", *value)?),
    }

    match &req.spread {
        Patch::Absent => {}
        Patch::Null => params.spread = None,
        Patch::Value(value) => params.spread = Some(positive_field("spread", *value)?),
    }

    // Seed keeps the effective-seed invariant: it is never cleared to None. A
    // null seed means "give me a fresh random seed" rather than "clear it".
    match &req.seed {
        Patch::Absent => {}
        Patch::Null => params.seed = Some(rand::rng().random()),
        Patch::Value(seed) => params.seed = Some(*seed),
    }

    Ok(())
}

#[utoipa::path(
    post,
    path = "/api/v1/chain",
    request_body(
        example = r#"
                    {
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
                      "skew_slope": -0.2,
                      "smile_curve": 0.5,
                      "spread": 0.02
                    }
                    "#
    ),
    responses(
        (status = 201, description = "Session created successfully", body = SessionResponse),
        (status = 400, description = "Validation failed: a parameter was non-finite, out of range (e.g. negative price/volatility), or steps/chain_size exceeded the configured limits.", body = ValidationErrorResponse),
        (status = 409, description = "Session id already exists", body = ErrorResponse),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn create_session(
    req: HttpRequest,
    session_manager: web::Data<Arc<SessionManager>>,
    metrics_collector: web::Data<Arc<MetricsCollector>>,
    mongodb_repo: web::Data<Arc<MongoDBRepository>>,
    json_req: web::Json<CreateSessionRequest>,
) -> impl Responder {
    info!("{} {}: body={}", req.method(), req.path(), json_req.0);

    // Validate and convert the request into domain SimulationParameters. Invalid input
    // (negative/non-finite numerics, out-of-range steps/chain_size, ...) yields a 400
    // instead of panicking during conversion. Validate before touching the active-session
    // metric so a rejected request does not inflate the counter.
    let simulation_params: SimulationParameters = match json_req.0.try_into() {
        Ok(params) => params,
        Err(error) => return map_error(error),
    };

    metrics_collector.increment_active_sessions();

    // Create session using session manager
    match session_manager.create_session(simulation_params).await {
        Ok(session) => {
            let created_at_utc = DateTime::<Utc>::from(session.created_at);
            let updated_at_utc = DateTime::<Utc>::from(session.updated_at);
            let method_value =
                serde_json::to_value(&session.parameters.method).unwrap_or(serde_json::Value::Null);
            let response = SessionResponse {
                id: session.id.to_string(),
                created_at: created_at_utc.to_rfc3339(),
                updated_at: updated_at_utc.to_rfc3339(),
                parameters: SessionParametersResponse {
                    symbol: session.parameters.symbol,
                    initial_price: session.parameters.initial_price.into(),
                    volatility: session.parameters.volatility.into(),
                    risk_free_rate: session.parameters.risk_free_rate.to_f64().unwrap(),
                    method: method_value,
                    time_frame: session.parameters.time_frame.to_string(),
                    dividend_yield: session.parameters.dividend_yield.into(),
                    skew_slope: session.parameters.skew_slope.map(|f| f.to_f64().unwrap()),
                    smile_curve: session.parameters.smile_curve.map(|f| f.to_f64().unwrap()),
                    spread: session.parameters.spread.map(|f| f.into()),
                    seed: session.parameters.seed,
                },
                current_step: session.current_step,
                total_steps: session.total_steps,
                state: session.state.to_string(),
            };

            // Save to MongoDB
            if let Err(e) = mongodb_repo
                .save_session_event(
                    session.id,
                    response.clone(),
                    metrics_collector.get_ref().clone(),
                )
                .await
            {
                error!(session_id = %session.id, "Failed to save session event to MongoDB: {}", e);
                // Continue as this is not critical for the main flow
            }
            HttpResponse::Created().json(response)
        }
        Err(error) => map_error(error),
    }
}

/// Query parameters for the advance-step command: the session id plus an
/// optional expected-cursor precondition for safe retries.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub(crate) struct AdvanceStepQuery {
    /// ID of the session to advance one step.
    #[serde(rename = "sessionid")]
    pub(crate) session_id: String,
    /// Optional expected cursor: when provided, the advance only proceeds if
    /// the session's current step matches — otherwise 412 is returned with
    /// the actual cursor, letting a client resolve an ambiguous retry
    /// (response lost after the save) without consuming another step.
    #[serde(default)]
    pub(crate) expected_step: Option<usize>,
}

#[utoipa::path(
    post,
    path = "/api/v1/chain/step",
    description = "Advance the session one step and return the served snapshot. Serves the \
        snapshot at the current cursor (index 0 first), then advances the cursor; the \
        advance that serves the last snapshot persists Completed, and any further call \
        returns 410 Gone. This is an explicit, state-mutating command. Use \
        GET /api/v1/chain for a safe, repeatable peek. Pass `expected_step` (the cursor \
        you believe the session is at) to make retries safe: if a previous attempt \
        already consumed the step, the call returns 412 with the actual cursor instead \
        of consuming another one.",
    params(
        ("sessionid" = String, Query, description = "ID of the session to advance one step"),
        ("expected_step" = Option<usize>, Query, description = "Expected current cursor; mismatch returns 412 without advancing")
    ),
    responses(
        (status = 200, description = "Advanced one step; served snapshot returned", body = ChainResponse),
        (status = 404, description = "Session not found"),
        (status = 410, description = "Simulation completed. No more steps available"),
        (status = 412, description = "expected_step does not match the session's current cursor; body carries `error` and `current_step`"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn advance_step(
    req: HttpRequest,
    session_manager: web::Data<Arc<SessionManager>>,
    metrics_collector: web::Data<Arc<MetricsCollector>>,
    mongodb_repo: web::Data<Arc<MongoDBRepository>>,
    query: web::Query<AdvanceStepQuery>,
) -> impl Responder {
    info!(
        "{} {}: session_id={}",
        req.method(),
        req.path(),
        query.session_id
    );
    let start_time = std::time::Instant::now();

    // Parse the session ID
    let session_id = match Uuid::parse_str(&query.session_id) {
        Ok(id) => id,
        Err(_) => {
            return map_error(ChainError::InvalidState(
                "Invalid session ID format".to_string(),
            ));
        }
    };

    // Expected-cursor precondition: a transport-level check (412) so an
    // ambiguous retry can be resolved without consuming another step.
    if let Some(expected) = query.expected_step {
        match session_manager.get_session(session_id).await {
            Ok(session) => {
                if session.current_step != expected {
                    return HttpResponse::PreconditionFailed().json(serde_json::json!({
                        "error": "expected_step does not match the session's current cursor",
                        "current_step": session.current_step,
                    }));
                }
            }
            Err(error) => return map_error(error),
        }
    }

    // Advance the session one step (mutates state and persists it).
    match session_manager.get_next_step(session_id).await {
        Ok((session, option_chain)) => {
            let response = build_chain_response(&session, &option_chain);
            let duration = start_time.elapsed();
            metrics_collector.record_simulation_step(&session.parameters.method.to_string());
            metrics_collector.record_simulation_duration(duration);

            // Save to MongoDB
            if let Err(e) = mongodb_repo
                .save_chain_step(
                    session_id,
                    response.clone(),
                    metrics_collector.get_ref().clone(),
                )
                .await
            {
                error!(session_id = %session_id, "Failed to save chain step to MongoDB: {}", e);
                // Continue as this is not critical for the main flow
            }
            HttpResponse::Ok().json(response)
        }
        Err(error) => map_error(error),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/chain",
    description = "Returns the snapshot the next advance will serve, without advancing the \
        session; safe and repeatable (a peek). The same snapshot is returned until an \
        explicit advance via POST /api/v1/chain/step moves the cursor. This endpoint does \
        not mutate session state or record a simulation step.",
    params(
        ("sessionid" = String, Query, description = "ID of the session to read the current snapshot for")
    ),
    responses(
        (status = 200, description = "Current snapshot returned (read-only; repeatable)", body = ChainResponse),
        (status = 404, description = "Session not found"),
        (status = 410, description = "Session completed; no current step available"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn get_current_step(
    req: HttpRequest,
    session_manager: web::Data<Arc<SessionManager>>,
    query: web::Query<SessionId>,
) -> impl Responder {
    info!(
        "{} {}: session_id={}",
        req.method(),
        req.path(),
        query.session_id
    );

    // Parse the session ID
    let session_id = match Uuid::parse_str(&query.session_id) {
        Ok(id) => id,
        Err(_) => {
            return map_error(ChainError::InvalidState(
                "Invalid session ID format".to_string(),
            ));
        }
    };

    // Peek the current snapshot: read-only, repeatable, no state change and no persistence.
    // No simulation-step metric is recorded and no chain-step event is written, because the
    // same step is served repeatedly.
    match session_manager.peek_current_step(session_id).await {
        Ok((session, option_chain)) => {
            let response = build_chain_response(&session, &option_chain);
            HttpResponse::Ok().json(response)
        }
        Err(error) => map_error(error),
    }
}

#[utoipa::path(
    put,
    path = "/api/v1/chain",
    params(
        ("sessionid" = String, Query, description = "ID of the session to replace")
    ),
    request_body(
        content = CreateSessionRequest,
        description = "New session parameters to replace the existing session"
    ),
    responses(
        (status = 200, description = "Session replaced", body = SessionResponse),
        (status = 400, description = "Validation failed: a parameter was non-finite, out of range, or exceeded the configured limits.", body = ValidationErrorResponse),
        (status = 404, description = "Session not found"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn replace_session(
    req: HttpRequest,
    session_manager: web::Data<Arc<SessionManager>>,
    metrics_collector: web::Data<Arc<MetricsCollector>>,
    query: web::Query<SessionId>,
    mongodb_repo: web::Data<Arc<MongoDBRepository>>,
    json_req: web::Json<CreateSessionRequest>,
) -> impl Responder {
    info!(
        "{} {}: body={} session_id={}",
        req.method(),
        req.path(),
        json_req.0,
        query.session_id
    );

    // Parse the session ID
    let session_id = match Uuid::parse_str(&query.session_id) {
        Ok(id) => id,
        Err(_) => {
            return map_error(ChainError::InvalidState(
                "Invalid session ID format".to_string(),
            ));
        }
    };

    // Validate and convert the request into domain SimulationParameters; reuse the same
    // fallible conversion as create so PUT cannot bypass the parameter bounds.
    let simulation_params: SimulationParameters = match json_req.0.try_into() {
        Ok(params) => params,
        Err(error) => return map_error(error),
    };

    // Replace session using session manager
    match session_manager
        .reinitialize_session(session_id, simulation_params)
        .await
    {
        Ok(session) => {
            let created_at_utc = DateTime::<Utc>::from(session.created_at);
            let updated_at_utc = DateTime::<Utc>::from(session.updated_at);
            let method_value =
                serde_json::to_value(&session.parameters.method).unwrap_or(serde_json::Value::Null);
            let response = SessionResponse {
                id: session.id.to_string(),
                created_at: created_at_utc.to_rfc3339(),
                updated_at: updated_at_utc.to_rfc3339(),
                parameters: SessionParametersResponse {
                    symbol: session.parameters.symbol,
                    initial_price: session.parameters.initial_price.into(),
                    volatility: session.parameters.volatility.into(),
                    risk_free_rate: session.parameters.risk_free_rate.to_f64().unwrap(),
                    method: method_value,
                    time_frame: session.parameters.time_frame.to_string(),
                    dividend_yield: session.parameters.dividend_yield.into(),
                    skew_slope: session.parameters.skew_slope.map(|f| f.to_f64().unwrap()),
                    smile_curve: session.parameters.smile_curve.map(|f| f.to_f64().unwrap()),
                    spread: session.parameters.spread.map(|f| f.into()),
                    seed: session.parameters.seed,
                },
                current_step: session.current_step,
                total_steps: session.total_steps,
                state: session.state.to_string(),
            };

            // Save to MongoDB
            if let Err(e) = mongodb_repo
                .save_session_event(
                    session_id,
                    response.clone(),
                    metrics_collector.get_ref().clone(),
                )
                .await
            {
                error!(session_id = %session_id, "Failed to save reinitialized session event to MongoDB: {}", e);
                // Continue as this is not critical for the main flow
            }

            HttpResponse::Ok().json(response)
        }
        Err(error) => map_error(error),
    }
}

#[utoipa::path(
    patch,
    path = "/api/v1/chain",
    params(
        ("sessionid" = String, Query, description = "ID of the session to update")
    ),
    request_body(
        content = UpdateSessionRequest,
        description = "Partial update. Optional fields are tri-state: omit a key to keep the \
            current value, send `null` to clear it, or send a value to replace it. \
            `seed: null` re-seeds the session with a fresh random seed (the seed is never \
            cleared, preserving reproducibility).",
        example = r#"
                    {
                      "volatility": 0.3,
                      "skew_slope": -0.15,
                      "smile_curve": null,
                      "seed": null
                    }
                    "#
    ),
    responses(
        (status = 200, description = "Session updated", body = SessionResponse),
        (status = 404, description = "Session not found"),
        (status = 400, description = "Validation failed: a supplied parameter was non-finite, out of range, or exceeded the configured limits.", body = ValidationErrorResponse),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn update_session(
    req: HttpRequest,
    session_manager: web::Data<Arc<SessionManager>>,
    query: web::Query<SessionId>,
    metrics_collector: web::Data<Arc<MetricsCollector>>,
    mongodb_repo: web::Data<Arc<MongoDBRepository>>,
    json_req: web::Json<UpdateSessionRequest>,
) -> impl Responder {
    info!(
        "{} {}: body={} session_id={}",
        req.method(),
        req.path(),
        json_req.0,
        query.session_id
    );

    // Parse the session ID
    let session_id = match Uuid::parse_str(&query.session_id) {
        Ok(id) => id,
        Err(_) => {
            return map_error(ChainError::InvalidState(
                "Invalid session ID format".to_string(),
            ));
        }
    };

    // Get current session to update only the parameters that were provided
    let current_session = match session_manager.get_session(session_id).await {
        Ok(session) => session,
        Err(error) => return map_error(error),
    };

    // Create a new SimulationParameters object with updated values. The merge applies the
    // tri-state PATCH semantics (absent = keep, null = clear, value = replace) and validates
    // every user-supplied numeric with the same helpers as the create/replace conversions, so
    // a bad float yields a 400 instead of panicking during the PATCH merge.
    let mut updated_params = current_session.parameters.clone();
    if let Err(error) = apply_update(&mut updated_params, &json_req.0) {
        return map_error(error);
    }

    // Update the session with new parameters
    match session_manager
        .update_session(session_id, updated_params)
        .await
    {
        Ok(session) => {
            let created_at_utc = DateTime::<Utc>::from(session.created_at);
            let updated_at_utc = DateTime::<Utc>::from(session.updated_at);
            let method_value =
                serde_json::to_value(&session.parameters.method).unwrap_or(serde_json::Value::Null);

            let response = SessionResponse {
                id: session.id.to_string(),
                created_at: created_at_utc.to_rfc3339(),
                updated_at: updated_at_utc.to_rfc3339(),
                parameters: SessionParametersResponse {
                    symbol: session.parameters.symbol,
                    initial_price: session.parameters.initial_price.into(),
                    volatility: session.parameters.volatility.into(),
                    risk_free_rate: session.parameters.risk_free_rate.to_f64().unwrap_or(0.0),
                    method: method_value,
                    time_frame: session.parameters.time_frame.to_string(),
                    dividend_yield: session.parameters.dividend_yield.into(),
                    skew_slope: session
                        .parameters
                        .skew_slope
                        .map(|f| f.to_f64().unwrap_or(0.0)),
                    smile_curve: session
                        .parameters
                        .smile_curve
                        .map(|f| f.to_f64().unwrap_or(0.0)),
                    spread: session.parameters.spread.map(|f| f.into()),
                    seed: session.parameters.seed,
                },
                current_step: session.current_step,
                total_steps: session.total_steps,
                state: session.state.to_string(),
            };

            // Save to MongoDB
            if let Err(e) = mongodb_repo
                .save_session_event(
                    session_id,
                    response.clone(),
                    metrics_collector.get_ref().clone(),
                )
                .await
            {
                error!(session_id = %session_id, "Failed to save updated session event to MongoDB: {}", e);
                // Continue as this is not critical for the main flow
            }

            HttpResponse::Ok().json(response)
        }
        Err(error) => map_error(error),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/chain",
    params(
        ("sessionid" = String, Query, description = "ID of the session to delete")
    ),
    responses(
        (status = 200, description = "Session deleted", body = String),
        (status = 404, description = "Session not found"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn delete_session(
    req: HttpRequest,
    session_manager: web::Data<Arc<SessionManager>>,
    query: web::Query<SessionId>,
    metrics_collector: web::Data<Arc<MetricsCollector>>,
) -> impl Responder {
    info!(
        "{} {}: session_id={}",
        req.method(),
        req.path(),
        query.session_id
    );
    metrics_collector.decrement_active_sessions();
    let session_id = Uuid::parse_str(&query.session_id)
        .map_err(|_| ChainError::InvalidState("Invalid session ID format".to_string()));

    match session_id {
        Ok(id) => match session_manager.delete_session(id).await {
            Ok(true) => {
                let msg = format!("Session deleted successfully: {}", id);
                let msg = serde_json::json!({
                    "message": msg,
                    "session_id": id.to_string()
                });
                HttpResponse::Ok().json(msg)
            }
            Ok(false) => HttpResponse::NotFound().json(serde_json::json!({
                "error": format!("Session not found: {}", id)
            })),
            Err(chain_error) => {
                error!("{} {}", id, chain_error);
                map_error(chain_error)
            }
        },
        Err(error) => {
            error!("{}", error);
            map_error(error)
        }
    }
}

#[cfg(test)]
mod tests_advance_step_query {
    use super::AdvanceStepQuery;

    #[test]
    fn test_expected_step_absent_deserializes_to_none() {
        let q: AdvanceStepQuery =
            serde_json::from_str(r#"{"sessionid":"abc"}"#).expect("query must parse");
        assert_eq!(q.session_id, "abc");
        assert_eq!(q.expected_step, None);
    }

    #[test]
    fn test_expected_step_present_deserializes_to_some() {
        let q: AdvanceStepQuery = serde_json::from_str(r#"{"sessionid":"abc","expected_step":3}"#)
            .expect("query must parse");
        assert_eq!(q.expected_step, Some(3));
    }
}

#[cfg(test)]
mod tests_apply_update {
    use super::*;
    use optionstratlib::simulation::WalkType;
    use optionstratlib::utils::TimeFrame;
    use positive::{Positive, pos_or_panic};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    /// Base parameters with every optional field populated, so a `Null` patch has
    /// something to clear and an `Absent` patch has something to preserve.
    fn base_params() -> SimulationParameters {
        SimulationParameters {
            symbol: "AAPL".to_string(),
            steps: 20,
            initial_price: pos_or_panic!(100.0),
            days_to_expiration: pos_or_panic!(30.0),
            volatility: pos_or_panic!(0.2),
            risk_free_rate: dec!(0.03),
            dividend_yield: Positive::ZERO,
            method: WalkType::Brownian {
                dt: pos_or_panic!(1.0 / 252.0),
                drift: Decimal::ZERO,
                volatility: pos_or_panic!(0.2),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(30),
            strike_interval: Some(pos_or_panic!(5.0)),
            skew_slope: Some(dec!(-0.2)),
            smile_curve: Some(dec!(0.4)),
            spread: Some(pos_or_panic!(0.02)),
            seed: Some(42),
        }
    }

    /// An update request that touches nothing (all required fields `None`, all
    /// optional fields `Patch::Absent`).
    fn empty_update() -> UpdateSessionRequest {
        UpdateSessionRequest {
            symbol: None,
            steps: None,
            initial_price: None,
            days_to_expiration: None,
            volatility: None,
            risk_free_rate: None,
            dividend_yield: None,
            method: None,
            time_frame: None,
            chain_size: Patch::Absent,
            strike_interval: Patch::Absent,
            skew_slope: Patch::Absent,
            smile_curve: Patch::Absent,
            spread: Patch::Absent,
            seed: Patch::Absent,
        }
    }

    #[test]
    fn test_apply_update_absent_preserves_all_fields() {
        let mut params = base_params();
        let before = params.clone();

        apply_update(&mut params, &empty_update()).expect("empty update succeeds");

        assert_eq!(params.symbol, before.symbol);
        assert_eq!(params.steps, before.steps);
        assert_eq!(params.chain_size, before.chain_size);
        assert_eq!(params.strike_interval, before.strike_interval);
        assert_eq!(params.skew_slope, before.skew_slope);
        assert_eq!(params.smile_curve, before.smile_curve);
        assert_eq!(params.spread, before.spread);
        assert_eq!(params.seed, before.seed);
    }

    #[test]
    fn test_apply_update_null_clears_each_optional_field() {
        let mut params = base_params();
        let req = UpdateSessionRequest {
            chain_size: Patch::Null,
            strike_interval: Patch::Null,
            skew_slope: Patch::Null,
            smile_curve: Patch::Null,
            spread: Patch::Null,
            ..empty_update()
        };

        apply_update(&mut params, &req).expect("null update succeeds");

        assert_eq!(params.chain_size, None);
        assert_eq!(params.strike_interval, None);
        assert_eq!(params.skew_slope, None);
        assert_eq!(params.smile_curve, None);
        assert_eq!(params.spread, None);
        // Seed was left absent, so it is preserved (never cleared).
        assert_eq!(params.seed, Some(42));
    }

    #[test]
    fn test_apply_update_value_replaces_optional_fields() {
        let mut params = base_params();
        let req = UpdateSessionRequest {
            chain_size: Patch::Value(25),
            strike_interval: Patch::Value(2.5),
            skew_slope: Patch::Value(-0.15),
            smile_curve: Patch::Value(0.6),
            spread: Patch::Value(0.03),
            ..empty_update()
        };

        apply_update(&mut params, &req).expect("value update succeeds");

        assert_eq!(params.chain_size, Some(25));
        assert_eq!(params.strike_interval, Some(pos_or_panic!(2.5)));
        assert_eq!(params.skew_slope, Some(dec!(-0.15)));
        assert_eq!(params.smile_curve, Some(dec!(0.6)));
        assert_eq!(params.spread, Some(pos_or_panic!(0.03)));
    }

    #[test]
    fn test_apply_update_skew_slope_is_now_patchable() {
        // Regression for #20: skew_slope was previously unreachable via PATCH.
        let mut params = base_params();
        params.skew_slope = None;

        let req = UpdateSessionRequest {
            skew_slope: Patch::Value(-0.3),
            ..empty_update()
        };
        apply_update(&mut params, &req).expect("skew_slope patch succeeds");
        assert_eq!(params.skew_slope, Some(dec!(-0.3)));

        // And it can be cleared again.
        let clear = UpdateSessionRequest {
            skew_slope: Patch::Null,
            ..empty_update()
        };
        apply_update(&mut params, &clear).expect("skew_slope clear succeeds");
        assert_eq!(params.skew_slope, None);
    }

    #[test]
    fn test_apply_update_invalid_value_is_validation_error() {
        let mut params = base_params();
        let req = UpdateSessionRequest {
            spread: Patch::Value(-1.0),
            ..empty_update()
        };

        match apply_update(&mut params, &req) {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "spread"),
            other => panic!("expected Validation error for spread, got {other:?}"),
        }
    }

    #[test]
    fn test_apply_update_invalid_skew_slope_is_validation_error() {
        let mut params = base_params();
        let req = UpdateSessionRequest {
            skew_slope: Patch::Value(f64::NAN),
            ..empty_update()
        };

        match apply_update(&mut params, &req) {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "skew_slope"),
            other => panic!("expected Validation error for skew_slope, got {other:?}"),
        }
    }

    #[test]
    fn test_apply_update_seed_value_sets_seed() {
        let mut params = base_params();
        let req = UpdateSessionRequest {
            seed: Patch::Value(777),
            ..empty_update()
        };

        apply_update(&mut params, &req).expect("seed value update succeeds");
        assert_eq!(params.seed, Some(777));
    }

    #[test]
    fn test_apply_update_seed_null_regenerates_fresh_seed() {
        // A null seed must NOT clear the seed (the effective-seed invariant keeps it
        // Some); it re-seeds with a fresh random value. Retry a few times so a random
        // collision with the previous seed does not flake the test.
        let old_seed = 42u64;
        let mut changed = false;
        for _ in 0..3 {
            let mut params = base_params();
            let req = UpdateSessionRequest {
                seed: Patch::Null,
                ..empty_update()
            };
            apply_update(&mut params, &req).expect("seed null update succeeds");

            assert!(
                params.seed.is_some(),
                "seed must stay Some to preserve reproducibility"
            );
            if params.seed != Some(old_seed) {
                changed = true;
                break;
            }
        }
        assert!(
            changed,
            "seed null should produce a fresh seed different from the previous one"
        );
    }
}
