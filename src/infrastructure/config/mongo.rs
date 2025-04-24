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

#[cfg(test)]
mod tests {
    use once_cell::sync::Lazy;
    use std::env;
    use std::sync::Mutex;

    use super::*;

    static ENV_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn set_var(name: &str, value: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            env::set_var(name, value);
        }
    }

    fn remove_var(name: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            env::remove_var(name);
        }
    }

    #[test]
    fn test_default_values() {
        let _guard = ENV_MUTEX.lock().expect("ENV_MUTEX poisoned");

        // Clear all relevant environment variables to test defaults
        remove_var("MONGODB_URI");
        remove_var("MONGODB_DATABASE");
        remove_var("MONGODB_STEPS_COLLECTION");
        remove_var("MONGODB_EVENTS_COLLECTION");
        remove_var("MONGODB_TIMEOUT");

        let config = MongoDBConfig::default();

        // Check default values
        assert_eq!(config.uri, "mongodb://admin:password@localhost:27017");
        assert_eq!(config.database, "optionchain_simulator");
        assert_eq!(config.steps_collection, "steps");
        assert_eq!(config.events_collection, "events");
        assert_eq!(config.timeout, 30);
    }

    #[test]
    fn test_environment_variable_overrides() {
        let _guard = ENV_MUTEX.lock().expect("ENV_MUTEX poisoned");

        // Set environment variables
        set_var("MONGODB_URI", "mongodb://testuser:testpass@testhost:27018");
        set_var("MONGODB_DATABASE", "test_database");
        set_var("MONGODB_STEPS_COLLECTION", "test_steps");
        set_var("MONGODB_EVENTS_COLLECTION", "test_events");
        set_var("MONGODB_TIMEOUT", "60");

        let config = MongoDBConfig::default();

        // Check values from environment variables
        assert_eq!(config.uri, "mongodb://testuser:testpass@testhost:27018");
        assert_eq!(config.database, "test_database");
        assert_eq!(config.steps_collection, "test_steps");
        assert_eq!(config.events_collection, "test_events");
        assert_eq!(config.timeout, 60);

        // Clean up
        remove_var("MONGODB_URI");
        remove_var("MONGODB_DATABASE");
        remove_var("MONGODB_STEPS_COLLECTION");
        remove_var("MONGODB_EVENTS_COLLECTION");
        remove_var("MONGODB_TIMEOUT");
    }

    #[test]
    fn test_invalid_timeout_format() {
        let _guard = ENV_MUTEX.lock().expect("ENV_MUTEX poisoned");

        // Set invalid timeout
        set_var("MONGODB_TIMEOUT", "not_a_number");

        let config = MongoDBConfig::default();

        // Should fall back to default
        assert_eq!(config.timeout, 30);

        // Clean up
        remove_var("MONGODB_TIMEOUT");
    }

    #[test]
    fn test_display_implementation() {
        let config = MongoDBConfig {
            uri: "mongodb://localhost:27017".to_string(),
            database: "testdb".to_string(),
            steps_collection: "steps_collection".to_string(),
            events_collection: "events_collection".to_string(),
            timeout: 45,
        };

        let display_string = format!("{}", config);
        let expected = "mongodb://localhost:27017/testdb steps=steps_collection, events=events_collection (timeout: 45s)";

        assert_eq!(display_string, expected);
    }

    #[test]
    fn test_clone() {
        let original = MongoDBConfig {
            uri: "mongodb://localhost:27017".to_string(),
            database: "testdb".to_string(),
            steps_collection: "steps_collection".to_string(),
            events_collection: "events_collection".to_string(),
            timeout: 45,
        };

        let cloned = original.clone();

        assert_eq!(cloned.uri, original.uri);
        assert_eq!(cloned.database, original.database);
        assert_eq!(cloned.steps_collection, original.steps_collection);
        assert_eq!(cloned.events_collection, original.events_collection);
        assert_eq!(cloned.timeout, original.timeout);
    }

    #[test]
    fn test_partial_environment_variables() {
        let _guard = ENV_MUTEX.lock().expect("ENV_MUTEX poisoned");

        // Clear all, then set only some variables
        remove_var("MONGODB_URI");
        remove_var("MONGODB_DATABASE");
        remove_var("MONGODB_STEPS_COLLECTION");
        remove_var("MONGODB_EVENTS_COLLECTION");
        remove_var("MONGODB_TIMEOUT");

        // Set only URI and timeout
        set_var("MONGODB_URI", "mongodb://custom:27017");
        set_var("MONGODB_TIMEOUT", "10");

        let config = MongoDBConfig::default();

        // Check that set variables are custom and others are default
        assert_eq!(config.uri, "mongodb://custom:27017");
        assert_eq!(config.database, "optionchain_simulator"); // default
        assert_eq!(config.steps_collection, "steps"); // default
        assert_eq!(config.events_collection, "events"); // default
        assert_eq!(config.timeout, 10); // from env var

        // Clean up
        remove_var("MONGODB_URI");
        remove_var("MONGODB_TIMEOUT");
    }

    #[test]
    fn test_serialization_deserialization() {
        let config = MongoDBConfig {
            uri: "mongodb://localhost:27017".to_string(),
            database: "testdb".to_string(),
            steps_collection: "steps_collection".to_string(),
            events_collection: "events_collection".to_string(),
            timeout: 45,
        };

        // Serialize to JSON
        let serialized = serde_json::to_string(&config).expect("Failed to serialize config");

        // Validate JSON contains expected fields
        assert!(serialized.contains("mongodb://localhost:27017"));
        assert!(serialized.contains("testdb"));
        assert!(serialized.contains("steps_collection"));
        assert!(serialized.contains("events_collection"));
        assert!(serialized.contains("45"));

        // Deserialize back to object
        let deserialized: MongoDBConfig =
            serde_json::from_str(&serialized).expect("Failed to deserialize");

        // Check all fields match
        assert_eq!(deserialized.uri, config.uri);
        assert_eq!(deserialized.database, config.database);
        assert_eq!(deserialized.steps_collection, config.steps_collection);
        assert_eq!(deserialized.events_collection, config.events_collection);
        assert_eq!(deserialized.timeout, config.timeout);
    }
}
