use crate::api::rest::get_favicon;
use crate::api::rest::handlers::{
    create_session, delete_session, get_next_step, replace_session, update_session,
};
use crate::api::rest::middleware::metrics_endpoint;
use crate::api::rest::swagger::ApiDoc;
use crate::infrastructure::MetricsCollector;
use crate::session::SessionManager;
use actix_web::web;
use std::sync::Arc;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

/// Configures the routes for the web application under the "/api/v1/chain" endpoint.
///
/// This function sets up the necessary routing and handler configuration for managing
/// session-related operations. It registers multiple HTTP methods (POST, GET, PUT, PATCH, DELETE)
/// to their corresponding handler functions for the "/api/v1/chain" resource.
///
/// # Arguments
///
/// * `cfg` - A mutable reference to the Actix Web `ServiceConfig`. This object is used to configure
///   the application's services and routes.
/// * `session_manager` - An `Arc` instance of `SessionManager`. This is wrapped in `web::Data`
///   to make it accessible to the route handlers. The `SessionManager` is responsible for managing
///   session data and operations.
///
/// # Endpoints
///
/// - **POST** `/api/v1/chain`  
///   Handled by the `create_session` function. This is used to create a new session.
///
/// - **GET** `/api/v1/chain`  
///   Handled by the `get_next_step` function. This is used to fetch the next step of the session.
///
/// - **PUT** `/api/v1/chain`  
///   Handled by the `replace_session` function. This is used to replace an existing session with new data.
///
/// - **PATCH** `/api/v1/chain`  
///   Handled by the `update_session` function. This is used to update parts of an existing session.
///
/// - **DELETE** `/api/v1/chain`  
///   Handled by the `delete_session` function. This is used to remove an existing session.
///
/// # Usage
///
/// This function should be called during the setup phase of the Actix Web application to configure
/// session management routes. The `SessionManager` must be wrapped in an `Arc` to ensure thread-safe
/// sharing of session data.
///
pub fn configure_routes(
    cfg: &mut web::ServiceConfig,
    session_manager: Arc<SessionManager>,
    metrics_collector: Arc<MetricsCollector>,
) {
    cfg.app_data(web::Data::new(session_manager))
        .app_data(web::Data::new(metrics_collector.clone()))
        .service(
            web::resource("/api/v1/chain")
                .route(web::post().to(create_session))
                .route(web::get().to(get_next_step))
                .route(web::put().to(replace_session))
                .route(web::patch().to(update_session))
                .route(web::delete().to(delete_session)),
        )
        .route("/metrics", web::get().to(metrics_endpoint))
        .route("/favicon.ico", web::get().to(get_favicon))
        .service(
            SwaggerUi::new("/swagger-ui/{_:.*}").url("/api-docs/openapi.json", ApiDoc::openapi()),
        );
}
