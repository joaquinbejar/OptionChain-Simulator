use crate::api::rest::error::map_error;
use crate::api::rest::limits::{MAX_CHAIN_SIZE, MAX_STEPS};
use crate::api::rest::models::SessionId;
use crate::api::rest::requests::{CreateSessionRequest, UpdateSessionRequest};
use crate::api::rest::responses::{
    ChainResponse, ErrorResponse, OptionContractResponse, OptionPriceResponse, SessionInfoResponse,
    SessionParametersResponse, SessionResponse,
};
use crate::api::rest::validation::{self, decimal_field, positive_field, strictly_positive_field};
use crate::infrastructure::{MetricsCollector, MongoDBRepository};
use crate::session::{SessionManager, SimulationParameters};
use crate::utils::ChainError;
use actix_web::{HttpRequest, HttpResponse, Responder, web};
use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use std::sync::Arc;
use tracing::{error, info};
use uuid::Uuid;

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
        (status = 400, description = "Validation failed: a parameter was non-finite, out of range (e.g. negative price/volatility), or steps/chain_size exceeded the configured limits. The body includes an `error` message and the offending `field`.", body = ErrorResponse),
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
    match session_manager.create_session(simulation_params) {
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

#[utoipa::path(
    get,
    path = "/api/v1/chain",
    params(
        ("sessionid" = String, Query, description = "ID of the session to get next step for")
    ),
    responses(
        (status = 200, description = "Next step returned", body = ChainResponse),
        (status = 404, description = "Session not found"),
        (status = 410, description = "Simulation completed. No more steps available"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn get_next_step(
    req: HttpRequest,
    session_manager: web::Data<Arc<SessionManager>>,
    metrics_collector: web::Data<Arc<MetricsCollector>>,
    mongodb_repo: web::Data<Arc<MongoDBRepository>>,
    query: web::Query<SessionId>,
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

    // Get next step from session manager
    match session_manager.get_next_step(session_id).await {
        Ok((session, option_chain)) => {
            // Convert session and option chain to ChainResponse
            let expiration = option_chain.get_expiration_date();
            let response = ChainResponse {
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
                                delta: call_delta.map(|d| d.to_f64().unwrap()),
                            },
                            put: OptionPriceResponse {
                                bid: put_bid.map(|b| b.into()),
                                ask: put_ask.map(|a| a.into()),
                                mid: contract.put_middle.map(|m| m.into()),
                                delta: put_delta.map(|d| d.to_f64().unwrap()),
                            },
                            implied_volatility: Some(volatility.into()),
                            gamma: contract.current_gamma().map(|g| g.to_f64().unwrap()),
                        }
                    })
                    .collect(),
                session_info: SessionInfoResponse {
                    id: session.id.to_string(),
                    current_step: session.current_step,
                    total_steps: session.total_steps,
                },
            };
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
        (status = 400, description = "Validation failed: a parameter was non-finite, out of range, or exceeded the configured limits. The body includes an `error` message and the offending `field`."),
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
    match session_manager.reinitialize_session(session_id, simulation_params) {
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
    responses(
        (status = 200, description = "Session updated", body = SessionResponse),
        (status = 404, description = "Session not found"),
        (status = 400, description = "Validation failed: a supplied parameter was non-finite, out of range, or exceeded the configured limits. The body includes an `error` message and the offending `field`."),
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
    let current_session = match session_manager.get_session(session_id) {
        Ok(session) => session,
        Err(error) => return map_error(error),
    };

    // Create a new SimulationParameters object with updated values
    let mut updated_params = current_session.parameters.clone();

    // Update only the fields that are provided in the request. Every user-supplied
    // numeric is validated with the same helpers as the create/replace conversions, so a
    // bad float yields a 400 instead of panicking during the PATCH merge.
    if let Some(symbol) = &json_req.symbol {
        updated_params.symbol = symbol.clone();
    }

    if let Some(steps) = json_req.steps {
        if steps < 1 {
            return map_error(ChainError::Validation {
                field: "steps".to_string(),
                reason: "must be at least 1".to_string(),
            });
        }
        if steps > *MAX_STEPS {
            return map_error(ChainError::Validation {
                field: "steps".to_string(),
                reason: format!("must not exceed {}, got {}", *MAX_STEPS, steps),
            });
        }
        updated_params.steps = steps;
    }

    if let Some(initial_price) = json_req.initial_price {
        updated_params.initial_price = match positive_field("initial_price", initial_price) {
            Ok(value) => value,
            Err(error) => return map_error(error),
        };
    }

    if let Some(days_to_expiration) = json_req.days_to_expiration {
        updated_params.days_to_expiration =
            match positive_field("days_to_expiration", days_to_expiration) {
                Ok(value) => value,
                Err(error) => return map_error(error),
            };
    }

    if let Some(volatility) = json_req.volatility {
        updated_params.volatility = match positive_field("volatility", volatility) {
            Ok(value) => value,
            Err(error) => return map_error(error),
        };
    }

    if let Some(risk_free_rate) = json_req.risk_free_rate {
        updated_params.risk_free_rate = match decimal_field("risk_free_rate", risk_free_rate) {
            Ok(value) => value,
            Err(error) => return map_error(error),
        };
    }

    if let Some(dividend_yield) = json_req.dividend_yield {
        updated_params.dividend_yield = match positive_field("dividend_yield", dividend_yield) {
            Ok(value) => value,
            Err(error) => return map_error(error),
        };
    }

    if let Some(method) = &json_req.method {
        updated_params.method = match method.clone().try_into() {
            Ok(value) => value,
            Err(error) => return map_error(error),
        };
    }

    if let Some(time_frame) = json_req.time_frame {
        updated_params.time_frame = match validation::time_frame_field("time_frame", time_frame) {
            Ok(value) => value,
            Err(error) => return map_error(error),
        };
    }

    if let Some(chain_size) = json_req.chain_size {
        if chain_size > *MAX_CHAIN_SIZE {
            return map_error(ChainError::Validation {
                field: "chain_size".to_string(),
                reason: format!("must not exceed {}, got {}", *MAX_CHAIN_SIZE, chain_size),
            });
        }
        updated_params.chain_size = Some(chain_size);
    }

    if let Some(strike_interval) = json_req.strike_interval {
        updated_params.strike_interval =
            match strictly_positive_field("strike_interval", strike_interval) {
                Ok(value) => Some(value),
                Err(error) => return map_error(error),
            };
    }

    if let Some(smile_curve) = json_req.smile_curve {
        updated_params.smile_curve = match decimal_field("smile_curve", smile_curve) {
            Ok(value) => Some(value),
            Err(error) => return map_error(error),
        };
    }

    if let Some(spread) = json_req.spread {
        updated_params.spread = match positive_field("spread", spread) {
            Ok(value) => Some(value),
            Err(error) => return map_error(error),
        };
    }

    if let Some(seed) = json_req.seed {
        updated_params.seed = Some(seed);
    }

    // Update the session with new parameters
    match session_manager.update_session(session_id, updated_params) {
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
        Ok(id) => match session_manager.delete_session(id) {
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
