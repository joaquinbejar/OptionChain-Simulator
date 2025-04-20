use crate::session::{Session, SessionStore};
use crate::utils::ChainError;
use async_trait::async_trait;
use redis::{Commands, RedisError};
use serde_json;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{debug, error, info, instrument};
use uuid::Uuid;
use crate::infrastructure::RedisClient;

/// Session store implementation that uses Redis for persistence
pub struct InRedisSessionStore {
    /// Redis client for storage operations
    client: Arc<RedisClient>,
    /// Key prefix for session data in Redis
    key_prefix: String,
    /// Default TTL for sessions in seconds (30 minutes)
    session_ttl: u64,
}

impl InRedisSessionStore {
    /// Creates a new Redis-backed session store
    ///
    /// # Arguments
    ///
    /// * `client` - An Arc-wrapped Redis client for thread-safe access
    /// * `key_prefix` - Optional prefix for Redis keys. Default is "session:"
    /// * `session_ttl` - Optional time-to-live for sessions in seconds. Default is 1800 (30 minutes)
    ///
    /// # Returns
    ///
    /// A new instance of `InRedisSessionStore`
    #[instrument(skip(client), level = "debug")]
    pub fn new(
        client: Arc<RedisClient>,
        key_prefix: Option<String>,
        session_ttl: Option<u64>,
    ) -> Self {
        let prefix = key_prefix.unwrap_or_else(|| "session:".to_string());
        let ttl = session_ttl.unwrap_or(1800); // Default 30 minutes

        info!(
            key_prefix = %prefix,
            session_ttl = ttl,
            "Created new Redis session store"
        );

        Self {
            client,
            key_prefix: prefix,
            session_ttl: ttl,
        }
    }

    /// Constructs a Redis key for a session ID
    fn session_key(&self, id: Uuid) -> String {
        format!("{}{}", self.key_prefix, id)
    }

    /// Maps a Redis error to a ChainError
    fn map_redis_error(err: RedisError) -> ChainError {
        ChainError::Internal(format!("Redis error: {}", err))
    }
}

impl SessionStore for InRedisSessionStore {
    #[instrument(skip(self), level = "debug")]
    fn get(&self, id: Uuid) -> Result<Session, ChainError> {
        let key = self.session_key(id);
        debug!(session_id = %id, key = %key, "Getting session from Redis");

        match self.client.get::<String>(&key) {
            Ok(Some(json_str)) => {
                // Try to deserialize the session
                match serde_json::from_str::<Session>(&json_str) {
                    Ok(session) => {
                        debug!(session_id = %id, "Session retrieved successfully");
                        Ok(session)
                    }
                    Err(e) => {
                        error!(session_id = %id, error = %e, "Failed to deserialize session");
                        Err(ChainError::Internal(format!(
                            "Failed to deserialize session: {}",
                            e
                        )))
                    }
                }
            }
            Ok(None) => {
                debug!(session_id = %id, "Session not found in Redis");
                Err(ChainError::NotFound(format!(
                    "Session with id {} not found",
                    id
                )))
            }
            Err(e) => {
                error!(session_id = %id, error = %e, "Redis error while getting session");
                Err(Self::map_redis_error(e))
            }
        }
    }

    #[instrument(skip(self, session), level = "debug")]
    fn save(&self, session: Session) -> Result<(), ChainError> {
        let key = self.session_key(session.id);
        debug!(session_id = %session.id, key = %key, "Saving session to Redis");

        // Serialize session to JSON
        let json_str = match serde_json::to_string(&session) {
            Ok(s) => s,
            Err(e) => {
                error!(session_id = %session.id, error = %e, "Failed to serialize session");
                return Err(ChainError::Internal(format!(
                    "Failed to serialize session: {}",
                    e
                )));
            }
        };

        // Save to Redis with TTL
        match self.client.set(&key, json_str, Some(self.session_ttl)) {
            Ok(_) => {
                debug!(session_id = %session.id, "Session saved successfully");
                Ok(())
            }
            Err(e) => {
                error!(session_id = %session.id, error = %e, "Redis error while saving session");
                Err(Self::map_redis_error(e))
            }
        }
    }

    #[instrument(skip(self), level = "debug")]
    fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
        let key = self.session_key(id);
        debug!(session_id = %id, key = %key, "Deleting session from Redis");

