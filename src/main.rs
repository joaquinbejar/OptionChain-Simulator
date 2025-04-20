//! TBD

use optionchain_simulator::api::{ListenOn, start_server};
use optionchain_simulator::session::{InMemorySessionStore, SessionManager};
use optionstratlib::utils::setup_logger;
use std::sync::Arc;
use tracing::info;

/// The entry point of a Rust program.
///
/// The `main` function serves as the starting point of execution for any
/// Rust program. It is a special function that is called by the runtime
/// when the program is executed.
///
/// For more complex programs, the `main` function can initialize
/// various components or call other functions to perform specific tasks.
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    setup_logger();

    // Create session store
    let session_store = Arc::new(InMemorySessionStore::new());

    // Create session manager
    let session_manager = Arc::new(SessionManager::new(session_store));

    let listen_on = ListenOn::All;
    let port = 7070;
    // Start HTTP server
    info!("Starting HTTP server at http://{}:{}", listen_on, port);
    start_server(session_manager, listen_on, port).await
}
