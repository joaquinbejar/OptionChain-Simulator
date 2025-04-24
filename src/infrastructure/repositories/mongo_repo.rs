use std::sync::Arc;
use std::time::Instant;
use tracing::{info, instrument};
use uuid::Uuid;

use crate::api::rest::responses::{ChainResponse, SessionResponse};
use crate::infrastructure::MetricsCollector;
use crate::infrastructure::config::mongo::MongoDBConfig;
use crate::infrastructure::mongodb::MongoDBClient;
use crate::utils::ChainError;

/// Repository for managing session history in MongoDB (insert-only operations)
pub struct MongoDBRepository {
    /// MongoDB client
    client: Arc<MongoDBClient>,
}

impl MongoDBRepository {
    /// Creates a new MongoDB repository with the provided client
    pub fn new(client: Arc<MongoDBClient>) -> Self {
        Self { client }
    }

    /// Saves a chain response to the steps collection
    #[instrument(skip(self, chain_data, metrics), level = "debug")]
    pub async fn save_chain_step(
        &self,
        session_id: Uuid,
        chain_data: ChainResponse,
        metrics: Arc<MetricsCollector>,
    ) -> Result<(), ChainError> {
        info!(session_id = %session_id, "Saving chain step to MongoDB");
        // Start measuring execution time
        let start_time = Instant::now();
        // MongoDB operation
        let result = self.client.save_step(session_id, chain_data).await;
        // Record metrics
        let duration = start_time.elapsed();
        metrics.record_mongodb_insert("steps"); // Count the insert 
        metrics.record_mongodb_insert_duration("steps", duration); // Record the latency

        result
    }

    /// Saves a session response to the events collection
    #[instrument(skip(self, session_data, metrics), level = "debug")]
    pub async fn save_session_event(
        &self,
        session_id: Uuid,
        session_data: SessionResponse,
        metrics: Arc<MetricsCollector>,
    ) -> Result<(), ChainError> {
        info!(session_id = %session_id, "Saving session event to MongoDB");
        // Start measuring execution time
        let start_time = Instant::now();
        // MongoDB operation
        let result = self.client.save_event(session_id, session_data).await;
        // Record metrics
        let duration = start_time.elapsed();
        metrics.record_mongodb_insert("events"); // Count the insert 
        metrics.record_mongodb_insert_duration("events", duration); // Record the latency

        result
    }

    /// Saves a generic event to the events collection
    #[instrument(skip(self, event), level = "debug")]
    pub async fn save_generic_event<T>(&self, session_id: Uuid, event: T) -> Result<(), ChainError>
    where
        T: Sync + Send + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    {
        info!(session_id = %session_id, "Saving generic event to MongoDB");
        self.client.save_event(session_id, event).await
    }
}

/// Initializes MongoDB client and repository, then returns the repository
pub async fn init_mongodb() -> Result<Arc<MongoDBRepository>, ChainError> {
    let config = MongoDBConfig::default();
    info!("Initializing MongoDB with config: {}", config);

    let client = MongoDBClient::new(config).await?;
    let client_arc = Arc::new(client);
    let repository = Arc::new(MongoDBRepository::new(client_arc));

    info!("MongoDB repository initialized successfully");
    Ok(repository)
}
