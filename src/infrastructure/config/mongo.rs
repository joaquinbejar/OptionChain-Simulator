/// Configuration for MongoDB connection
///
/// The `uri` may embed credentials, so `Debug` is implemented manually (not
/// derived) and, together with `Display`, renders the userinfo redacted so the
/// URI is never leaked through logs. `Serialize`/`Deserialize` are intentionally
/// not implemented: nothing in the crate persists this config, and deriving them
/// would risk writing the raw URI to disk or logs.
#[derive(Clone)]
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
        // Redact any credentials embedded in the connection URI before writing.
        let uri = super::redact_uri(&self.uri);
        write!(
            f,
            "{}/{} steps={}, events={} (timeout: {}s)",
            uri, self.database, self.steps_collection, self.events_collection, self.timeout,
        )
    }
}

impl std::fmt::Debug for MongoDBConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak credentials through `{:?}`; render the redacted form.
        write!(f, "MongoDBConfig({})", self)
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
    fn test_display_and_debug_redact_delimiter_passwords() {
        // Whole-URI redaction bounds userinfo by the LAST '@', so passwords
        // containing '/', whitespace, or '@' never leak.
        for pw in ["p/secret-pw", "p secret-pw", "p@secret-pw"] {
            let config = MongoDBConfig {
                uri: format!("mongodb://admin:{pw}@localhost:27017"),
                database: "db".to_string(),
                steps_collection: "steps".to_string(),
                events_collection: "events".to_string(),
                timeout: 30,
            };
            let display = format!("{}", config);
            let debug = format!("{:?}", config);
            assert!(!display.contains(pw), "Display leaked {pw:?}: {display}");
            assert!(!debug.contains(pw), "Debug leaked {pw:?}: {debug}");
            assert!(display.contains("***@localhost:27017"));
            assert!(config.uri.contains(pw));
        }
    }

    #[test]
    fn test_display_and_debug_redact_credentials() {
        let config = MongoDBConfig {
            uri: "mongodb://admin:s3ntinel-pw@localhost:27017".to_string(),
            database: "testdb".to_string(),
            steps_collection: "steps".to_string(),
            events_collection: "events".to_string(),
            timeout: 30,
        };

        let display = format!("{}", config);
        let debug = format!("{:?}", config);

        // Neither Display nor Debug may leak the password or the username.
        assert!(!display.contains("s3ntinel-pw"));
        assert!(!display.contains("admin"));
        assert!(display.contains("***"));
        assert!(!debug.contains("s3ntinel-pw"));
        assert!(!debug.contains("admin"));
        assert!(debug.contains("***"));

        // The raw URI (used to connect) must still carry the real credentials.
        assert!(config.uri.contains("s3ntinel-pw"));
    }
}
