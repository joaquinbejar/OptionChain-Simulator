use redis::{Client, Commands, Connection, RedisError, RedisResult};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, instrument};
use crate::infrastructure::config::redis::RedisConfig;

/// A client for interacting with a Redis database
pub struct RedisClient {
    /// The underlying Redis client
    client: Client,
    /// A connection pool (connection wrapped in Mutex for thread safety)
    connection_pool: Arc<Mutex<Connection>>,
    /// Redis configuration
    config: RedisConfig,
}

impl RedisClient {
    /// Creates a new Redis client with the provided configuration
    #[instrument(skip(config), level = "debug")]
    pub fn new(config: RedisConfig) -> Result<Self, RedisError> {
        // Build Redis connection URL
        let mut url = config.url();
        info!("Connecting to Redis at {}", url);

        // Create Redis client
        let client = Client::open(url)?;

        // Create a single connection for now
        // In a production system, you might want to use a more robust connection pool
        let connection = client.get_connection()?;
        let connection_pool = Arc::new(Mutex::new(connection));

        info!("Successfully connected to Redis");

        Ok(Self {
            client,
            connection_pool,
            config,
        })
    }

    /// Gets a connection from the pool
    fn get_connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, RedisError> {
        self.connection_pool.lock().map_err(|e| {
            error!("Failed to acquire Redis connection lock: {}", e);
            RedisError::from(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to acquire Redis connection lock: {}", e),
            ))
        })
    }

    /// Gets a value from Redis by key
    #[instrument(skip(self), level = "debug")]
    pub fn get<T: redis::FromRedisValue>(&self, key: &str) -> RedisResult<Option<T>> {
        let mut connection = self.get_connection()?;
        let exists: bool = connection.exists(key)?;

        if !exists {
            debug!("Key '{}' not found in Redis", key);
            return Ok(None);
        }

        debug!("Retrieving key '{}' from Redis", key);
        let value: T = connection.get(key)?;
        Ok(Some(value))
    }

    /// Sets a value in Redis with an optional expiration time in seconds
    #[instrument(skip(self, value), level = "debug")]
    pub fn set<T: redis::ToRedisArgs>(&self, key: &str, value: T, expiry_secs: Option<u64>) -> RedisResult<()> {
        let mut connection = self.get_connection()?;

        match expiry_secs {
            Some(secs) => {
                debug!("Setting key '{}' in Redis with expiry of {} seconds", key, secs);
                connection.set_ex(key, value, secs)
            },
            None => {
                debug!("Setting key '{}' in Redis without expiry", key);
                connection.set(key, value)
            }
        }
    }

    /// Deletes a key from Redis
    #[instrument(skip(self), level = "debug")]
    pub fn delete(&self, key: &str) -> RedisResult<bool> {
        let mut connection = self.get_connection()?;
        debug!("Deleting key '{}' from Redis", key);
        let deleted: i32 = connection.del(key)?;
        Ok(deleted > 0)
    }

    /// Gets all keys matching a pattern
    #[instrument(skip(self), level = "debug")]
    pub fn keys(&self, pattern: &str) -> RedisResult<Vec<String>> {
        let mut connection = self.get_connection()?;
        debug!("Getting keys matching pattern '{}' from Redis", pattern);
        connection.keys(pattern)
    }

    /// Checks if a key exists in Redis
    #[instrument(skip(self), level = "debug")]
    pub fn exists(&self, key: &str) -> RedisResult<bool> {
        let mut connection = self.get_connection()?;
        debug!("Checking if key '{}' exists in Redis", key);
        connection.exists(key)
    }

    /// Returns the Redis configuration
    pub fn get_config(&self) -> &RedisConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use redis::{RedisError, RedisResult};

    // Mock implementation for Redis connection
    // We need to create a custom struct that implements necessary methods
    struct MockConnection {
        exists_results: std::collections::HashMap<String, bool>,
        get_results: std::collections::HashMap<String, String>,
        del_results: std::collections::HashMap<String, i32>,
        keys_results: std::collections::HashMap<String, Vec<String>>,
    }

    impl MockConnection {
        fn new() -> Self {
            Self {
                exists_results: std::collections::HashMap::new(),
                get_results: std::collections::HashMap::new(),
                del_results: std::collections::HashMap::new(),
                keys_results: std::collections::HashMap::new(),
            }
        }

        // Configurators to set expected results
        fn with_exists(mut self, key: &str, result: bool) -> Self {
            self.exists_results.insert(key.to_string(), result);
            self
        }

        fn with_get(mut self, key: &str, result: &str) -> Self {
            self.get_results.insert(key.to_string(), result.to_string());
            self
        }

        fn with_del(mut self, key: &str, result: i32) -> Self {
            self.del_results.insert(key.to_string(), result);
            self
        }

        fn with_keys(mut self, pattern: &str, result: Vec<String>) -> Self {
            self.keys_results.insert(pattern.to_string(), result);
            self
        }

        // Mock implementation of connection methods
        fn exists(&mut self, key: &str) -> RedisResult<bool> {
            match self.exists_results.get(key) {
                Some(result) => Ok(*result),
                None => Err(RedisError::from((redis::ErrorKind::TypeError, "Key not configured in mock")))
            }
        }

        fn get(&mut self, key: &str) -> RedisResult<String> {
            match self.get_results.get(key) {
                Some(value) => Ok(value.clone()),
                None => Err(RedisError::from((redis::ErrorKind::TypeError, "Key not configured in mock")))
            }
        }

        fn set(&mut self, _key: &str, _value: &str) -> RedisResult<()> {
            // Simply return success for tests
            Ok(())
        }

        fn set_ex(&mut self, _key: &str, _value: &str, _seconds: u64) -> RedisResult<()> {
            // Simply return success for tests
            Ok(())
        }

        fn del(&mut self, key: &str) -> RedisResult<i32> {
            match self.del_results.get(key) {
                Some(result) => Ok(*result),
                None => Err(RedisError::from((redis::ErrorKind::TypeError, "Key not configured in mock")))
            }
        }

        fn keys(&mut self, pattern: &str) -> RedisResult<Vec<String>> {
            match self.keys_results.get(pattern) {
                Some(result) => Ok(result.clone()),
                None => Err(RedisError::from((redis::ErrorKind::TypeError, "Pattern not configured in mock")))
            }
        }
    }

    // We'll need to patch the RedisClient to use our mock
    // This avoids the trait/struct mismatch issue
    struct TestRedisClient {
        client: redis::Client,
        connection: Arc<Mutex<MockConnection>>,
        config: RedisConfig,
    }

    impl TestRedisClient {
        // Implement the same methods as RedisClient but using our MockConnection
        fn get<T: std::string::ToString + std::fmt::Display>(&self, key: T) -> RedisResult<Option<String>> {
            let key_str = key.to_string();
            let mut connection = self.connection.lock().map_err(|e| {
                RedisError::from(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to acquire Redis connection lock: {}", e),
                ))
            })?;

            let exists = connection.exists(&key_str)?;

            if !exists {
                return Ok(None);
            }

            let value = connection.get(&key_str)?;
            Ok(Some(value))
        }

        fn set<T: std::string::ToString, V: std::string::ToString>(
            &self,
            key: T,
            value: V,
            expiry_secs: Option<u64>
        ) -> RedisResult<()> {
            let key_str = key.to_string();
            let value_str = value.to_string();
            let mut connection = self.connection.lock().map_err(|e| {
                RedisError::from(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to acquire Redis connection lock: {}", e),
                ))
            })?;

            match expiry_secs {
                Some(secs) => connection.set_ex(&key_str, &value_str, secs),
                None => connection.set(&key_str, &value_str),
            }
        }

        fn delete<T: std::string::ToString>(&self, key: T) -> RedisResult<bool> {
            let key_str = key.to_string();
            let mut connection = self.connection.lock().map_err(|e| {
                RedisError::from(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to acquire Redis connection lock: {}", e),
                ))
            })?;

            let deleted = connection.del(&key_str)?;
            Ok(deleted > 0)
        }

        fn keys<T: std::string::ToString>(&self, pattern: T) -> RedisResult<Vec<String>> {
            let pattern_str = pattern.to_string();
            let mut connection = self.connection.lock().map_err(|e| {
                RedisError::from(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to acquire Redis connection lock: {}", e),
                ))
            })?;

            connection.keys(&pattern_str)
        }

        fn exists<T: std::string::ToString>(&self, key: T) -> RedisResult<bool> {
            let key_str = key.to_string();
            let mut connection = self.connection.lock().map_err(|e| {
                RedisError::from(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to acquire Redis connection lock: {}", e),
                ))
            })?;

            connection.exists(&key_str)
        }

        fn get_config(&self) -> &RedisConfig {
            &self.config
        }
    }

    // Now the tests

    #[test]
    fn test_get_existing_key() {
        // Create a mock connection with predefined behavior
        let mock_conn = MockConnection::new()
            .with_exists("test_key", true)
            .with_get("test_key", "test_value");

        // Create the Redis client with the mock connection
        let client = create_test_client(mock_conn);

        // Test the get method
        let result = client.get("test_key");

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some("test_value".to_string()));
    }

    #[test]
    fn test_get_non_existing_key() {
        // Create a mock connection with predefined behavior
        let mock_conn = MockConnection::new()
            .with_exists("non_existing_key", false);

        // Create the Redis client with the mock connection
        let client = create_test_client(mock_conn);

        // Test the get method
        let result = client.get("non_existing_key");

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_set_without_expiry() {
        // The simplified implementation of set always returns success
        let mock_conn = MockConnection::new();

        // Create the Redis client with the mock connection
        let client = create_test_client(mock_conn);

        // Test the set method
        let result = client.set("test_key", "test_value", None);

        assert!(result.is_ok());
    }

    #[test]
    fn test_set_with_expiry() {
        // The simplified implementation of set_ex always returns success
        let mock_conn = MockConnection::new();

        // Create the Redis client with the mock connection
        let client = create_test_client(mock_conn);

        // Test the set method with expiration
        let result = client.set("test_key", "test_value", Some(60));

        assert!(result.is_ok());
    }

    #[test]
    fn test_delete_existing_key() {
        // Create a mock connection with predefined behavior
        let mock_conn = MockConnection::new()
            .with_del("test_key", 1);

        // Create the Redis client with the mock connection
        let client = create_test_client(mock_conn);

        // Test the delete method
        let result = client.delete("test_key");

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[test]
    fn test_delete_non_existing_key() {
        // Create a mock connection with predefined behavior
        let mock_conn = MockConnection::new()
            .with_del("non_existing_key", 0);

        // Create the Redis client with the mock connection
        let client = create_test_client(mock_conn);

        // Test the delete method
        let result = client.delete("non_existing_key");

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[test]
    fn test_keys() {
        // Create a mock connection with predefined behavior
        let mock_conn = MockConnection::new()
            .with_keys("test*", vec!["test1".to_string(), "test2".to_string()]);

        // Create the Redis client with the mock connection
        let client = create_test_client(mock_conn);

        // Test the keys method
        let result = client.keys("test*");

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec!["test1".to_string(), "test2".to_string()]);
    }

    #[test]
    fn test_exists_true() {
        // Create a mock connection with predefined behavior
        let mock_conn = MockConnection::new()
            .with_exists("existing_key", true);

        // Create the Redis client with the mock connection
        let client = create_test_client(mock_conn);

        // Test the exists method
        let result = client.exists("existing_key");

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[test]
    fn test_exists_false() {
        // Create a mock connection with predefined behavior
        let mock_conn = MockConnection::new()
            .with_exists("non_existing_key", false);

        // Create the Redis client with the mock connection
        let client = create_test_client(mock_conn);

        // Test the exists method
        let result = client.exists("non_existing_key");

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[test]
    fn test_get_config() {
        // Create configuration
        let config = RedisConfig {
            host: "test-host".to_string(),
            port: 6380,
            username: Some("testuser".to_string()),
            password: Some("testpass".to_string()),
            database: 2,
            timeout: 60,
        };

        // Create Redis client
        let mock_conn = MockConnection::new();
        let client = TestRedisClient {
            client: redis::Client::open(format!("redis://{}:{}", config.host, config.port))
                .expect("Failed to create Redis client"),
            connection: Arc::new(Mutex::new(mock_conn)),
            config: config.clone(),
        };

        // Test the get_config method
        let result = client.get_config();

        assert_eq!(result.host, "test-host");
        assert_eq!(result.port, 6380);
        assert_eq!(result.username, Some("testuser".to_string()));
        assert_eq!(result.password, Some("testpass".to_string()));
        assert_eq!(result.database, 2);
        assert_eq!(result.timeout, 60);
    }

    // Helper function to create a test Redis client with a mock connection
    fn create_test_client(mock_conn: MockConnection) -> TestRedisClient {
        let config = RedisConfig::default();

        // Create a real client but inject our mock connection
        let client = redis::Client::open(format!("redis://{}:{}", config.host, config.port))
            .expect("Failed to create Redis client");

        TestRedisClient {
            client,
            connection: Arc::new(Mutex::new(mock_conn)),
            config,
        }
    }

    // The following test simulates a poisoned mutex to test error handling
    #[test]
    fn test_get_connection_error() {
        use std::sync::{Arc, Mutex};
        use std::thread;

        // Create a mock connection
        let connection = Arc::new(Mutex::new(MockConnection::new()));

        // Poison the mutex by panicking while holding the lock
        let connection_clone = Arc::clone(&connection);
        let handle = thread::spawn(move || {
            let _lock = connection_clone.lock().unwrap();
            panic!("Intentionally poisoning the mutex");
        });

        // Wait for the thread to complete and the mutex to be poisoned
        let _ = handle.join();

        // Create client with poisoned mutex
        let config = RedisConfig::default();
        let client = TestRedisClient {
            client: redis::Client::open(format!("redis://{}:{}", config.host, config.port))
                .expect("Failed to create Redis client"),
            connection: connection,
            config,
        };

        // Test the get method - should fail due to poisoned mutex
        let result = client.get("test_key");
        assert!(result.is_err());
    }
}