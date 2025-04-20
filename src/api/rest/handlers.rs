use crate::api::rest::models::SessionId;
use crate::api::rest::requests::{CreateSessionRequest, UpdateSessionRequest};
use crate::api::rest::responses::{ErrorResponse, SessionParametersResponse, SessionResponse};
use crate::session::{SessionManager, SimulationParameters};
use crate::utils::ChainError;
use actix_web::{HttpResponse, Responder, web};
use std::sync::Arc;
use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use tracing::error;
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
                      "skew_factor": 0.0005,
                      "spread": 0.02
                    }
                    "#
    ),
    responses(
        (status = 201, description = "Session created successfully", body = SessionResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn create_session(
    session_manager: web::Data<Arc<SessionManager>>,
    req: web::Json<CreateSessionRequest>,
) -> impl Responder {
    // Convert request to domain SimulationParameters
    let simulation_params = match SimulationParameters::try_from(req.0) {
        Ok(params) => params,
        Err(error) => {
            return map_error(ChainError::InvalidState(error.to_string()));
        }
    };

    // Create session using session manager
    match session_manager.create_session(simulation_params) {
        Ok(session) => {
            let created_at_utc = DateTime::<Utc>::from(session.created_at);
            let updated_at_utc = DateTime::<Utc>::from(session.updated_at);
            let response = SessionResponse {
                id: session.id.to_string(),
                created_at: created_at_utc.to_rfc3339(),
                updated_at: updated_at_utc.to_rfc3339(),
                parameters: SessionParametersResponse {
                    symbol: session.parameters.symbol,
                    initial_price: session.parameters.initial_price.into(),
                    volatility: session.parameters.volatility.into(),
                    risk_free_rate: session.parameters.risk_free_rate.to_f64().unwrap(),
                    method: format!("{:?}", session.parameters.method),
                    time_frame: session.parameters.time_frame.to_string(),
                    dividend_yield: session.parameters.dividend_yield.into(),
                    skew_factor: session.parameters.skew_factor.map(|f| f.to_f64().unwrap()),
                    spread: session.parameters.spread.map(|f| f.into()),
                },
                current_step: session.current_step,
                total_steps: session.total_steps,
                state: session.state.to_string(),
            };

            HttpResponse::Created().json(response)
        }
        Err(error) => map_error(error)
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/chain",
    params(
        ("sessionid" = String, Query, description = "ID of the session to get next step for")
    ),
    responses(
        (status = 200, description = "Next step returned", body = String),
        (status = 404, description = "Session not found")
    )
)]
pub(crate) async fn get_next_step(
    _session_manager: web::Data<Arc<SessionManager>>,
    query: web::Query<SessionId>,
) -> impl Responder {
    let session_id = &query.session_id;
    let msg = format!("Next step for session ID: {} returned", session_id);
    HttpResponse::Ok().body(msg)
}

#[utoipa::path(
    put,
    path = "/api/v1/chain",
    params(
        ("sessionid" = String, Query, description = "ID of the session to replace")
    ),
    responses(
        (status = 200, description = "Session replaced", body = String),
        (status = 404, description = "Session not found")
    )
)]
pub(crate) async fn replace_session(
    _session_manager: web::Data<Arc<SessionManager>>,
    query: web::Query<SessionId>,
    _req: web::Json<CreateSessionRequest>,
) -> impl Responder {
    let session_id = &query.session_id;
    let msg = format!("Session replaced ID: {}", session_id);
    HttpResponse::Ok().body(msg)
}

#[utoipa::path(
    patch,
    path = "/api/v1/chain",
    params(
        ("sessionid" = String, Query, description = "ID of the session to update")
    ),
    responses(
        (status = 200, description = "Session updated", body = String),
        (status = 404, description = "Session not found")
    )
)]
pub(crate) async fn update_session(
    _session_manager: web::Data<Arc<SessionManager>>,
    query: web::Query<SessionId>,
    _req: web::Json<UpdateSessionRequest>,
) -> impl Responder {
    let session_id = &query.session_id;
    let msg = format!("Session updated ID: {}", session_id);
    HttpResponse::Ok().body(msg)
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
    session_manager: web::Data<Arc<SessionManager>>,
    query: web::Query<SessionId>,
) -> impl Responder {
    let session_id = Uuid::parse_str(&query.session_id).map_err(|_| {
        ChainError::InvalidState("Invalid session ID format".to_string())
    });

    match session_id {
        Ok(id) => {
            match session_manager.delete_session(id) {
                Ok(true) => {
                    let msg = format!("Session deleted successfully: {}", id);
                    HttpResponse::Ok().json(serde_json::json!({
                        "message": msg,
                        "session_id": id.to_string()
                    }))
                }
                Ok(false) => {
                    HttpResponse::NotFound().json(serde_json::json!({
                        "error": format!("Session not found: {}", id)
                    }))
                }
                Err(chain_error) => {
                    error!( "{} {}", id, chain_error );
                    map_error(chain_error)
                }
            }
        }
        Err(error) => {
            error!( "{}",  error );
            map_error(error)
        }
    }
}

fn map_error(error: ChainError) -> HttpResponse {
    match error {
        ChainError::NotFound(_) => {
            HttpResponse::NotFound().json(serde_json::json!({"error": error.to_string()}))
        }
        ChainError::InvalidState(_) => {
            HttpResponse::BadRequest().json(serde_json::json!({"error": error.to_string()}))
        }
        _ => HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "Internal server error".to_string()})),
    }
}
