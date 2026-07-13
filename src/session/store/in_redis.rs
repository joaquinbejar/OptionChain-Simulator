use crate::infrastructure::RedisClient;
use crate::session::{Session, SessionStore};
use crate::utils::ChainError;
use async_trait::async_trait;
use redis::RedisError;
use serde_json;
use std::sync::Arc;
use tracing::{debug, error, info, instrument};
use uuid::Uuid;

/// Session store implementation that uses Redis for persistence
pub struct InRedisSessionStore {
    /// Redis client for storage operations
    client: Arc<RedisClient>,
    /// Key prefix for session data in Redis
    key_prefix: String,
    /// Default TTL for sessions in seconds (30 minutes)
    session_ttl: u64,
}

/// Server-side Lua for an atomic session compare-and-swap.
///
/// Redis runs a script as a single, uninterruptible unit, so the two GETs, the
/// version comparison, and the conditional SETs happen without any interleaving
/// from another connection — this is what makes the CAS race-free.
///
/// The version compared for the CAS is NOT read from the JSON document: decoding
/// it with `cjson` would round the integer through an IEEE-754 double, which
/// cannot represent every `u64`, so two adjacent revisions above `2^53` collapse
/// to the same value and a stale writer could pass the check. Instead the version
/// lives in a COMPANION key (`{session_key}:ver`) as a plain integer STRING and is
/// compared as a string (`ver ~= ARGV[2]`), i.e. an exact byte comparison with no
/// floating-point rounding. A session written before this companion key existed
/// has no `:ver` key; `GET` then returns `false`, so `or '0'` treats it as
/// revision `0`.
///
/// Keys: `KEYS[1]` = session key; `KEYS[2]` = companion version key.
/// Arguments: `ARGV[1]` = new session JSON; `ARGV[2]` = expected version string;
/// `ARGV[3]` = new version string; `ARGV[4]` = TTL in seconds. Both keys are
/// rewritten with the same TTL so they always expire together.
///
/// Return codes: `-1` = key missing (NotFound); `-2` = version mismatch
/// (Conflict); `1` = written.
const SAVE_CAS_SCRIPT: &str = r#"
local cur = redis.call('GET', KEYS[1])
if not cur then
    return -1
end
local ver = redis.call('GET', KEYS[2]) or '0'
if ver ~= ARGV[2] then
    return -2
end
redis.call('SET', KEYS[1], ARGV[1], 'EX', tonumber(ARGV[4]))
redis.call('SET', KEYS[2], ARGV[3], 'EX', tonumber(ARGV[4]))
return 1
"#;

/// Translates the integer result code returned by [`SAVE_CAS_SCRIPT`] into the
/// crate's `ChainError` boundary. Pure and side-effect free so the mapping is
/// unit-testable without a live Redis.
///
/// - `1` → `Ok(())` (the CAS committed);
/// - `-1` → [`ChainError::NotFound`] (no session at that key);
/// - `-2` → [`ChainError::Conflict`] (stored revision differed);
/// - anything else → [`ChainError::Internal`] (script contract violation).
fn map_cas_result(code: i64, id: Uuid, expected_version: u64) -> Result<(), ChainError> {
    match code {
        1 => Ok(()),
        -1 => Err(ChainError::NotFound(format!(
            "Session with id {} not found",
            id
        ))),
        -2 => Err(ChainError::Conflict(format!(
            "Session {} was modified concurrently (expected version {})",
            id, expected_version
        ))),
        other => Err(ChainError::Internal(format!(
            "Unexpected CAS result code {} for session {}",
            other, id
        ))),
    }
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

    /// Constructs the companion version key for a session ID (`{session_key}:ver`).
    ///
    /// This key holds the optimistic-concurrency revision as an integer STRING so
    /// [`SAVE_CAS_SCRIPT`] can compare it exactly, avoiding the IEEE-754 rounding
    /// that a `cjson`-decoded `u64` would suffer above `2^53`.
    fn version_key(&self, id: Uuid) -> String {
        format!("{}:ver", self.session_key(id))
    }

    /// Maps a Redis error to a ChainError
    fn map_redis_error(err: RedisError) -> ChainError {
        ChainError::Internal(format!("Redis error: {}", err))
    }
}