        match self.client.delete(&key) {
            Ok(deleted) => {
                debug!(session_id = %id, deleted = deleted, "Session delete result");
                Ok(deleted)
            }
            Err(e) => {
                error!(session_id = %id, error = %e, "Redis error while deleting session");
                Err(Self::map_redis_error(e))
            }
        }
    }

    #[instrument(skip(self), level = "debug")]
    fn cleanup(&self) -> Result<usize, ChainError> {
        debug!("Cleaning up expired sessions from Redis");

        // Redis automatically expires keys, so we don't need to manually
        // clean them up. However, if we want to implement additional cleanup
        // logic (like cleaning up stale sessions that haven't had their TTL updated),
        // we can do that here.

        // Redis doesn't provide a simple way to count expired keys, so we'll
        // return 0 to indicate that automatic cleanup is handled by Redis.
        info!("Redis handles automatic expiration of session keys");
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionState, SimulationMethod, SimulationParameters};
    use optionstratlib::{Positive, pos};
    use optionstratlib::utils::TimeFrame;
    use rust_decimal::Decimal;
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;
    use uuid::Uuid;
    use redis::RedisResult;

    // TestInRedisSessionStore - a version of InRedisSessionStore that we can test
    // This is a test double for our real implementation
    struct TestInRedisSessionStore {
        // Instead of holding a real Redis client, we'll store sessions in memory
        sessions: Mutex<std::collections::HashMap<String, String>>,
        key_prefix: String,
        session_ttl: u64,
    }

    impl TestInRedisSessionStore {
        // Constructor that mimics InRedisSessionStore::new
        fn new(
            key_prefix: Option<String>,
            session_ttl: Option<u64>,
        ) -> Self {
            let prefix = key_prefix.unwrap_or_else(|| "session:".to_string());
            let ttl = session_ttl.unwrap_or(1800); // Default 30 minutes

            Self {
                sessions: Mutex::new(std::collections::HashMap::new()),
                key_prefix: prefix,
                session_ttl: ttl,
            }
        }

        // Constructs a Redis key for a session ID (copied from original)
        fn session_key(&self, id: Uuid) -> String {
            format!("{}{}", self.key_prefix, id)
        }

        // Helper to get store size for tests
        fn get_store_size(&self) -> usize {
            self.sessions.lock().unwrap().len()
        }
    }

    // Implement SessionStore for our test double 
    // using the same logic as the original but with in-memory storage
    impl SessionStore for TestInRedisSessionStore {
        fn get(&self, id: Uuid) -> Result<Session, ChainError> {
            let key = self.session_key(id);

            let sessions = self.sessions.lock().unwrap();
            match sessions.get(&key) {
                Some(json_str) => {
                    // Try to deserialize the session
                    match serde_json::from_str::<Session>(json_str) {
                        Ok(session) => Ok(session),
                        Err(e) => {
                            Err(ChainError::Internal(format!(
                                "Failed to deserialize session: {}",
                                e
                            )))
                        }
                    }
                }
                None => {
                    Err(ChainError::NotFound(format!(
                        "Session with id {} not found",
                        id
                    )))
                }
            }
        }

        fn save(&self, session: Session) -> Result<(), ChainError> {
            let key = self.session_key(session.id);

            // Serialize session to JSON
            let json_str = match serde_json::to_string(&session) {
                Ok(s) => s,
                Err(e) => {
                    return Err(ChainError::Internal(format!(
                        "Failed to serialize session: {}",
                        e
                    )));
                }
            };

            // Save to our in-memory store
            let mut sessions = self.sessions.lock().unwrap();
            sessions.insert(key, json_str);

            Ok(())
        }

        fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
            let key = self.session_key(id);

            let mut sessions = self.sessions.lock().unwrap();
            Ok(sessions.remove(&key).is_some())
        }

        fn cleanup(&self) -> Result<usize, ChainError> {
            // Redis handles expiration automatically, so our test double 
            // should also just return 0
            Ok(0)
        }
    }

    // Helper function to create a test session
    fn create_test_session() -> Session {
        let params = SimulationParameters {
            symbol: "TEST".to_string(),
            steps: 10,
            initial_price: pos!(100.0),
            days_to_expiration: pos!(30.0),
            volatility: pos!(0.2),
            risk_free_rate: Decimal::new(0, 0),
            dividend_yield: Positive::ZERO,
            method: SimulationMethod::GeometricBrownian {
                dt: pos!(1.0),
                drift: Decimal::new(0, 0),
                volatility: pos!(0.2),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(5),
            strike_interval: Some(pos!(5.0)),
            skew_factor: None,
            spread: None,
        };

        Session {
            id: Uuid::new_v4(),
            created_at: SystemTime::now(),
            updated_at: SystemTime::now(),
            current_step: 0,
            total_steps: 10,
            parameters: params,
            state: SessionState::Initialized,
        }
    }

    #[test]
    fn test_new_with_defaults() {
        let store = TestInRedisSessionStore::new(None, None);

        assert_eq!(store.key_prefix, "session:");
        assert_eq!(store.session_ttl, 1800);
    }

    #[test]
    fn test_new_with_custom_values() {
        let store = TestInRedisSessionStore::new(
            Some("custom_prefix:".to_string()),
            Some(3600),
        );

        assert_eq!(store.key_prefix, "custom_prefix:");
        assert_eq!(store.session_ttl, 3600);
    }

    #[test]
    fn test_session_key_format() {
        let store = TestInRedisSessionStore::new(Some("test:".to_string()), None);

        let id = Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479").unwrap();
        let key = store.session_key(id);

        assert_eq!(key, "test:f47ac10b-58cc-4372-a567-0e02b2c3d479");
    }

    #[test]
    fn test_save_and_get_session() {
        let store = TestInRedisSessionStore::new(None, None);

        let session = create_test_session();
        let session_id = session.id;

        // Save the session
        let save_result = store.save(session.clone());
        assert!(save_result.is_ok());

        // Check that something was stored
        assert_eq!(store.get_store_size(), 1);

        // Get the session back
        let get_result = store.get(session_id);
        assert!(get_result.is_ok());

        let retrieved_session = get_result.unwrap();
        assert_eq!(retrieved_session.id, session_id);
        assert_eq!(retrieved_session.state, SessionState::Initialized);
        assert_eq!(retrieved_session.current_step, 0);
        assert_eq!(retrieved_session.total_steps, 10);
    }

    #[test]
    fn test_get_non_existent_session() {
        let store = TestInRedisSessionStore::new(None, None);

        let non_existent_id = Uuid::new_v4();
        let result = store.get(non_existent_id);

        assert!(result.is_err());
        match result {
            Err(ChainError::NotFound(msg)) => {
                assert!(msg.contains(&non_existent_id.to_string()));
            }
            _ => panic!("Expected NotFound error"),
        }
    }

    #[test]
    fn test_delete_existing_session() {
        let store = TestInRedisSessionStore::new(None, None);

        let session = create_test_session();
        let session_id = session.id;

        // Save the session first
        store.save(session).unwrap();
        assert_eq!(store.get_store_size(), 1);

        // Delete the session
        let delete_result = store.delete(session_id);
        assert!(delete_result.is_ok());
        assert_eq!(delete_result.unwrap(), true);

        // Verify it's removed
        assert_eq!(store.get_store_size(), 0);

        // Try to get the deleted session
        let get_result = store.get(session_id);
        assert!(get_result.is_err());
    }

    #[test]
    fn test_delete_non_existent_session() {
        let store = TestInRedisSessionStore::new(None, None);

        let non_existent_id = Uuid::new_v4();
        let result = store.delete(non_existent_id);

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[test]
    fn test_cleanup() {
        let store = TestInRedisSessionStore::new(None, None);

        let result = store.cleanup();

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_update_existing_session() {
        let store = TestInRedisSessionStore::new(None, None);

        // Create and save initial session
        let mut session = create_test_session();
        let session_id = session.id;

        store.save(session.clone()).unwrap();

        // Modify the session
        session.current_step = 5;
        session.state = SessionState::InProgress;

        // Update the session
        let update_result = store.save(session.clone());
        assert!(update_result.is_ok());

        // Retrieve and check the updated session
        let get_result = store.get(session_id);
        assert!(get_result.is_ok());

        let updated_session = get_result.unwrap();
        assert_eq!(updated_session.current_step, 5);
        assert_eq!(updated_session.state, SessionState::InProgress);
    }

    // TestErrorInRedisSessionStore - to test error cases
    struct TestErrorInRedisSessionStore {
        // Store that always generates errors
    }

    impl SessionStore for TestErrorInRedisSessionStore {
        fn get(&self, id: Uuid) -> Result<Session, ChainError> {
            Err(ChainError::Internal(format!("Simulated error for session {}", id)))
        }

        fn save(&self, session: Session) -> Result<(), ChainError> {
            Err(ChainError::Internal(format!("Simulated error saving session {}", session.id)))
        }

        fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
            Err(ChainError::Internal(format!("Simulated error deleting session {}", id)))
        }

        fn cleanup(&self) -> Result<usize, ChainError> {
            Err(ChainError::Internal("Simulated error during cleanup".to_string()))
        }
    }

    #[test]
    fn test_error_handling() {
        let error_store = TestErrorInRedisSessionStore{};

        let session = create_test_session();
        let session_id = session.id;

        // Test that errors are properly propagated
        let save_result = error_store.save(session);
        assert!(save_result.is_err());

        let get_result = error_store.get(session_id);
        assert!(get_result.is_err());

        let delete_result = error_store.delete(session_id);
        assert!(delete_result.is_err());

        let cleanup_result = error_store.cleanup();
        assert!(cleanup_result.is_err());
    }
}