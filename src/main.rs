//! The entry point of the OptionChain Simulator application.
//!
//! This asynchronous function initializes logging, sets up a Redis-backed session store,
//! and starts an HTTP server to handle client requests.
//!
//! # Workflow
//! 1. Sets up the application logger to handle logging across the application.
//! 2. Creates a `RedisConfig` with default settings and logs the connection details.
//! 3. Initializes a `RedisClient` instance to communicate with Redis, wrapped in an `Arc`
//!    for safe concurrent access.
//! 4. Creates an `InRedisSessionStore` for managing session data, using the Redis client,
//!    with a custom Redis key prefix (`optionchain:session:`) and a TTL of 1 hour.
//! 5. Constructs a `SessionManager` to manage user sessions by wrapping the session store.
//! 6. Starts an HTTP server using `start_server`, listening on all available interfaces (`ListenOn::All`)
//!    at port `7070`.
//!
//! # Returns
//! - On success, returns `Ok(())`.
//! - On failure, returns an error wrapped in a `Box<dyn std::error::Error>`.
//!
//! # Dependencies
//! - The `optionchain_simulator` crate is used for infrastructure utilities like Redis client and session store setup.
//! - `optionstratlib::utils::setup_logger` is used for setting up logging.
//! - `tracing` crate is used for log output.
//!
//! # Redis Configuration
//! - The Redis key prefix for the session store is `optionchain:session:`.
//! - The TTL (time-to-live) for session keys in the Redis store is 3600 seconds (1 hour).
//!
//! # HTTP Server Details
//! - Listening Address: `0.0.0.0` (all interfaces).
//! - Port: `7070`.
//!
//! # Example
//! ```
//! // To run the application:
//! // $ cargo run
//! ```
//!
//! # Error Handling
//! If any error occurs during setup (e.g., Redis connection issues, server failure), the error
//! message will be logged, and the function returns an appropriate error.
//!
//! # Relevant Modules
//! - `optionchain_simulator::session`: Manages session storage and session services.
//! - `optionchain_simulator::infrastructure`: Provides Redis client configuration and integration.
//! - `optionstratlib::utils`: Contains utility functions including the logger setup.
//!
//! # Panics
//! - This function panics if the `actix_web::main` macro fails to initialize the Actix runtime.
//!
//! # See Also
//! - [`RedisClient`] for details on Redis interaction.
//! - [`SessionManager`] for session management implementation.
//! - [`start_server`] for the HTTP server startup logic.
//!
//! # Author
//! - Generated and maintained by the developers of `optionchain_simulator`.

use optionchain_simulator::api::{ListenOn, start_server};
use optionchain_simulator::infrastructure::{RedisClient, RedisConfig};
use optionchain_simulator::session::{InRedisSessionStore, SessionManager};
use optionstratlib::utils::setup_logger_with_level;
use std::sync::Arc;
use tracing::info;

/// The `main` function is the entry point of the application using the Actix Web server framework.
/// It initializes the logger, sets up the session management with Redis as the backend, and starts the HTTP server.
///
/// # Steps:
/// 1. Calls `setup_logger()` to initialize logging.
/// 2. Creates a Redis configuration using the default `RedisConfig`.
/// 3. Logs the Redis connection details.
/// 4. Initializes a `RedisClient` with the configuration and wraps it in an `Arc` for shared ownership.
/// 5. Setups up an in-Redis session store (`InRedisSessionStore`) with:
///    - An optional custom key prefix (`optionchain:session:`).
///    - An optional TTL (time-to-live) of 1 hour for the session keys.
/// 6. Wraps the session store in an `Arc` for shared ownership and constructs a `SessionManager` instance.
/// 7. Defines the listening IP/host (`ListenOn::All`, meaning all available interfaces) and the server port (7070).
/// 8. Logs the server's starting information.
/// 9. Calls `start_server` to start the HTTP server with the session manager, listen address, and port:
///    - On success, the server runs as expected, and `Ok(())` is returned.
///    - On failure, the error is logged and returned.
///
/// # Returns:
/// - On success: `Ok(())`.
/// - On error: `Err(Box<dyn std::error::Error>)` with the description of the failure.
///
/// # Dependencies:
/// - `RedisConfig`: Configuration for connecting to the Redis instance.
/// - `RedisClient`: Redis client for communicating with the Redis database.
/// - `InRedisSessionStore`: Manages session persistence in Redis.
/// - `SessionManager`: Manages session lifecycle and retrieval for the web application.
/// - `ListenOn`: Enum to specify the server's listening IP/host.
/// - `start_server`: Function to start the Actix Web server with the provided session manager,
///    host, and port.
///
/// # Notes:
/// - Make sure that the Redis server is running and accessible at the address specified in `RedisConfig`.
/// - Ensure the Actix-Web dependencies are properly configured with the required features for `#[actix_web::main]`.
/// - The server listens on all interfaces (`0.0.0.0`) and port 7070 by default.
///
/// # Example Log Output:
/// ```text
/// [INFO] Connecting to Redis at default://127.0.0.1:6379
/// [INFO] Starting HTTP server at http://0.0.0.0:7070
/// ```
/// #[actix_web::main]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logger_with_level("DEBUG");

    // Create session store
    let redis_config = RedisConfig::default();
    info!("Connecting to Redis at {}", redis_config);
    let redis_client = Arc::new(RedisClient::new(redis_config)?);
    let store = Arc::new(InRedisSessionStore::new(
        redis_client,
        Some("optionchain:session:".to_string()), // Custom key prefix
        Some(3600),                               // 1 hour TTL
    ));

    // Create session manager
    let session_manager = Arc::new(SessionManager::new(store.clone()));
    let listen_on = ListenOn::All;
    let port = 7070;
    // Start HTTP server
    info!("Starting HTTP server at http://{}:{}", listen_on, port);
    match start_server(session_manager, listen_on, port).await {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string().into()),
    }
}
