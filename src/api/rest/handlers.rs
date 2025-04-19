use std::sync::Arc;
use actix_web::{web, HttpResponse, Responder};
use uuid::Uuid;
use crate::api::rest::requests::CreateSessionRequest;
use crate::api::rest::responses::{SessionParametersResponse, SessionResponse};
use crate::session::SessionManager;
use crate::utils::ChainError;

pub(crate) async fn create_session(
    session_manager: web::Data<Arc<SessionManager>>,
    req: web::Json<CreateSessionRequest>
) -> impl Responder {
    // In real implementation, convert from request to domain model
    // For now we return a mock response
    HttpResponse::Created()
        .json(SessionResponse {
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



pub(crate) async fn get_next_step(
    session_manager: web::Data<Arc<SessionManager>>,
    req: web::Path<String>,
) -> impl Responder {
    // Implementation would get next simulation step
    // Mock response for now
    HttpResponse::Ok().body("Next step data would be returned here")
}

pub(crate) async fn replace_session(
    session_manager: web::Data<Arc<SessionManager>>,
    session_id: web::Path<String>,
    req: web::Json<CreateSessionRequest>,
) -> impl Responder {
    // Implementation would replace all session parameters
    HttpResponse::Ok().body("Session replaced")
}

pub(crate) async fn update_session(
    session_manager: web::Data<Arc<SessionManager>>,
    session_id: web::Path<String>,
    req: web::Json<serde_json::Value>,
) -> impl Responder {
    // Implementation would patch specific session parameters
    HttpResponse::Ok().body("Session updated")
}

pub(crate) async fn delete_session(
    session_manager: web::Data<Arc<SessionManager>>,
    session_id: web::Path<String>,
) -> impl Responder {
    // Implementation would delete the session
    HttpResponse::Ok().body("Session deleted")
}

fn map_error(error: ChainError) -> HttpResponse {
    match error {
        ChainError::NotFound(_) => {
            HttpResponse::NotFound().json(serde_json::json!({"error": error.to_string()}))
        }
        ChainError::InvalidState(_) => {
            HttpResponse::BadRequest().json(serde_json::json!({"error": error.to_string()}))
        }
        _ => HttpResponse::InternalServerError().json(
            serde_json::json!({"error": "Internal server error".to_string()})
        )

    }
}