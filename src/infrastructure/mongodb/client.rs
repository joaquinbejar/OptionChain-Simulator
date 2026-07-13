use mongodb::bson::doc;
use std::fmt::Debug;
use std::sync::Arc;

use crate::infrastructure::config::mongo::MongoDBConfig;
use crate::infrastructure::config::{redact_uri, redact_userinfo};
use crate::utils::ChainError;
use mongodb::options::ClientOptions;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use tracing::{debug, info, instrument};
use uuid::Uuid;

/// Represents a connection to MongoDB with collections for steps and events
pub struct MongoDBClient {
    /// Database instance
    db: Arc<Mutex<mongodb::Database>>,
    /// Configuration for MongoDB connection
    config: MongoDBConfig,
}

impl MongoDBClient {
    /// Creates a new MongoDB client with the provided configuration
    #[instrument(skip(config), level = "debug")]
    pub async fn new(config: MongoDBConfig) -> Result<Self, ChainError> {
        // The URI may embed credentials; never log it raw.
        info!("Connecting to MongoDB at {}", redact_uri(&config.uri));

        // Driver errors can echo the connection URI back, so every error string
        // is passed through `redact_userinfo` before it reaches a log or a
        // `ChainError`.
        let mut client_options = ClientOptions::parse(&config.uri).await.map_err(|e| {
            ChainError::Internal(redact_userinfo(&format!(
                "Failed to parse MongoDB URI: {}",
                e
            )))
        })?;

        client_options.app_name = Some("OptionChain-Simulator".to_string());
        // `config.timeout` (MONGODB_TIMEOUT, seconds) bounds BOTH phases of
        // reaching a server: the TCP connect AND server selection. Without the
        // latter, the startup ping against an unavailable server waits for the
        // driver's default server-selection timeout (~30s) regardless of the
        // configured value.
        let timeout = std::time::Duration::from_secs(config.timeout);
        client_options.connect_timeout = Some(timeout);
        client_options.server_selection_timeout = Some(timeout);

        let client = mongodb::Client::with_options(client_options).map_err(|e| {
            ChainError::Internal(redact_userinfo(&format!(
                "Failed to create MongoDB client: {}",
                e
            )))
        })?;

        // Test connection
        client
            .database("admin")
            .run_command(doc! {"ping": 1})
            .await
            .map_err(|e| {
                ChainError::Internal(redact_userinfo(&format!("Failed to ping MongoDB: {}", e)))
            })?;

        info!("Successfully connected to MongoDB");

        let db = client.database(&config.database);

        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            config,
        })
    }

    /// Gets a reference to the steps collection
    async fn steps_collection<T>(&self) -> mongodb::Collection<T>
    where
        T: Sync + Send + Serialize + DeserializeOwned,
    {
        let db = self.db.lock().await;
        let config = self.get_config();
        db.collection(&config.steps_collection)
    }

    /// Gets a reference to the events collection
    async fn events_collection<T>(&self) -> mongodb::Collection<T>
    where
        T: Sync + Send + Serialize + DeserializeOwned,
    {
        let db = self.db.lock().await;
        let config = self.get_config();
        db.collection(&config.events_collection)
    }

    /// Saves a simulation step to the steps collection
    #[instrument(skip(self, step), level = "debug")]
    pub async fn save_step<T>(&self, session_id: Uuid, step: T) -> Result<(), ChainError>
    where
        T: Sync + Send + Serialize + DeserializeOwned + Debug,
    {
        let collection = self.steps_collection::<T>().await;

        debug!(session_id = %session_id, "Saving step to MongoDB");

        let result = collection
            .insert_one(step)
            .await
            .map_err(|e| ChainError::Internal(format!("Failed to save step to MongoDB: {}", e)))?;

        debug!(
            session_id = %session_id,
            insert_id = %result.inserted_id,
            "Step saved successfully"
        );

        Ok(())
    }

    /// Saves an event to the events collection
    #[instrument(skip(self, event), level = "debug")]
    pub async fn save_event<T>(&self, session_id: Uuid, event: T) -> Result<(), ChainError>
    where
        T: Sync + Send + Serialize + DeserializeOwned + Debug,
    {
        let collection = self.events_collection::<T>().await;

        debug!(session_id = %session_id, "Saving event to MongoDB");

        let result = collection
            .insert_one(event)
            .await
            .map_err(|e| ChainError::Internal(format!("Failed to save event to MongoDB: {}", e)))?;

        debug!(
            session_id = %session_id,
            insert_id = %result.inserted_id,
            "Event saved successfully"
        );

        Ok(())
    }

    /// Returns the MongoDB configuration
    pub fn get_config(&self) -> &MongoDBConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use crate::infrastructure::config::mongo::MongoDBConfig;
    use crate::infrastructure::mongodb::MongoDBClient;
    use std::sync::Arc;
    use tokio::test;

    /// Live integration test: requires a running MongoDB. Opt in with
    /// `cargo test -- --ignored` (CI runs it in the dedicated Integration job
    /// with a mongo service container).
    #[test]
    #[ignore = "requires live MongoDB on localhost:27017; run with -- --ignored"]
    async fn test_mongodb_client_initialization() {
        // Test that we can create a new MongoDB client
        let config = MongoDBConfig {
            uri: "mongodb://localhost:27017".to_string(),
            database: "test_db".to_string(),
            steps_collection: "test_steps".to_string(),
            events_collection: "test_events".to_string(),
            timeout: 5,
        };

        let client_result = MongoDBClient::new(config.clone()).await;
        assert!(
            client_result.is_ok(),
            "Failed to create MongoDB client: {:?}",
            client_result.err()
        );

        let client = client_result.unwrap();
        assert_eq!(client.get_config().database, "test_db");
    }

    /// Hermetic replacement for the connection test: exercises config
    /// construction and URI parsing without pinging a server.
    #[test]
    async fn test_client_options_parse_without_server() {
        let config = MongoDBConfig {
            uri: "mongodb://localhost:27017".to_string(),
            database: "test_db".to_string(),
            steps_collection: "test_steps".to_string(),
            events_collection: "test_events".to_string(),
            timeout: 5,
        };

        let options = mongodb::options::ClientOptions::parse(&config.uri).await;
        assert!(options.is_ok(), "a well-formed URI must parse");
        assert_eq!(config.database, "test_db");
        assert_eq!(config.timeout, 5);
    }

    /// Regression for issue #17: with an unavailable server, `new` must fail
    /// within the configured timeout instead of the driver's ~30s default
    /// server-selection timeout. Hermetic — asserts the FAILURE is fast
    /// (port 1 on localhost refuses connections; server selection retries
    /// until the configured bound).
    #[test]
    async fn test_unavailable_server_fails_within_configured_timeout() {
        let config = MongoDBConfig {
            uri: "mongodb://localhost:1".to_string(),
            database: "test_db".to_string(),
            steps_collection: "test_steps".to_string(),
            events_collection: "test_events".to_string(),
            timeout: 1,
        };

        let start = std::time::Instant::now();
        let result = MongoDBClient::new(config).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "connection to a closed port must fail");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "startup failure took {elapsed:?}; the 1s configured timeout must \
             bound server selection, not the ~30s driver default"
        );
    }

    /// Live integration test: exercises `init_mongodb` against the default
    /// (env-driven) configuration. Even though it tolerates a missing server,
    /// the failure path burns the full configured server-selection timeout
    /// (30s by default), so it is opt-in with the rest of the live tests.
    #[test]
    #[ignore = "exercises a live MongoDB via env config; run with -- --ignored"]
    async fn test_init_mongodb() {
        // Test the init_mongodb function
        let result = crate::infrastructure::repositories::mongo_repo::init_mongodb().await;

        // This might fail if MongoDB is not running, which is expected in CI environments
        match result {
            Ok(repo) => {
                // If it succeeded, verify we got a repository
                assert!(Arc::strong_count(&repo) >= 1);
            }
            Err(e) => {
                // In CI, we expect this to fail with a connection error
                println!("MongoDB initialization failed (expected in CI): {:?}", e);
                // We don't assert because this is expected to fail in environments without MongoDB
            }
        }
    }
}
