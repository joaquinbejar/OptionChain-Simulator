//! TBD


use std::sync::Arc;
use optionstratlib::utils::setup_logger;
use tracing::info;
use optionchain_simulator::api::start_server;
use optionchain_simulator::session::{InMemorySessionStore, SessionManager};


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

    // Start HTTP server
    info!("Starting HTTP server at http://127.0.0.1:8080");
    start_server(session_manager).await
}