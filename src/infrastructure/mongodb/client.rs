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

/// MongoDB credentials taken from outside the URI (Docker secrets). `Debug`
/// never shows the password.
#[derive(Clone, Default)]
pub struct MongoDBCredentials {
    /// Replaces the URI's user when set.
    pub username: Option<String>,
    /// Replaces the URI's password when set.
    pub password: Option<String>,
}

impl MongoDBCredentials {
    /// `MONGODB_USER_FILE` and `MONGODB_PASSWORD_FILE`, each read, trimmed and
    /// required to be non-empty when set.
    ///
    /// # Errors
    ///
    /// When a `*_FILE` variable names a file that cannot be read or is empty.
    pub fn from_env() -> Result<Self, ChainError> {
        Ok(Self {
            username: crate::infrastructure::config::read_secret_file("MONGODB_USER")?,
            password: crate::infrastructure::config::read_secret_file("MONGODB_PASSWORD")?,
        })
    }
}

impl std::fmt::Debug for MongoDBCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongoDBCredentials")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

/// The `authSource` option of a MongoDB URI, if it has one (option names are
/// case-insensitive in the URI spec).
fn auth_source_from_uri(uri: &str) -> Option<String> {
    let query = uri.split_once('?')?.1;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| key.eq_ignore_ascii_case("authSource"))
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
}

impl MongoDBClient {
    /// Creates a new MongoDB client with the provided configuration
    ///
    /// The client authenticates exactly as `config.uri` says; no secret file
    /// is consulted, so a caller that builds a config for some other server
    /// never sends it the deployment's credentials. The environment-driven
    /// path (`init_mongodb`) uses [`MongoDBClient::with_credentials`].
    #[instrument(skip(config), level = "debug")]
    pub async fn new(config: MongoDBConfig) -> Result<Self, ChainError> {
        Self::with_credentials(config, MongoDBCredentials::default()).await
    }

    /// Like [`MongoDBClient::new`], with `credentials` (from secret files)
    /// winning over the user and password in the URI.
    ///
    /// When the URI carries no user, the driver drops its `authSource` and
    /// default database while parsing (there is no credential to attach them
    /// to), so the credential built here takes its source from the URI's
    /// `authSource`, else from its default database, exactly as the driver
    /// would have with the user in the URI.
    #[instrument(skip(config, credentials), level = "debug")]
    pub async fn with_credentials(
        config: MongoDBConfig,
        credentials: MongoDBCredentials,
    ) -> Result<Self, ChainError> {
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

        if credentials.username.is_some() || credentials.password.is_some() {
            let mut credential = match client_options.credential.take() {
                Some(credential) => credential,
                None => {
                    let mut credential = mongodb::options::Credential::default();
                    credential.source = auth_source_from_uri(&config.uri)
                        .or_else(|| client_options.default_database.clone());
                    credential
                }
            };
            if credentials.username.is_some() {
                credential.username = credentials.username;
            }
            if credentials.password.is_some() {
                credential.password = credentials.password;
            }
            client_options.credential = Some(credential);
        }

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

    /// Asks the server whether it is there.
    ///
    /// The same `{ping: 1}` the constructor runs: a no-auth, cheap command that
    /// says the deployment answered, and nothing about any one collection.
    ///
    /// The mutex is released BEFORE the command is awaited. Held across it, a
    /// probe against an unreachable server would pin the handle for the probe's
    /// whole timeout, and every `save_step` and `save_event` on the request
    /// path would block behind a health check that is polled on a timer.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Internal`] when the command fails, with the
    /// message passed through `redact_userinfo`: driver errors echo the
    /// connection URI, and a readiness body is a public surface.
    #[instrument(skip(self), level = "debug")]
    pub async fn ping(&self) -> Result<(), ChainError> {
        // `Database` is a cheap handle; cloning it costs a refcount and buys
        // back the lock immediately.
        let db = {
            let guard = self.db.lock().await;
            guard.clone()
        };
        db.run_command(doc! {"ping": 1}).await.map_err(|e| {
            ChainError::Internal(redact_userinfo(&format!("Failed to ping MongoDB: {}", e)))
        })?;
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
    /// (env-driven) configuration and ASSERTS success. The environment must
    /// provide a reachable MongoDB whose credentials match `MONGODB_URI` (the
    /// CI integration job provisions admin/password on its service container);
    /// tolerating `Err` here would let the job stay green without validating
    /// initialization at all.
    #[test]
    #[ignore = "requires a live MongoDB matching MONGODB_URI; run with -- --ignored"]
    async fn test_init_mongodb() {
        let repo = crate::infrastructure::repositories::mongo_repo::init_mongodb()
            .await
            .expect("init_mongodb must succeed against the provisioned live MongoDB");
        assert!(Arc::strong_count(&repo) >= 1);
    }

    #[::core::prelude::v1::test]
    fn test_auth_source_is_read_from_the_uri() {
        assert_eq!(
            super::auth_source_from_uri("mongodb://mongo/app?retryWrites=true&AUTHSOURCE=users"),
            Some("users".to_string())
        );
        assert_eq!(super::auth_source_from_uri("mongodb://mongo/app"), None);
        assert_eq!(
            super::auth_source_from_uri("mongodb://mongo/?authSource="),
            None
        );
    }

    #[::core::prelude::v1::test]
    fn test_credentials_debug_hides_the_password() {
        let credentials = super::MongoDBCredentials {
            username: Some("admin".to_string()),
            password: Some("s3cr3t".to_string()),
        };
        let debug = format!("{credentials:?}");
        assert!(debug.contains("admin"));
        assert!(!debug.contains("s3cr3t"));
    }
}
