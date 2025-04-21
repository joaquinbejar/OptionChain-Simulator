use actix_web::HttpResponse;
use crate::utils::ChainError;

pub(crate) fn map_error(error: ChainError) -> HttpResponse {
    match error {
        ChainError::NotFound(_) => {
            HttpResponse::NotFound().json(serde_json::json!({"error": error.to_string()}))
        },
        ChainError::InvalidState(_) => {
            HttpResponse::BadRequest().json(serde_json::json!({"error": error.to_string()}))
        },
        ChainError::SimulatorError(err) => {
            HttpResponse::Gone().json(serde_json::json!({"error": err}))
        },
        _ => HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "Internal server error".to_string()})),
    }
}