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
//! 6. Reads where to listen from `OCS_BIND_ADDRESS` and `OCS_PORT`, refusing to
//!    start when either is unusable, and starts an HTTP server there using
//!    `start_server`.
//!
//! # Returns
//! - On success, returns `Ok(())`.
//! - On failure, returns an error wrapped in a `Box<dyn std::error::Error>`.
//!
//! # Dependencies
//! - The `optionchain_simulator` crate is used for infrastructure utilities like Redis client and session store setup.
//! - `optionstratlib::utils::setup_logger_with_level` sets up logging, at the
//!   level `LOGLEVEL` resolves to (default `INFO`).
//! - `tracing` crate is used for log output.
//!
//! # Redis Configuration
//! - The Redis key prefix for the session store is `optionchain:session:`.
//! - The TTL (time-to-live) for session keys in the Redis store is 3600 seconds (1 hour).
//!
//! # HTTP Server Details
//! - Listening Address: `OCS_BIND_ADDRESS`, an IP address or the words `all`
//!   and `localhost`. Default: `127.0.0.1`. It used to be a hardcoded
//!   `0.0.0.0`, so a deployment that relied on being reachable off the host has
//!   to ask for it now; a container must set `0.0.0.0` for its published port
//!   to lead anywhere.
//! - Port: `OCS_PORT`, `1..=65535`. Default: `7070`.
//! - An unusable value in either FAILS STARTUP naming the variable, rather than
//!   binding somewhere nobody asked for.
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

