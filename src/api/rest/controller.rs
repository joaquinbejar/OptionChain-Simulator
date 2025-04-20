use crate::api::rest::handlers::{
    create_session, delete_session, get_next_step, replace_session, update_session,
};
use crate::api::rest::models::ListenOn;
use crate::session::SessionManager;
use actix_web::{App, HttpServer, web};
use std::sync::Arc;
use tracing::info;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;
use crate::api::rest::swagger::ApiDoc;

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
pub fn configure_routes(cfg: &mut web::ServiceConfig, session_manager: Arc<SessionManager>) {
    cfg.app_data(web::Data::new(session_manager)).service(
        web::resource("/api/v1/chain")
            .route(web::post().to(create_session))
            .route(web::get().to(get_next_step))
            .route(web::put().to(replace_session))
            .route(web::patch().to(update_session))
            .route(web::delete().to(delete_session)),
    );
        // .service(SwaggerUi::new("/swagger-ui/{_:.*}").url("/api-docs/openapi.json", ApiDoc::openapi()));
    
}

/// Starts the HTTP server with the specified configuration
///
/// # Arguments
///
/// * `session_manager` - The session manager instance that will be shared with all HTTP workers
/// * `listen_on` - Determines which network interface to bind to
/// * `port` - The port number to listen on
///
/// # Returns
///
/// A Result containing the server instance if successful, or an IO error if binding fails
///
pub async fn start_server(
    session_manager: Arc<SessionManager>,
    listen_on: ListenOn,
    port: u16,
) -> std::io::Result<()> {
    let bind_address = format!("{}:{}", listen_on, port);

    info!("Starting server on {}", bind_address);

    HttpServer::new(move || {
        App::new().configure(|cfg| configure_routes(cfg, session_manager.clone()))
    })
    .bind(bind_address)?
    .run()
    .await
}
