use crate::api::rest::models::ListenOn;

use crate::session::SessionManager;
use actix_web::{App, HttpServer};
use std::sync::Arc;
use tracing::info;

use crate::api::rest::routes::configure_routes;
use crate::infrastructure::{MetricsCollector, MetricsMiddleware};

/// Starts an HTTP server with the given configuration.
///
/// # Arguments
///
/// * `session_manager` - A shared reference to the `SessionManager`, used to manage user sessions.
/// * `metrics_collector` - A shared reference to the `MetricsCollector`, used for collecting server metrics.
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
    metrics_collector: Arc<MetricsCollector>,
    listen_on: ListenOn,
    port: u16,
) -> std::io::Result<()> {
    let bind_address = format!("{}:{}", listen_on, port);

    info!("Starting server on {}", bind_address);

    HttpServer::new(move || {
        App::new()
            .wrap(MetricsMiddleware::new(metrics_collector.clone()))
            .configure(|cfg| {
                configure_routes(cfg, session_manager.clone(), metrics_collector.clone())
            })
    })
    .bind(bind_address)?
    .run()
    .await
}