use optionchain_simulator::api::start_server;
use optionchain_simulator::infrastructure::{
    ClickHouseSnapshotRepository, DependencyProbe, MetricsCollector, MongoDbProbe, Readiness,
    RedisClient, RedisConfig, RedisProbe, ServerConfig, SimulationV2Config, WarehouseProbe,
    init_mongodb, resolve_log_level_from_env,
};
use optionchain_simulator::session::{
    DEFAULT_TAPE_KEY_PREFIX, InRedisSessionStore, InRedisSimulationStore, RedisTapeCache,
    SessionManager, SimulationManager,
};
use optionstratlib::utils::setup_logger_with_level;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// The `main` function is the entry point of the application using the Actix Web server framework.
/// It initializes the logger, sets up the session management with Redis as the backend, and starts the HTTP server.
///
/// # Steps:
/// 1. Resolves `LOGLEVEL` (default `INFO`) and initialises logging at that
///    level, warning once if the value was unrecognised.
/// 2. Creates a Redis configuration using the default `RedisConfig`.
/// 3. Logs the Redis connection details.
/// 4. Initializes a `RedisClient` with the configuration and wraps it in an `Arc` for shared ownership.
/// 5. Setups up an in-Redis session store (`InRedisSessionStore`) with:
///    - An optional custom key prefix (`optionchain:session:`).
///    - An optional TTL (time-to-live) of 1 hour for the session keys.
/// 6. Wraps the session store in an `Arc` for shared ownership and constructs a `SessionManager` instance.
/// 7. Resolves the listening IP/host and port from `OCS_BIND_ADDRESS` and `OCS_PORT`, defaulting to
///    `127.0.0.1:7070` and failing startup when either value is unusable.
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
/// - `ServerConfig`: Where to listen, read from `OCS_BIND_ADDRESS` and `OCS_PORT`.
/// - `start_server`: Function to start the Actix Web server with the provided session manager,
///    host, and port.
///
/// # Notes:
/// - Make sure that the Redis server is running and accessible at the address specified in `RedisConfig`.
/// - Ensure the Actix-Web dependencies are properly configured with the required features for `#[actix_web::main]`.
/// - The server listens on `127.0.0.1:7070` by default; `OCS_BIND_ADDRESS=0.0.0.0` is what makes it
///   reachable off the host, which this service leaves to the operator because it has no
///   authentication and no rate limiting.
///
/// # Example Log Output:
/// ```text
/// [INFO] Connecting to Redis at default://127.0.0.1:6379
/// [INFO] Starting HTTP server at http://127.0.0.1:7070
/// ```
/// #[actix_web::main]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `LOGLEVEL`, not a literal. The level has to be resolved BEFORE a
    // subscriber exists, so a rejected value is carried out of the resolver and
    // warned about below, once the subscriber can carry the warning.
    let log_level = resolve_log_level_from_env();
    setup_logger_with_level(log_level.level.as_str());
    if let Some(rejected) = &log_level.rejected {
        warn!(
            value = %rejected,
            default = %log_level.level,
            "unrecognised LOGLEVEL; using the default"
        );
    }
    // INFO, whatever the resolved level is. Confirming a healthy configuration
    // at WARN or ERROR would file an incident on every restart of a service
    // that is working exactly as configured, and an operator who filters at
    // those severities is filtering precisely to not see this. Only the
    // rejected-value branch above is a real problem, and it warns.
    //
    // The consequence is deliberate: with `LOGLEVEL=WARN` or above this line is
    // suppressed. That is the setting doing its job, and the level is
    // observable without it — every subsequent line is emitted at the level it
    // resolved to.
    info!(level = %log_level.level, "Log level resolved from LOGLEVEL");

    // `OCS_BIND_ADDRESS` and `OCS_PORT`, not constants, and read HERE — before
    // Redis, MongoDB and ClickHouse are dialled. A typo in a port should cost a
    // refusal, not three connections and a schema migration first. An unusable
    // value fails startup naming the variable rather than binding something
    // nobody asked for: two shards quietly fighting over one port is worse.
    let server = ServerConfig::from_env()?;

    // Create a session store
    let redis_config = RedisConfig::default();
    info!("Connecting to Redis at {}", redis_config);
    let redis_client = Arc::new(RedisClient::new(redis_config).await?);
    let redis_client_v2 = Arc::clone(&redis_client);
    let redis_client_probe = Arc::clone(&redis_client);
    let store = Arc::new(InRedisSessionStore::new(
        redis_client,
        Some("optionchain:session:".to_string()), // Custom key prefix
        Some(3600),                               // 1 hour TTL
    ));

    // Create a metrics collector
    let metrics_collector = Arc::new(MetricsCollector::new()?);
    // Create a MongoDB repository
    let mongodb_repository = init_mongodb().await?;

    // Create a session manager
    let session_manager = Arc::new(SessionManager::new(store.clone()));

    // The v2 rolling simulations live in their own Redis key space, so a v2 id
    // can never resolve a v1 session and a v1 document is never read back as
    // rolling configuration (ADR 0001 section 12.2).
    //
    // Their operational limits are loaded and validated here, before anything
    // binds: an invalid knob fails startup with a message naming the variable
    // rather than silently reverting to a default that would change how long
    // simulations live.
    let v2_config = SimulationV2Config::from_env()?;
    let simulation_store = Arc::new(InRedisSimulationStore::new(
        Arc::clone(&redis_client_v2),
        None, // the documented v2 prefix
        Some(v2_config.retention_secs()),
    ));
    // Built tapes are shared through the same Redis, so a step served by a
    // different instance does not rebuild what its neighbour already walked
    // (issue #136). The window matches the simulations': a tape that outlives
    // its simulation is memory nobody can use, and one that expires first
    // costs a rebuild.
    let shared_tapes = Arc::new(RedisTapeCache::new(
        Arc::clone(&redis_client_v2),
        DEFAULT_TAPE_KEY_PREFIX,
        Duration::from_secs(v2_config.retention_secs()),
    ));
    // Snapshot persistence is opt-in (`OCS_SNAPSHOT_PERSISTENCE_ENABLED`). When
    // it is off the manager never learns the feature exists; when it is on, the
    // tables are created here rather than on the first advance, so a schema
    // problem is a startup failure with a message rather than a warning buried
    // in the serving path.
    //
    // The manager owns the one repository: it writes through it, and the v2
    // export reads back through the same handle — the routes take it off the
    // manager rather than being passed a second one, because two handles could
    // be configured differently and there is only ever one warehouse.
    let mut simulation_manager =
        SimulationManager::new(simulation_store, v2_config).with_shared_tapes(shared_tapes);
    match ClickHouseSnapshotRepository::from_env()? {
        Some(warehouse) => {
            warehouse.ensure_schema().await?;
            info!("v2 snapshot persistence is enabled");
            simulation_manager = simulation_manager.with_warehouse(Arc::new(warehouse));
        }
        None => {
            info!("v2 snapshot persistence is disabled; snapshots are served from replay only");
        }
    }
    let simulation_manager = Arc::new(simulation_manager);

    // Reap idle simulations and evict what they left cached. Detached, because
    // the sweep is independent of any request; it stops when the process does.
    let sweeper = Arc::clone(&simulation_manager);
    let sweeper_metrics = Arc::clone(&metrics_collector);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(v2_config.cleanup_interval);
        loop {
            ticker.tick().await;
            match sweeper.cleanup().await {
                Ok(expired) => {
                    sweeper_metrics.record_v2_simulations_expired(expired.len());
                    sweeper_metrics.set_v2_cache_sizes(
                        sweeper.cached_tapes() as i64,
                        sweeper.cached_snapshots() as i64,
                    );
                }
                // A failed sweep is worth knowing about but must not stop the
                // loop: the next tick retries, and Redis expiring keys on its
                // own means nothing leaks in the meantime except the cache
                // entries this pass would have dropped.
                Err(error) => warn!(%error, "the v2 retention sweep failed"),
            }
        }
    });

    // What `GET /ready` will check: the services THIS process opened, so an
    // instance is never reported unready over a dependency it does not use.
    // Redis and MongoDB are always there — startup aborts without them — and
    // the warehouse only when snapshot persistence is on.
    let mut probes: Vec<Arc<dyn DependencyProbe>> = vec![
        Arc::new(RedisProbe::new(redis_client_probe)),
        Arc::new(MongoDbProbe::new(Arc::clone(&mongodb_repository))),
    ];
    if let Some(warehouse) = simulation_manager.warehouse() {
        probes.push(Arc::new(WarehouseProbe::new(warehouse)));
    }
    let readiness = Readiness::new(probes);

    let listen_on = server.address;
    let port = server.port;

    // Through `SocketAddr` so an IPv6 literal is bracketed: `http://::1:7070`
    // is not something an operator can paste anywhere.
    info!(
        "Starting HTTP server at http://{}",
        SocketAddr::new(listen_on.ip(), port)
    );
    if listen_on.is_public() {
        // Said once, out loud: this service has no authentication and no rate
        // limiting, so reaching it off the host is a decision.
        warn!(
            address = %listen_on,
            "Listening beyond loopback; the service has no authentication"
        );
    }
    match start_server(
        session_manager,
        simulation_manager,
        metrics_collector,
        mongodb_repository,
        listen_on,
        port,
        readiness,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string().into()),
    }
}
