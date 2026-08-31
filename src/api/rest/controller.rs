use crate::session::{SessionManager, SimulationManager};
use actix_web::{App, HttpServer};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;

use crate::api::rest::health::PROBE_PATHS;
use crate::api::rest::instance::InstanceHeader;
use crate::api::rest::routes::configure_routes;
use crate::infrastructure::{
    ListenOn, MetricsCollector, MetricsMiddleware, MongoDBRepository, Readiness,
};

/// Starts an HTTP server with the given configuration.
///
/// # Arguments
///
/// * `session_manager` - A shared reference to the `SessionManager`, used to manage user sessions.
/// * `metrics_collector` - A shared reference to the `MetricsCollector`, used for collecting server metrics.
/// * `snapshots` - The v2 snapshot warehouse, when snapshot persistence is enabled. It is the same
///   repository the manager files snapshots into, shared so the v2 export can read them back and
///   prefer a persisted step over replaying it. `None` leaves the export replaying every step.
/// * `readiness` - The dependency probes `GET /ready` runs.
/// * `listen_on` - The address or hostname where the server will listen, typically an IP address or hostname.
/// * `port` - The port number on which the server will accept requests.
///
/// # Returns
///
/// Returns a `std::io::Result<()>` that resolves when the server stops running or if an
/// error occurs when binding or running the HTTP server.
///
/// # Details
///
/// This function initializes and starts an Actix Web server:
/// - The server is configured with a custom `MetricsMiddleware` for metrics collection.
/// - Routes are dynamically configured using the `configure_routes` function, which is provided with
///   references to the `SessionManager` and `MetricsCollector`.
/// - The server binds to the provided `listen_on` address and `port`, constructing the bind address
///   in the format `"address:port"`.
///
/// The server starts asynchronously and will remain active, awaiting requests, until it is stopped
/// or encounters an error.
///
/// # Errors
///
/// This function will return an `Err` result if:
/// - The server fails to bind to the specified address and port.
/// - An error occurs while attempting to run the server.
pub async fn start_server(
    session_manager: Arc<SessionManager>,
    simulation_manager: Arc<SimulationManager>,
    metrics_collector: Arc<MetricsCollector>,
    mongodb_repo: Arc<MongoDBRepository>,
    listen_on: ListenOn,
    port: u16,
    readiness: Readiness,
) -> std::io::Result<()> {
    // A `SocketAddr`, not a formatted string: an IPv6 literal needs brackets,
    // and `format!("{listen_on}:{port}")` would log `http://::1:7070`, which is
    // not an address anyone can paste anywhere.
    let bind_address = SocketAddr::new(listen_on.ip(), port);

    info!("Starting server on {}", bind_address);

    HttpServer::new(move || {
        App::new()
            // The probe routes are excluded: an orchestrator polls them every
            // few seconds forever, and counting that constant would bury the
            // traffic the metrics exist to describe.
            // Every response says which process produced it, probes and
            // errors included: attribution is exactly what a replicated
            // deployment lacks otherwise.
            .wrap(InstanceHeader)
            .wrap(MetricsMiddleware::new(metrics_collector.clone()).excluding(&PROBE_PATHS))
            .configure(|cfg| {
                configure_routes(
                    cfg,
                    session_manager.clone(),
                    simulation_manager.clone(),
                    metrics_collector.clone(),
                    mongodb_repo.clone(),
                    readiness.clone(),
                )
            })
    })
    .bind(bind_address)?
    .run()
    .await
}
