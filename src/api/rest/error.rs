use crate::utils::ChainError;
use actix_web::HttpResponse;

pub(crate) fn map_error(error: ChainError) -> HttpResponse {
    match error {
        ChainError::NotFound(_) => {
            HttpResponse::NotFound().json(serde_json::json!({"error": error.to_string()}))
        }
        ChainError::InvalidState(_) => {
            HttpResponse::BadRequest().json(serde_json::json!({"error": error.to_string()}))
        }
        ChainError::SimulatorError(err) => {
            HttpResponse::Gone().json(serde_json::json!({"error": err}))
        }
        _ => HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "Internal server error".to_string()})),
    }
}

/// Tests for the `map_error` function.
#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::body::to_bytes;
    use actix_web::http::StatusCode;
    use serde_json::Value;

    /// NotFound should map to 404 with the error message from `Display`.
    #[actix_web::test]
    async fn test_map_error_not_found() {
        let err = ChainError::NotFound("item_xyz".into());
        let resp: HttpResponse = map_error(err.clone());
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let body = to_bytes(resp.into_body()).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({"error": err.to_string()}));
    }

    /// InvalidState should map to 400 with the error message from `Display`.
    #[actix_web::test]
    async fn test_map_error_invalid_state() {
        let err = ChainError::InvalidState("bad_state".into());
        let resp: HttpResponse = map_error(err.clone());
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(resp.into_body()).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({"error": err.to_string()}));
    }

    /// SimulatorError should map to 410 with the raw inner string.
    #[actix_web::test]
    async fn test_map_error_simulator_error() {
        let err = ChainError::SimulatorError("sim_fail".into());
        let resp: HttpResponse = map_error(err.clone());
        assert_eq!(resp.status(), StatusCode::GONE);

        let body = to_bytes(resp.into_body()).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({"error": "sim_fail"}));
    }

    /// Any other variant (e.g., SessionError) should map to 500 with a generic message.
    #[actix_web::test]
    async fn test_map_error_default() {
        let err = ChainError::SessionError("oops".into());
        let resp: HttpResponse = map_error(err);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = to_bytes(resp.into_body()).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({"error": "Internal server error"}));
    }
}
