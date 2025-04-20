use crate::api::rest::models::SessionId;
use crate::api::rest::requests::{CreateSessionRequest, UpdateSessionRequest};
use crate::api::rest::responses::{ErrorResponse, SessionParametersResponse, SessionResponse};
use crate::session::SessionManager;
use crate::utils::ChainError;
use actix_web::{HttpResponse, Responder, web};
use std::sync::Arc;
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
        (status = 200, description = "Session created successfully", body = SessionResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse)
    )
)]
pub(crate) async fn create_session(
    _session_manager: web::Data<Arc<SessionManager>>,
    _req: web::Json<CreateSessionRequest>,
) -> impl Responder {
    // In real implementation, convert from request to domain model
    // For now we return a mock response
    HttpResponse::Created().json(SessionResponse {
        id: Uuid::new_v4().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        parameters: SessionParametersResponse {
            symbol: "".to_string(),
            initial_price: 0.0,
            volatility: 0.0,
            risk_free_rate: 0.0,
            strikes: vec![],
            expirations: vec![],
            method: "".to_string(),
            time_frame: "".to_string(),
            dividend_yield: 0.0,
            skew_factor: None,
            spread: None,
        },
        current_step: 0,
        total_steps: 20,
        state: "Initialized".to_string(),
    })
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
        (status = 404, description = "Session not found")
    )
)]
pub(crate) async fn delete_session(
    _session_manager: web::Data<Arc<SessionManager>>,
    query: web::Query<SessionId>,
) -> impl Responder {
    let session_id = &query.session_id;
    let msg = format!("Session deleted ID: {}", session_id);
    HttpResponse::Ok().body(msg)
}

#[allow(dead_code)] // TODO: remove as soon as we have proper error handling
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