#[async_trait]
impl SessionStore for InRedisSessionStore {
    #[instrument(skip(self), level = "debug")]
    async fn get(&self, id: Uuid) -> Result<Session, ChainError> {
        let key = self.session_key(id);
        debug!(session_id = %id, key = %key, "Getting session from Redis");

        match self.client.get::<String>(&key).await {
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
    async fn create(&self, session: Session) -> Result<(), ChainError> {
        let key = self.session_key(session.id);
        let version_key = self.version_key(session.id);
        debug!(session_id = %session.id, key = %key, "Creating session in Redis");

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

        // SET NX guarantees we never overwrite an existing session id.
        match self
            .client
            .set_nx(&key, json_str, Some(self.session_ttl))
            .await
        {
            Ok(true) => {
                // Seed the companion version key the CAS script compares against.
                // create/save/delete are NOT the compare-and-swap race surface (only
                // `save_cas` races), so plain sequential commands are fine here; the
                // script defaults a missing `:ver` key to "0", so even a half-written
                // pair still compares safely.
                self.client
                    .set(
                        &version_key,
                        session.version.to_string(),
                        Some(self.session_ttl),
                    )
                    .await
                    .map_err(Self::map_redis_error)?;
                debug!(session_id = %session.id, "Session created successfully");
                Ok(())
            }
            Ok(false) => {
                error!(session_id = %session.id, "Session id already exists in Redis");
                Err(ChainError::AlreadyExists(format!(
                    "Session with id {} already exists",
                    session.id
                )))
            }
            Err(e) => {
                error!(session_id = %session.id, error = %e, "Redis error while creating session");
                Err(Self::map_redis_error(e))
            }
        }
    }

    #[instrument(skip(self, session), level = "debug")]
    async fn save(&self, session: Session) -> Result<(), ChainError> {
        let key = self.session_key(session.id);
        let version_key = self.version_key(session.id);
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

        // Blind upsert: write the document, then keep the companion version key in
        // sync. `save` is not the compare-and-swap race surface (see `create`), so
        // two sequential SETs are fine — a torn write only leaves the pair briefly
        // inconsistent and the CAS script tolerates a missing `:ver` key.
        self.client
            .set(&key, json_str, Some(self.session_ttl))
            .await
            .map_err(Self::map_redis_error)?;
        self.client
            .set(
                &version_key,
                session.version.to_string(),
                Some(self.session_ttl),
            )
            .await
            .map_err(Self::map_redis_error)?;

        debug!(session_id = %session.id, "Session saved successfully");
        Ok(())
    }

    #[instrument(skip(self, session), level = "debug")]
    async fn save_cas(&self, session: Session, expected_version: u64) -> Result<(), ChainError> {
        let id = session.id;
        let key = self.session_key(id);
        let version_key = self.version_key(id);
        debug!(session_id = %id, key = %key, expected_version, "CAS-saving session to Redis");

        // Serialize session to JSON
        let json_str = match serde_json::to_string(&session) {
            Ok(s) => s,
            Err(e) => {
                error!(session_id = %id, error = %e, "Failed to serialize session");
                return Err(ChainError::Internal(format!(
                    "Failed to serialize session: {}",
                    e
                )));
            }
        };

        // Run the compare-and-swap atomically server-side. The manager (the block
        // scheduler) supplies `expected_version`; the script only writes when the
        // companion version key still matches, so a concurrent advance cannot be
        // lost. Versions cross the boundary as STRINGS so the Lua comparison is exact
        // for every `u64` (no cjson double rounding). `session.version` was already
        // bumped past `expected_version` by the manager.
        let mut conn = self.client.connection_manager();
        let code: i64 = redis::Script::new(SAVE_CAS_SCRIPT)
            .key(&key)
            .key(&version_key)
            .arg(json_str)
            .arg(expected_version.to_string())
            .arg(session.version.to_string())
            .arg(self.session_ttl)
            .invoke_async(&mut conn)
            .await
            .map_err(Self::map_redis_error)?;

        map_cas_result(code, id, expected_version).inspect_err(|e| {
            debug!(session_id = %id, error = %e, "CAS save rejected");
        })
    }

    #[instrument(skip(self), level = "debug")]
    async fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
        let key = self.session_key(id);
        let version_key = self.version_key(id);
        debug!(session_id = %id, key = %key, "Deleting session from Redis");

        // Remove BOTH the session document and its companion version key. Sequential
        // deletes are fine (delete is not the CAS race surface); the session key's
        // result is what determines whether a session actually existed, and deleting
        // an already-absent `:ver` key is harmless.
        let deleted = self
            .client
            .delete(&key)
            .await
            .map_err(Self::map_redis_error)?;
        self.client
            .delete(&version_key)
            .await
            .map_err(Self::map_redis_error)?;

        debug!(session_id = %id, deleted = deleted, "Session delete result");
        Ok(deleted)
    }

    #[instrument(skip(self), level = "debug")]
    async fn cleanup(&self) -> Result<usize, ChainError> {
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
    use crate::infrastructure::RedisConfig;
    use crate::session::{SessionState, SimulationMethod, SimulationParameters};
    use optionstratlib::utils::TimeFrame;
    use positive::{Positive, pos_or_panic};
    use rust_decimal::Decimal;
    use std::sync::Mutex;
    use std::time::SystemTime;
    use uuid::Uuid;

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
        fn new(key_prefix: Option<String>, session_ttl: Option<u64>) -> Self {
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

        // Companion version key mirroring the real store (`{session_key}:ver`).
        fn version_key(&self, id: Uuid) -> String {
            format!("{}:ver", self.session_key(id))
        }

        // Helper to get store size for tests. Counts only session documents, not the
        // companion `:ver` keys, so "one session" reads as size 1 as before.
        fn get_store_size(&self) -> usize {
            self.sessions
                .lock()
                .unwrap()
                .keys()
                .filter(|k| !k.ends_with(":ver"))
                .count()
        }
    }

    // Implement SessionStore for our test double
    // using the same logic as the original but with in-memory storage
    #[async_trait]
    impl SessionStore for TestInRedisSessionStore {
        async fn get(&self, id: Uuid) -> Result<Session, ChainError> {
            let key = self.session_key(id);

            let sessions = self.sessions.lock().unwrap();
            match sessions.get(&key) {
                Some(json_str) => {
                    // Try to deserialize the session
                    match serde_json::from_str::<Session>(json_str) {
                        Ok(session) => Ok(session),
                        Err(e) => Err(ChainError::Internal(format!(
                            "Failed to deserialize session: {}",
                            e
                        ))),
                    }
                }
                None => Err(ChainError::NotFound(format!(
                    "Session with id {} not found",
                    id
                ))),
            }
        }

        async fn create(&self, session: Session) -> Result<(), ChainError> {
            let key = self.session_key(session.id);
            let version_key = self.version_key(session.id);

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

            // Mirror the real store's SET NX behaviour: reject id collisions, and
            // seed the companion version key alongside the document.
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.contains_key(&key) {
                return Err(ChainError::AlreadyExists(format!(
                    "Session with id {} already exists",
                    session.id
                )));
            }
            sessions.insert(key, json_str);
            sessions.insert(version_key, session.version.to_string());

            Ok(())
        }

        async fn save(&self, session: Session) -> Result<(), ChainError> {
            let key = self.session_key(session.id);
            let version_key = self.version_key(session.id);

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

            // Upsert the document and keep the companion version key in sync.
            let mut sessions = self.sessions.lock().unwrap();
            sessions.insert(key, json_str);
            sessions.insert(version_key, session.version.to_string());

            Ok(())
        }

        async fn save_cas(
            &self,
            session: Session,
            expected_version: u64,
        ) -> Result<(), ChainError> {
            let key = self.session_key(session.id);
            let version_key = self.version_key(session.id);

            let json_str = match serde_json::to_string(&session) {
                Ok(s) => s,
                Err(e) => {
                    return Err(ChainError::Internal(format!(
                        "Failed to serialize session: {}",
                        e
                    )));
                }
            };

            // Mirror the Lua CAS: the companion version key is the source of truth.
            // A missing `:ver` key defaults to 0 (a legacy session written before the
            // companion key existed); compare it exactly, then rewrite both keys.
            let mut sessions = self.sessions.lock().unwrap();
            if !sessions.contains_key(&key) {
                return Err(ChainError::NotFound(format!(
                    "Session with id {} not found",
                    session.id
                )));
            }
            let stored_version: u64 = sessions
                .get(&version_key)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            if stored_version != expected_version {
                return Err(ChainError::Conflict(format!(
                    "Session {} was modified concurrently (expected version {}, found {})",
                    session.id, expected_version, stored_version
                )));
            }
            sessions.insert(key, json_str);
            sessions.insert(version_key, session.version.to_string());

            Ok(())
        }

        async fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
            let key = self.session_key(id);
            let version_key = self.version_key(id);

            // Remove both keys; the session document's presence decides the result.
            let mut sessions = self.sessions.lock().unwrap();
            let removed = sessions.remove(&key).is_some();
            sessions.remove(&version_key);
            Ok(removed)
        }

        async fn cleanup(&self) -> Result<usize, ChainError> {
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
            initial_price: pos_or_panic!(100.0),
            days_to_expiration: pos_or_panic!(30.0),
            volatility: pos_or_panic!(0.2),
            risk_free_rate: Decimal::new(0, 0),
            dividend_yield: Positive::ZERO,
            method: SimulationMethod::GeometricBrownian {
                dt: pos_or_panic!(1.0),
                drift: Decimal::new(0, 0),
                volatility: pos_or_panic!(0.2),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(5),
            strike_interval: Some(pos_or_panic!(5.0)),
            skew_slope: None,
            smile_curve: None,
            spread: None,
            seed: None,
        };

        Session {
            id: Uuid::new_v4(),
            created_at: SystemTime::now(),
            updated_at: SystemTime::now(),
            current_step: 0,
            total_steps: 10,
            parameters: params,
            state: SessionState::Initialized,
            version: 0,
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
        let store = TestInRedisSessionStore::new(Some("custom_prefix:".to_string()), Some(3600));

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

    #[tokio::test]
    async fn test_save_and_get_session() {
        let store = TestInRedisSessionStore::new(None, None);

        let session = create_test_session();
        let session_id = session.id;

        // Save the session
        let save_result = store.save(session.clone()).await;
        assert!(save_result.is_ok());

        // Check that something was stored
        assert_eq!(store.get_store_size(), 1);

        // Get the session back
        let get_result = store.get(session_id).await;
        assert!(get_result.is_ok());

        let retrieved_session = get_result.unwrap();
        assert_eq!(retrieved_session.id, session_id);
        assert_eq!(retrieved_session.state, SessionState::Initialized);
        assert_eq!(retrieved_session.current_step, 0);
        assert_eq!(retrieved_session.total_steps, 10);
    }

    #[tokio::test]
    async fn test_create_new_session() {
        let store = TestInRedisSessionStore::new(None, None);

        let session = create_test_session();
        let session_id = session.id;

        // create succeeds on a fresh id
        assert!(store.create(session).await.is_ok());
        assert_eq!(store.get_store_size(), 1);

        // and the session is retrievable
        assert!(store.get(session_id).await.is_ok());
    }

    #[tokio::test]
    async fn test_create_duplicate_returns_already_exists() {
        let store = TestInRedisSessionStore::new(None, None);

        let session = create_test_session();

        // first create wins
        assert!(store.create(session.clone()).await.is_ok());

        // second create with the same id is rejected instead of overwriting
        match store.create(session).await {
            Err(ChainError::AlreadyExists(msg)) => {
                assert!(msg.contains("already exists"));
            }
            other => panic!("Expected AlreadyExists error, got {:?}", other),
        }

        // still exactly one stored entry (no overwrite / no duplicate)
        assert_eq!(store.get_store_size(), 1);
    }

    #[tokio::test]
    async fn test_save_still_updates_after_create() {
        let store = TestInRedisSessionStore::new(None, None);

        let mut session = create_test_session();
        let session_id = session.id;

        assert!(store.create(session.clone()).await.is_ok());

        // save is still an upsert on top of a created session
        session.current_step = 3;
        session.state = SessionState::InProgress;
        assert!(store.save(session).await.is_ok());

        let updated = store.get(session_id).await.unwrap();
        assert_eq!(updated.current_step, 3);
        assert_eq!(updated.state, SessionState::InProgress);
    }

    #[tokio::test]
    async fn test_get_non_existent_session() {
        let store = TestInRedisSessionStore::new(None, None);

        let non_existent_id = Uuid::new_v4();
        let result = store.get(non_existent_id).await;

        assert!(result.is_err());
        match result {
            Err(ChainError::NotFound(msg)) => {
                assert!(msg.contains(&non_existent_id.to_string()));
            }
            _ => panic!("Expected NotFound error"),
        }
    }

    #[tokio::test]
    async fn test_delete_existing_session() {
        let store = TestInRedisSessionStore::new(None, None);

        let session = create_test_session();
        let session_id = session.id;

        // Save the session first
        store.save(session).await.unwrap();
        assert_eq!(store.get_store_size(), 1);

        // Delete the session
        let delete_result = store.delete(session_id).await;
        assert!(delete_result.is_ok());
        assert!(delete_result.unwrap());

        // Verify it's removed
        assert_eq!(store.get_store_size(), 0);

        // Try to get the deleted session
        let get_result = store.get(session_id).await;
        assert!(get_result.is_err());
    }

    #[tokio::test]
    async fn test_delete_non_existent_session() {
        let store = TestInRedisSessionStore::new(None, None);

        let non_existent_id = Uuid::new_v4();
        let result = store.delete(non_existent_id).await;

        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn test_cleanup() {
        let store = TestInRedisSessionStore::new(None, None);

        let result = store.cleanup().await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_update_existing_session() {
        let store = TestInRedisSessionStore::new(None, None);

        // Create and save initial session
        let mut session = create_test_session();
        let session_id = session.id;

        store.save(session.clone()).await.unwrap();

        // Modify the session
        session.current_step = 5;
        session.state = SessionState::InProgress;

        // Update the session
        let update_result = store.save(session.clone()).await;
        assert!(update_result.is_ok());

        // Retrieve and check the updated session
        let get_result = store.get(session_id).await;
        assert!(get_result.is_ok());

        let updated_session = get_result.unwrap();
        assert_eq!(updated_session.current_step, 5);
        assert_eq!(updated_session.state, SessionState::InProgress);
    }

    /// Issue #8: the CAS Lua script is the atomicity contract. Assert the string
    /// constant carries the exact server-side logic the store relies on: read the
    /// session via GET, compare the revision against the COMPANION version key as an
    /// exact string (not a `cjson`-decoded double that collapses above 2^53),
    /// default a missing companion key to legacy "0", and the three documented
    /// return codes.
    #[test]
    fn test_save_cas_script_encodes_the_contract() {
        assert!(SAVE_CAS_SCRIPT.contains("redis.call('GET', KEYS[1])")); // session doc
        assert!(SAVE_CAS_SCRIPT.contains("redis.call('GET', KEYS[2])")); // companion version key
        assert!(SAVE_CAS_SCRIPT.contains("or '0'")); // missing version → legacy 0
        assert!(SAVE_CAS_SCRIPT.contains("ver ~= ARGV[2]")); // exact string compare, no doubles
        assert!(!SAVE_CAS_SCRIPT.contains("cjson")); // the lossy decoder is gone
        assert!(SAVE_CAS_SCRIPT.contains("return -1")); // NotFound
        assert!(SAVE_CAS_SCRIPT.contains("return -2")); // Conflict
        assert!(SAVE_CAS_SCRIPT.contains("return 1")); // committed
        assert!(SAVE_CAS_SCRIPT.contains("'EX'")); // TTL preserved on write
    }

    /// Issue #8: the pure return-code mapper turns each documented script code
    /// into the right `ChainError` boundary (and rejects an out-of-contract code).
    #[test]
    fn test_map_cas_result_maps_each_code() {
        let id = Uuid::new_v4();

        assert!(map_cas_result(1, id, 0).is_ok());

        match map_cas_result(-1, id, 0) {
            Err(ChainError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }

        match map_cas_result(-2, id, 4) {
            Err(ChainError::Conflict(msg)) => assert!(msg.contains("version 4")),
            other => panic!("expected Conflict, got {other:?}"),
        }

        match map_cas_result(42, id, 0) {
            Err(ChainError::Internal(_)) => {}
            other => panic!("expected Internal for an unexpected code, got {other:?}"),
        }
    }

    /// Issue #8: the in-memory Redis double mirrors the Lua CAS — a matching
    /// expected version commits, a stale one conflicts without overwriting, and a
    /// missing key is NotFound.
    #[tokio::test]
    async fn test_test_double_save_cas_behaves_like_lua() {
        let store = TestInRedisSessionStore::new(None, None);
        let mut session = create_test_session();
        let id = session.id;

        // Missing key → NotFound.
        match store.save_cas(session.clone(), 0).await {
            Err(ChainError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }

        store.create(session.clone()).await.unwrap();

        // Matching version → commit (stored revision advances to 1).
        session.current_step = 2;
        assert!(session.bump_version().is_ok());
        assert!(store.save_cas(session.clone(), 0).await.is_ok());
        assert_eq!(store.get(id).await.unwrap().version, 1);

        // Stale version → Conflict, store unchanged.
        let mut stale = session.clone();
        stale.current_step = 77;
        assert!(stale.bump_version().is_ok());
        match store.save_cas(stale, 0).await {
            Err(ChainError::Conflict(_)) => {}
            other => panic!("expected Conflict, got {other:?}"),
        }
        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.current_step, 2);
        assert_eq!(stored.version, 1);
    }

    /// Issue #8: the Lua script is the production atomicity boundary, so exercise it
    /// against a REAL Redis (the test doubles above only approximate it). Two
    /// `save_cas` calls that both read version 0 race through the single-threaded Lua
    /// engine: exactly one commits and one gets `Conflict`, the stored session ends at
    /// version 1, and a late third writer still expecting 0 also conflicts. Ignored by
    /// default because it needs a Redis on localhost:6379; run with `-- --ignored`.
    #[tokio::test]
    #[ignore = "requires live Redis on localhost:6379; run with -- --ignored"]
    async fn test_save_cas_is_atomic_against_live_redis() {
        // Build a client straight against a local, password-less Redis. If CI later
        // provides a redis service on localhost:6379 this runs there unchanged.
        let config = RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            username: None,
            password: None,
            database: 0,
            timeout: 5,
            connect_timeout: 5,
        };
        let client = Arc::new(
            RedisClient::new(config)
                .await
                .expect("connect to Redis on localhost:6379"),
        );

        // A unique prefix (with a random uuid) isolates this run's keys so parallel or
        // repeated `--ignored` runs never collide.
        let prefix = format!("test-cas:{}:", Uuid::new_v4());
        let store = InRedisSessionStore::new(client, Some(prefix), Some(60));

        let base = create_test_session();
        let id = base.id;
        store
            .create(base.clone())
            .await
            .expect("create session in Redis");

        // Two racing writers: both observed version 0 and bumped their clone to 1.
        let mut a = base.clone();
        a.current_step = 1;
        a.bump_version().expect("bump writer a");
        let mut b = base.clone();
        b.current_step = 2;
        b.bump_version().expect("bump writer b");

        let (ra, rb) = tokio::join!(store.save_cas(a, 0), store.save_cas(b, 0));
        let results = [ra, rb];
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        let conflict_count = results
            .iter()
            .filter(|r| matches!(r, Err(ChainError::Conflict(_))))
            .count();
        assert_eq!(ok_count, 1, "exactly one concurrent CAS must commit");
        assert_eq!(
            conflict_count, 1,
            "exactly one concurrent CAS must conflict"
        );

        // The single winner advanced the persisted revision to exactly 1.
        let stored = store.get(id).await.expect("load stored session");
        assert_eq!(stored.version, 1, "one winning advance leaves version at 1");

        // A late writer that still believes it is at version 0 must also conflict,
        // because the companion version key is now "1".
        let mut stale = base.clone();
        stale.current_step = 3;
        stale.bump_version().expect("bump stale writer");
        match store.save_cas(stale, 0).await {
            Err(ChainError::Conflict(_)) => {}
            other => panic!("expected Conflict for a stale writer, got {other:?}"),
        }

        // Clean up: delete removes both the document and its companion version key.
        assert!(store.delete(id).await.expect("cleanup session"));
    }

    // TestErrorInRedisSessionStore - to test error cases
    struct TestErrorInRedisSessionStore {
        // Store that always generates errors
    }

    #[async_trait]
    impl SessionStore for TestErrorInRedisSessionStore {
        async fn get(&self, id: Uuid) -> Result<Session, ChainError> {
            Err(ChainError::Internal(format!(
                "Simulated error for session {}",
                id
            )))
        }

        async fn create(&self, session: Session) -> Result<(), ChainError> {
            Err(ChainError::Internal(format!(
                "Simulated error creating session {}",
                session.id
            )))
        }

        async fn save(&self, session: Session) -> Result<(), ChainError> {
            Err(ChainError::Internal(format!(
                "Simulated error saving session {}",
                session.id
            )))
        }

        async fn save_cas(
            &self,
            session: Session,
            _expected_version: u64,
        ) -> Result<(), ChainError> {
            Err(ChainError::Internal(format!(
                "Simulated error CAS-saving session {}",
                session.id
            )))
        }

        async fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
            Err(ChainError::Internal(format!(
                "Simulated error deleting session {}",
                id
            )))
        }

        async fn cleanup(&self) -> Result<usize, ChainError> {
            Err(ChainError::Internal(
                "Simulated error during cleanup".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn test_error_handling() {
        let error_store = TestErrorInRedisSessionStore {};

        let session = create_test_session();
        let session_id = session.id;

        // Test that errors are properly propagated
        let create_result = error_store.create(session.clone()).await;
        assert!(create_result.is_err());

        let save_result = error_store.save(session.clone()).await;
        assert!(save_result.is_err());

        let save_cas_result = error_store.save_cas(session, 0).await;
        assert!(save_cas_result.is_err());

        let get_result = error_store.get(session_id).await;
        assert!(get_result.is_err());

        let delete_result = error_store.delete(session_id).await;
        assert!(delete_result.is_err());

        let cleanup_result = error_store.cleanup().await;
        assert!(cleanup_result.is_err());
    }
}
