use mongodb::bson::doc;
use serde::{Deserialize, Serialize};

/// Configuration for MongoDB connection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MongoDBConfig {
    /// MongoDB connection URI
    pub uri: String,
    /// Database name
    pub database: String,
    /// Collection for simulation steps
    pub steps_collection: String,
    /// Collection for events
    pub events_collection: String,
    /// Connection timeout in seconds
    pub timeout: u64,
}

impl Default for MongoDBConfig {
    fn default() -> Self {
        Self {
            uri: std::env::var("MONGODB_URI")
                .unwrap_or_else(|_| "mongodb://admin:password@localhost:27017".to_string()),
            database: std::env::var("MONGODB_DATABASE")
                .unwrap_or_else(|_| "optionchain_simulator".to_string()),
            steps_collection: std::env::var("MONGODB_STEPS_COLLECTION")
                .unwrap_or_else(|_| "steps".to_string()),
            events_collection: std::env::var("MONGODB_EVENTS_COLLECTION")
                .unwrap_or_else(|_| "events".to_string()),
            timeout: std::env::var("MONGODB_TIMEOUT")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(30),
        }
    }
}

impl std::fmt::Display for MongoDBConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{} steps={}, events={} (timeout: {}s)",
            self.uri, self.database, self.steps_collection, self.events_collection, self.timeout,
        )
    }
}
