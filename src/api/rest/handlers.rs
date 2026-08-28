use crate::api::rest::error::map_error;
use crate::api::rest::greeks::{GreekLevel, admit_blocking, greeks_for, serialize_body};
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
///
/// `level` is the resolved `greeks` query parameter. At [`GreekLevel::None`] —
/// the default, and what every existing client sends — the response is
/// byte-identical to the one before the parameter existed: `implied_volatility`,
/// `gamma` and the per-side `delta` still come from the convenience mirrors on
/// `OptionData`, which are defined at expiry and at zero volatility where the
/// full greek set is not.
fn build_chain_response(
    session: &Session,
    option_chain: &OptionChain,
    level: GreekLevel,
) -> ChainResponse {
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
                let (call_greeks, put_greeks) = greeks_for(contract, level);
                OptionContractResponse {
                    strike: contract.strike().into(),
                    expiration: expiration.clone(),
                    call: OptionPriceResponse {
                        bid: call_bid.map(|b| b.into()),
                        ask: call_ask.map(|a| a.into()),
                        mid: contract.call_middle.map(|m| m.into()),
                        delta: call_delta.map(|d| d.to_f64().unwrap_or(0.0)),
                        greeks: call_greeks,
                    },
                    put: OptionPriceResponse {
                        bid: put_bid.map(|b| b.into()),
                        ask: put_ask.map(|a| a.into()),
                        mid: contract.put_middle.map(|m| m.into()),
                        delta: put_delta.map(|d| d.to_f64().unwrap_or(0.0)),
                        greeks: put_greeks,
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

/// Renders the v1 chain bodies, under the shared greek admission bound.
///
/// Returns the serialised bytes to write and the DTO to persist. They differ
/// only above the default greek level, where the served payload carries the
/// greeks and the persisted one deliberately does not: the event log records
/// the chain at step N, not what the client who happened to advance it asked
/// for.
///
/// Both are produced inside one admitted blocking job, serialisation included.
/// `HttpResponse::json` would otherwise encode a large document on the Actix
/// worker after the job returned, which is the stall the job exists to avoid.
/// See [`crate::api::rest::greeks::admit_blocking`] for the bound v1 and v2
/// share.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] if the blocking task panics or is dropped,
/// or if the response cannot be serialised.
async fn render_chain_responses(
    session: Session,
    option_chain: OptionChain,
    level: GreekLevel,
) -> Result<(Vec<u8>, ChainResponse), ChainError> {
    admit_blocking(level, move || {
        let served = build_chain_response(&session, &option_chain, level);
        let persisted = if level.wants_greeks() {
            build_chain_response(&session, &option_chain, GreekLevel::None)
        } else {
            served.clone()
        };
        Ok((serialize_body(&served)?, persisted))
    })
    .await
}

/// Writes an already-serialised JSON body.
#[must_use]
fn json_body(bytes: Vec<u8>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(bytes)
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
    // instead of panicking during conversion.
    let simulation_params: SimulationParameters = match json_req.0.try_into() {
        Ok(params) => params,
        Err(error) => return map_error(error),
    };

    // Create session using session manager
    match session_manager.create_session(simulation_params).await {
        Ok(session) => {
            // Only record the active-session metric after the create actually
            // succeeded, so a failed create (store error, 409 AlreadyExists, ...)
            // does not inflate the gauge or the creation counter.
            metrics_collector.record_session_created();

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
#[serde(deny_unknown_fields)]
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
    /// How much of the greek set the served chain should carry: `none` (the
    /// default), `first` or `all`. Kept as a raw string so an unknown value is
    /// a typed `400` naming the field rather than actix's untyped query
    /// rejection.
    #[serde(default)]
    pub(crate) greeks: Option<String>,
}

/// The query of the v1 chain peek: the session, plus how much of the greek set
/// to carry.
///
/// Separate from [`SessionId`] because only the two chain-serving endpoints
/// take the parameter; PUT, PATCH and DELETE return no chain and must not
/// advertise it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChainQuery {
    /// ID of the session to read the current snapshot for.
    #[serde(rename = "sessionid")]
    pub(crate) session_id: String,
    /// How much of the greek set to carry. See [`AdvanceStepQuery::greeks`].
    #[serde(default)]
    pub(crate) greeks: Option<String>,
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
        ("expected_step" = Option<usize>, Query, description = "Expected current cursor; mismatch returns 412 without advancing"),
        ("greeks" = Option<GreekLevel>, Query, description = "How much of the greek set to carry: `none` (default), `first` (adds theta, vega, rho, rho_d) or `all` (the full twelve-value snapshot per style). Every value is per ONE LONG CONTRACT: the client applies position sign and size. The one exception is `alpha`, the ratio gamma/theta, which a short position leaves unchanged and which must NOT be scaled or sign-flipped. An unknown value is a 400")
    ),
    responses(
        (status = 200, description = "Advanced one step; served snapshot returned", body = ChainResponse),
        (status = 400, description = "Malformed session id, or an unknown `greeks` level. The unknown-level body is the typed `{error, field}` of ValidationErrorResponse with `field` = `greeks`; a malformed id carries `error` alone."),
        (status = 404, description = "Session not found"),
        (status = 409, description = "Concurrent modification: another request advanced or modified the session; retry"),
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
    // Rejected before the step is consumed: an unknown level must not advance
    // the cursor on its way to a 400.
    let level = match GreekLevel::parse(query.greeks.as_deref()) {
        Ok(level) => level,
        Err(error) => return map_error(error),
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
            // Read before the session moves into the renderer.
            let method = session.parameters.method.to_string();
            // `persisted` is the response at the DEFAULT level: the event log is
            // a record of the chain at step N, and letting a query parameter
            // shape it would mean two clients advancing the same session wrote
            // differently-shaped documents, with a `greeks=all` advance writing
            // roughly five times the BSON for no gain.
            let (body, persisted) = match render_chain_responses(session, option_chain, level).await
            {
                Ok(rendered) => rendered,
                Err(error) => return map_error(error),
            };
            let duration = start_time.elapsed();
            metrics_collector.record_simulation_step(&method);
            metrics_collector.record_simulation_duration(duration);
            // Publish the current simulation-cache occupancy: an advance may have
            // populated a fresh walk or evicted a completed one (issue #9).
            metrics_collector
                .set_simulation_cache_size(session_manager.simulation_cache_len().await as i64);

            if let Err(e) = mongodb_repo
                .save_chain_step(session_id, persisted, metrics_collector.get_ref().clone())
                .await
            {
                error!(session_id = %session_id, "Failed to save chain step to MongoDB: {}", e);
                // Continue as this is not critical for the main flow
            }
            json_body(body)
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
        ("sessionid" = String, Query, description = "ID of the session to read the current snapshot for"),
        ("greeks" = Option<GreekLevel>, Query, description = "How much of the greek set to carry: `none` (default), `first` (adds theta, vega, rho, rho_d) or `all` (the full twelve-value snapshot per style). Every value is per ONE LONG CONTRACT: the client applies position sign and size. The one exception is `alpha`, the ratio gamma/theta, which a short position leaves unchanged and which must NOT be scaled or sign-flipped. An unknown value is a 400")
    ),
    responses(
        (status = 200, description = "Current snapshot returned (read-only; repeatable)", body = ChainResponse),
        (status = 400, description = "Malformed session id, or an unknown `greeks` level. The unknown-level body is the typed `{error, field}` of ValidationErrorResponse with `field` = `greeks`; a malformed id carries `error` alone."),
        (status = 404, description = "Session not found"),
        (status = 410, description = "Session completed; no current step available"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn get_current_step(
    req: HttpRequest,
    session_manager: web::Data<Arc<SessionManager>>,
    query: web::Query<ChainQuery>,
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

    let level = match GreekLevel::parse(query.greeks.as_deref()) {
        Ok(level) => level,
        Err(error) => return map_error(error),
    };

    // Peek the current snapshot: read-only, repeatable, no state change and no persistence.
    // No simulation-step metric is recorded and no chain-step event is written, because the
    // same step is served repeatedly.
    match session_manager.peek_current_step(session_id).await {
        Ok((session, option_chain)) => {
            match render_chain_responses(session, option_chain, level).await {
                Ok((body, _)) => json_body(body),
                Err(error) => map_error(error),
            }
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
        (status = 409, description = "Concurrent modification: another request advanced or modified the session; retry"),
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
        (status = 409, description = "Concurrent modification: another request advanced or modified the session; retry"),
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
    let session_id = Uuid::parse_str(&query.session_id)
        .map_err(|_| ChainError::InvalidState("Invalid session ID format".to_string()));

    match session_id {
        Ok(id) => {
            let delete_result = session_manager.delete_session(id).await;
            // The delete evicted the cached walk (issue #9); publish the post-eviction
            // cache occupancy so the gauge tracks actual state.
            metrics_collector
                .set_simulation_cache_size(session_manager.simulation_cache_len().await as i64);
            match delete_result {
                Ok(true) => {
                    // Record the active-session metric ONLY when a session was
                    // actually deleted. Invalid ids, not-found (Ok(false)), and
                    // errors record nothing, so repeated DELETEs cannot drive the
                    // active_sessions gauge negative.
                    metrics_collector.record_session_deleted();

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
            }
        }
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

#[cfg(test)]
mod tests_chain_response_greeks {
    use super::*;
    use crate::api::rest::greeks::GreekLevel;
    use crate::session::{SimulationMethod, SimulationParameters};
    use crate::utils::UuidGenerator;
    use optionstratlib::ExpirationDate;
    use optionstratlib::chains::OptionChainBuildParams;
    use optionstratlib::chains::utils::OptionDataPriceParams;
    use optionstratlib::utils::TimeFrame;
    use positive::{Positive, pos_or_panic};
    use rust_decimal_macros::dec;
    use serde_json::Value;

    /// The twelve values of an upstream `GreeksSnapshot`.
    const FULL_SET: [&str; 12] = [
        "delta", "gamma", "theta", "vega", "rho", "rho_d", "alpha", "vanna", "vomma", "veta",
        "charm", "color",
    ];

    /// A session and a chain built from the same parameters, with a non-zero
    /// dividend yield so `rho_d` is a meaningful number rather than a null.
    fn session_and_chain() -> (Session, OptionChain) {
        let parameters = SimulationParameters {
            symbol: "AAPL".to_string(),
            steps: 10,
            initial_price: pos_or_panic!(100.0),
            days_to_expiration: pos_or_panic!(30.0),
            volatility: pos_or_panic!(0.2),
            risk_free_rate: dec!(0.04),
            dividend_yield: pos_or_panic!(0.015),
            method: SimulationMethod::GeometricBrownian {
                dt: pos_or_panic!(0.004),
                drift: dec!(0.0),
                volatility: pos_or_panic!(0.2),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(3),
            strike_interval: Some(pos_or_panic!(5.0)),
            skew_slope: Some(dec!(-0.2)),
            smile_curve: Some(dec!(0.5)),
            spread: Some(pos_or_panic!(0.02)),
            seed: Some(42),
        };

        let price_params = OptionDataPriceParams::new(
            Some(Box::new(parameters.initial_price)),
            Some(ExpirationDate::Days(parameters.days_to_expiration)),
            Some(parameters.risk_free_rate),
            Some(parameters.dividend_yield),
            Some(parameters.symbol.clone()),
        );
        let build_params = OptionChainBuildParams::new(
            parameters.symbol.clone(),
            Some(Positive::ONE),
            3,
            Some(pos_or_panic!(5.0)),
            dec!(-0.2),
            dec!(0.5),
            pos_or_panic!(0.02),
            2,
            price_params,
            parameters.volatility,
        );
        let chain = match OptionChain::build_chain(&build_params) {
            Ok(chain) => chain,
            Err(error) => panic!("the fixture chain must build: {error}"),
        };

        let namespace = match Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8") {
            Ok(namespace) => namespace,
            Err(error) => panic!("the fixture namespace must parse: {error}"),
        };
        let session = Session::new(parameters, &UuidGenerator::new(namespace));
        (session, chain)
    }

    /// Renders a chain response at one level and returns its quotes, both
    /// sides of every strike.
    fn quotes_at(level: GreekLevel) -> Vec<Value> {
        let (session, chain) = session_and_chain();
        let response = build_chain_response(&session, &chain, level);
        let value = match serde_json::to_value(&response) {
            Ok(value) => value,
            Err(error) => panic!("the response must serialize: {error}"),
        };
        let contracts = match value.get("contracts").and_then(Value::as_array) {
            Some(contracts) => contracts.clone(),
            None => panic!("the response must carry contracts: {value}"),
        };
        let mut quotes = Vec::new();
        for contract in &contracts {
            for side in ["call", "put"] {
                match contract.get(side) {
                    Some(quote) => quotes.push(quote.clone()),
                    None => panic!("every contract must carry a {side}: {contract}"),
                }
            }
        }
        assert!(!quotes.is_empty(), "the fixture chain must quote something");
        quotes
    }

    /// The regression that protects every existing v1 client: at the default
    /// level the response carries no `greeks` key at all, so the payload is
    /// what it has always been.
    #[test]
    fn test_the_default_v1_chain_carries_no_greeks_key() {
        for quote in quotes_at(GreekLevel::None) {
            assert!(
                quote.get("greeks").is_none(),
                "the default quote must carry no greeks key: {quote}"
            );
        }
    }

    /// `greeks=all` carries all twelve values for both styles on every strike.
    #[test]
    fn test_the_v1_chain_carries_the_twelve_values_per_style() {
        for quote in quotes_at(GreekLevel::All) {
            let greeks = match quote.get("greeks").and_then(Value::as_object) {
                Some(greeks) => greeks,
                None => panic!("greeks=all must carry a greeks object: {quote}"),
            };
            for key in FULL_SET {
                assert!(
                    greeks.contains_key(key),
                    "greeks=all must carry {key}: {quote}"
                );
            }
            assert_eq!(
                greeks.len(),
                FULL_SET.len(),
                "greeks=all must carry exactly the twelve values: {quote}"
            );
        }
    }

    /// `greeks=first` adds the four the default response lacks, and nothing
    /// else; `delta` stays on the quote itself.
    #[test]
    fn test_the_v1_chain_first_level_carries_only_the_remaining_first_order_set() {
        for quote in quotes_at(GreekLevel::First) {
            let greeks = match quote.get("greeks").and_then(Value::as_object) {
                Some(greeks) => greeks,
                None => panic!("greeks=first must carry a greeks object: {quote}"),
            };
            let mut keys: Vec<&str> = greeks.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, vec!["rho", "rho_d", "theta", "vega"], "{quote}");
            assert!(quote.get("delta").is_some(), "{quote}");
        }
    }

    /// The per-style split is real: `charm` differs between the call and the
    /// put, and `rho` carries opposite signs.
    #[test]
    fn test_the_v1_call_and_put_greeks_are_genuinely_different() {
        let (session, chain) = session_and_chain();
        let response = build_chain_response(&session, &chain, GreekLevel::All);
        let value = match serde_json::to_value(&response) {
            Ok(value) => value,
            Err(error) => panic!("the response must serialize: {error}"),
        };
        let contracts = match value.get("contracts").and_then(Value::as_array) {
            Some(contracts) => contracts,
            None => panic!("the response must carry contracts: {value}"),
        };

        let greek = |contract: &Value, side: &str, name: &str| -> f64 {
            let raw = contract
                .get(side)
                .and_then(|quote| quote.get("greeks"))
                .and_then(|greeks| greeks.get(name))
                .and_then(Value::as_f64);
            match raw {
                Some(value) => value,
                None => panic!("{side}.{name} must be present: {contract}"),
            }
        };

        for contract in contracts {
            assert_ne!(
                greek(contract, "call", "charm"),
                greek(contract, "put", "charm"),
                "charm must differ between the styles: {contract}"
            );
            let call_rho = greek(contract, "call", "rho");
            let put_rho = greek(contract, "put", "rho");
            assert!(
                call_rho * put_rho < 0.0,
                "rho must carry opposite signs, got {call_rho} and {put_rho}"
            );
        }
    }

    /// The level changes only what is added: prices, delta and gamma are the
    /// same numbers at all three levels, because the greeks are read off the
    /// option and never fed back into it.
    #[test]
    fn test_the_v1_greek_level_does_not_move_the_quoted_market() {
        let strip = |level: GreekLevel| -> Vec<Value> {
            quotes_at(level)
                .into_iter()
                .map(|quote| {
                    let mut quote = quote;
                    if let Some(object) = quote.as_object_mut() {
                        object.remove("greeks");
                    }
                    quote
                })
                .collect()
        };

        let none = strip(GreekLevel::None);
        assert_eq!(strip(GreekLevel::First), none);
        assert_eq!(strip(GreekLevel::All), none);
    }
}

/// HTTP-level coverage of the `greeks` parameter on the v1 surface.
///
/// The conversion tests above call `build_chain_response` directly, which
/// cannot exercise the query extractor, `map_error`, or the ordering of the
/// level parse against the store call. v1 is the surface IronCondor is on, so
/// its rejection path is worth proving through the real routes.
#[cfg(test)]
mod tests_v1_greeks_over_http {
    use super::*;
    use crate::session::InMemorySessionStore;
    use actix_web::App;
    use actix_web::http::StatusCode;
    use actix_web::test as actix_test;
    use serde_json::Value;

    /// Mounts the real peek route over an in-memory store.
    macro_rules! peek_service {
        () => {{
            let manager = Arc::new(SessionManager::new(Arc::new(InMemorySessionStore::new())));
            actix_test::init_service(
                App::new()
                    .app_data(web::Data::new(manager))
                    .service(web::resource("/api/v1/chain").route(web::get().to(get_current_step))),
            )
            .await
        }};
    }

    /// An unknown level is a typed 400 naming the field.
    ///
    /// The id below is well formed but belongs to no session, and the response
    /// is still a `400` rather than a `404`: the level is resolved before the
    /// store is touched, which is the ordering the step endpoint depends on.
    #[actix_web::test]
    async fn test_an_unknown_greek_level_is_a_typed_400_on_v1() {
        let app = peek_service!();

        for level in ["second", "ALL", ""] {
            let uri = format!(
                "/api/v1/chain?sessionid=6ba7b810-9dad-11d1-80b4-00c04fd430c8&greeks={level}"
            );
            let request = actix_test::TestRequest::get().uri(&uri).to_request();
            let response = actix_test::call_service(&app, request).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "greeks={level} must be rejected before the store is read"
            );
            let body: Value = actix_test::read_body_json(response).await;
            assert_eq!(
                body.get("field").and_then(Value::as_str),
                Some("greeks"),
                "the 400 must name the field: {body}"
            );
        }
    }

    /// A rejected level must not consume a step.
    ///
    /// The v2 half of this is hermetic; v1's advance handler extracts a
    /// `MongoDBRepository`, and actix resolves every extractor before the
    /// handler runs, so this one needs the live service CI provides.
    #[actix_web::test]
    #[ignore = "requires live MongoDB on localhost:27017; run with -- --ignored"]
    async fn test_an_unknown_greek_level_on_v1_step_does_not_advance() {
        use crate::infrastructure::{MetricsCollector, init_mongodb};
        use crate::session::{SimulationMethod, SimulationParameters};
        use optionstratlib::utils::TimeFrame;
        use positive::pos_or_panic;
        use rust_decimal_macros::dec;

        let store = Arc::new(InMemorySessionStore::new());
        let manager = Arc::new(SessionManager::new(store));
        let parameters = SimulationParameters {
            symbol: "AAPL".to_string(),
            steps: 10,
            initial_price: pos_or_panic!(100.0),
            days_to_expiration: pos_or_panic!(30.0),
            volatility: pos_or_panic!(0.2),
            risk_free_rate: dec!(0.04),
            dividend_yield: pos_or_panic!(0.015),
            method: SimulationMethod::GeometricBrownian {
                dt: pos_or_panic!(0.004),
                drift: dec!(0.0),
                volatility: pos_or_panic!(0.2),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(3),
            strike_interval: Some(pos_or_panic!(5.0)),
            skew_slope: Some(dec!(-0.2)),
            smile_curve: Some(dec!(0.5)),
            spread: Some(pos_or_panic!(0.02)),
            seed: Some(42),
        };
        let session = match manager.create_session(parameters).await {
            Ok(session) => session,
            Err(error) => panic!("the fixture session must be created: {error}"),
        };
        let session_id = session.id;

        let metrics = match MetricsCollector::new() {
            Ok(metrics) => Arc::new(metrics),
            Err(error) => panic!("the metrics collector must build: {error}"),
        };
        let mongo = match init_mongodb().await {
            Ok(repository) => repository,
            Err(error) => panic!("this test needs a live MongoDB: {error}"),
        };

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(manager.clone()))
                .app_data(web::Data::new(metrics))
                .app_data(web::Data::new(mongo))
                .service(web::resource("/api/v1/chain/step").route(web::post().to(advance_step))),
        )
        .await;

        let request = actix_test::TestRequest::post()
            .uri(&format!(
                "/api/v1/chain/step?sessionid={session_id}&greeks=second"
            ))
            .to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = actix_test::read_body_json(response).await;
        assert_eq!(body.get("field").and_then(Value::as_str), Some("greeks"));

        match manager.get_session(session_id).await {
            Ok(session) => assert_eq!(
                session.current_step, 0,
                "a rejected level must not consume a step"
            ),
            Err(error) => panic!("the session must still exist: {error}"),
        }
    }

    /// A known level reaches the store, so the parameter is not swallowing
    /// well-formed requests: an unknown session is a `404`, not a `400`.
    #[actix_web::test]
    async fn test_a_known_greek_level_reaches_the_store_on_v1() {
        let app = peek_service!();

        for level in ["", "?", "&greeks=none", "&greeks=first", "&greeks=all"] {
            let query = match level {
                "" | "?" => String::new(),
                other => other.to_string(),
            };
            let uri =
                format!("/api/v1/chain?sessionid=6ba7b810-9dad-11d1-80b4-00c04fd430c8{query}");
            let request = actix_test::TestRequest::get().uri(&uri).to_request();
            let response = actix_test::call_service(&app, request).await;
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "a valid level must reach the store, for {uri}"
            );
        }
    }
}
