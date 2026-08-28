use crate::api::rest::export::export_simulation;
use crate::api::rest::get_favicon;
use crate::api::rest::handlers::{
    advance_step, create_session, delete_session, get_current_step, replace_session, update_session,
};
use crate::api::rest::handlers_v2::{
    advance_simulation, create_simulation, delete_simulation, get_simulation, json_error_handler,
    peek_snapshot, query_error_handler,
};
use crate::api::rest::middleware::metrics_endpoint;
use crate::api::rest::swagger::ApiDoc;
use crate::infrastructure::{MetricsCollector, MongoDBRepository, SimulationSnapshotRepository};
use crate::session::{SessionManager, SimulationManager};
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
/// * `snapshots` - The v2 snapshot warehouse, when the operator enabled persistence. `None` is the
///   normal case and leaves the v2 export replaying every step.
///
/// # Endpoints
///
/// - **POST** `/api/v1/chain`  
///   Handled by the `create_session` function. This is used to create a new session.
///
/// - **GET** `/api/v1/chain`
///   Handled by the `get_current_step` function. This is a safe, repeatable peek that
///   returns the session's current snapshot WITHOUT advancing or persisting it.
///
/// - **POST** `/api/v1/chain/step`
///   Handled by the `advance_step` function. This advances the session one step, serves
///   the resulting snapshot, and persists the advance (the former GET behavior).
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
    simulation_manager: Arc<SimulationManager>,
    metrics_collector: Arc<MetricsCollector>,
    mongodb_repo: Arc<MongoDBRepository>,
) {
    // The export reads from the same warehouse the manager files into, taken
    // off the manager rather than threaded separately: two handles could be
    // configured differently, and there is only ever one warehouse.
    let snapshots = simulation_manager.warehouse();
    configure_v2_routes(cfg, simulation_manager, snapshots);

    cfg.app_data(web::Data::new(session_manager))
        .app_data(web::Data::new(metrics_collector.clone()))
        .app_data(web::Data::new(mongodb_repo))
        // v1's query DTOs reject unknown keys too, for the same reason: a
        // misspelled `greeks` must not read as the default level. The rejection
        // is rendered in the `{error, field}` shape v1 already documents.
        .app_data(web::QueryConfig::default().error_handler(query_error_handler))
        .service(
            web::resource("/api/v1/chain")
                .route(web::post().to(create_session))
                .route(web::get().to(get_current_step))
                .route(web::put().to(replace_session))
                .route(web::patch().to(update_session))
                .route(web::delete().to(delete_session)),
        )
        .service(web::resource("/api/v1/chain/step").route(web::post().to(advance_step)))
        .route("/metrics", web::get().to(metrics_endpoint))
        .route("/favicon.ico", web::get().to(get_favicon))
        .service(
            SwaggerUi::new("/swagger-ui/{_:.*}").url("/api-docs/openapi.json", ApiDoc::openapi()),
        );
}

/// Registers everything under `/api/v2/simulations`.
///
/// Split out of [`configure_routes`] so the v2 surface can be mounted on its
/// own — which is what its tests do, exercising the real paths and the real
/// JSON error handler without dragging in v1's MongoDB and metrics
/// dependencies. A route string tested here is the same string served in
/// production, rather than a copy that can drift.
///
/// # Endpoints
///
/// - **POST** `/api/v2/simulations` — create a rolling simulation.
/// - **GET** `/api/v2/simulations/{id}` — read its metadata and effective
///   parameters.
/// - **GET** `/api/v2/simulations/{id}/snapshot` — a safe, repeatable peek at
///   the current cursor.
/// - **POST** `/api/v2/simulations/{id}/step` — serve the current snapshot and
///   advance once, with an optional `expected_step` precondition.
/// - **DELETE** `/api/v2/simulations/{id}` — delete it and evict its caches.
/// - **GET** `/api/v2/simulations/{id}/export` — stream the complete tape, or a
///   step range of it, as JSON or CSV.
///
/// `snapshots` is the warehouse the export reads persisted steps from. It is an
/// `Option` because persistence is opt-in: registered, the export prefers a
/// stored snapshot over replaying it; absent, nothing about the export changes.
/// The handler extracts it as `Option<web::Data<_>>`, so a deployment without
/// ClickHouse registers nothing rather than a null.
pub(crate) fn configure_v2_routes(
    cfg: &mut web::ServiceConfig,
    simulation_manager: Arc<SimulationManager>,
    snapshots: Option<Arc<dyn SimulationSnapshotRepository>>,
) {
    if let Some(snapshots) = snapshots {
        cfg.app_data(web::Data::new(snapshots));
    }

    cfg.app_data(web::Data::new(simulation_manager))
        // A rejected v2 body must come back as the documented `{error, field}`
        // shape. Rule-level failures are raised *inside* deserialization, so
        // without this handler actix renders them as plaintext and the
        // structured field is lost.
        .app_data(web::JsonConfig::default().error_handler(json_error_handler))
        // Same treatment for the query string. The v2 query DTOs reject unknown
        // keys, so a misspelled `?greek=all` is an error rather than a silent
        // downgrade to the default level; without this it would surface as
        // actix's untyped plaintext instead of the documented shape.
        .app_data(web::QueryConfig::default().error_handler(query_error_handler))
        .service(web::resource("/api/v2/simulations").route(web::post().to(create_simulation)))
        .service(
            web::resource("/api/v2/simulations/{id}")
                .route(web::get().to(get_simulation))
                .route(web::delete().to(delete_simulation)),
        )
        .service(
            web::resource("/api/v2/simulations/{id}/snapshot").route(web::get().to(peek_snapshot)),
        )
        .service(
            web::resource("/api/v2/simulations/{id}/step")
                .route(web::post().to(advance_simulation)),
        )
        .service(
            web::resource("/api/v2/simulations/{id}/export")
                .route(web::get().to(export_simulation)),
        );
}
