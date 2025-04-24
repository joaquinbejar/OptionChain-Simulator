use mongodb::bson::doc;
use std::fmt::Debug;
use std::sync::Arc;

use crate::infrastructure::config::mongo::MongoDBConfig;
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
        info!("Connecting to MongoDB at {}", config.uri);

        let mut client_options = ClientOptions::parse(&config.uri)
            .await
            .map_err(|e| ChainError::Internal(format!("Failed to parse MongoDB URI: {}", e)))?;

        client_options.app_name = Some("OptionChain-Simulator".to_string());
        client_options.connect_timeout = Some(std::time::Duration::from_secs(config.timeout));

        let client = mongodb::Client::with_options(client_options)
            .map_err(|e| ChainError::Internal(format!("Failed to create MongoDB client: {}", e)))?;

        // Test connection
        client
            .database("admin")
            .run_command(doc! {"ping": 1})
            .await
            .map_err(|e| ChainError::Internal(format!("Failed to ping MongoDB: {}", e)))?;

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

    #[test]
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

    #[test]
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
