use uuid::Uuid;

use crate::session::model::Session;
use crate::utils::error::ChainError;

/// A trait that defines the behavior of a session store backend.
/// This trait is intended to manage sessions by providing methods for retrieving,
/// saving, deleting, and cleaning up session data.
///
/// Implementors of the `SessionStore` trait must provide thread-safe and
/// shareable implementations (i.e., satisfy `Send` and `Sync`).
///
/// # Associated Types
/// - `Uuid`: A universally unique identifier used to identify sessions.
/// - `Session`: The session object containing session data.
/// - `ChainError`: Represents errors that may occur during operations.
///
/// # Required Methods
///
/// ## `get`
/// Retrieves a session associated with the given `Uuid`.
///
/// - **Parameters**:
///   - `id`: A `Uuid` identifying the session.
/// - **Returns**:
///   - `Ok(Session)`: The session object if retrieval is successful.
///   - `Err(ChainError)`: An error if the session cannot be retrieved.
///
/// ## `save`
/// Persists the provided session object.
///
/// - **Parameters**:
///   - `session`: The `Session` object to be saved.
/// - **Returns**:
///   - `Ok(())`: Indicates successful saving of the session.
///   - `Err(ChainError)`: An error if the session cannot be saved.
///
/// ## `delete`
/// Deletes a session identified by the given `Uuid`.
///
/// - **Parameters**:
///   - `id`: A `Uuid` identifying the session to be deleted.
/// - **Returns**:
///   - `Ok(true)`: Indicates the session was successfully deleted.
///   - `Ok(false)`: Indicates the session did not exist.
///   - `Err(ChainError)`: An error if the deletion fails.
///
/// ## `cleanup`
/// Cleans up expired or stale sessions from the session store.
///
/// - **Returns**:
///   - `Ok(usize)`: The number of sessions that were cleaned up.
///   - `Err(ChainError)`: An error if cleanup fails.
///
pub trait SessionStore: Send + Sync {
    /// Retrieves a `Session` by its unique identifier.
    ///
    /// # Parameters
    /// - `id`: A `Uuid` that uniquely identifies the `Session` to retrieve.
    ///
    /// # Returns
    /// - `Ok(Session)`: The session corresponding to the provided `id`.
    /// - `Err(ChainError)`: If the session could not be retrieved due to an error.
    ///
    /// # Errors
    /// This function returns a `ChainError` if:
    /// - The session with the provided `id` does not exist.
    /// - There is an issue with the underlying storage or retrieval process.
    ///
    fn get(&self, id: Uuid) -> Result<Session, ChainError>;

    /// Saves the provided session into persistent storage or memory.
    ///
    /// # Parameters
    /// - `session`: A `Session` object that contains the details to be saved.
    ///
    /// # Returns
    /// - `Ok(())`: If the session is successfully saved.
    /// - `Err(ChainError)`: If an error occurs during the save operation, wrapped in a `ChainError`.
    ///
    /// # Errors
    /// This function may return a `ChainError` in scenarios such as:
    /// - Issues with accessing the storage system.
    /// - Serialization or persistence failures.
    ///
    fn save(&self, session: Session) -> Result<(), ChainError>;

    /// Deletes an entity identified by the given `id`.
    ///
    /// # Parameters
    /// - `id`: A `Uuid` representing the identifier of the entity to be deleted.
    ///
    /// # Returns
    /// - `Ok(true)`: If the deletion was successful.
    /// - `Ok(false)`: If the deletion was unsuccessful, but no error occurred (e.g., entity not found).
    /// - `Err(ChainError)`: If an error occurred during the deletion process.
    ///
    /// # Errors
    /// This function returns a `ChainError` if there is an issue with the deletion process,
    /// such as database communication errors or invalid input.
    ///
    fn delete(&self, id: Uuid) -> Result<bool, ChainError>;

    /// Cleans up stale or unnecessary data within the chain and performs housekeeping tasks.
    ///
    /// This method is responsible for managing and removing data that is no longer
    /// needed to ensure the efficient functioning of the chain. It allows the chain
    /// to remain performant and reduces unnecessary memory or storage usage.
    ///
    /// # Returns
    /// * `Ok(usize)` - The number of items successfully cleaned up.
    /// * `Err(ChainError)` - If an error occurs during the cleanup process.
    ///
    /// # Errors
    /// This function will return a `ChainError` in case of failures, such as issues
    /// accessing resources, file system problems, or other internal errors during
    /// cleanup.
    fn cleanup(&self) -> Result<usize, ChainError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::model::Session;
    use crate::utils::error::ChainError;
    use mockall::mock;
    use optionstratlib::simulation::WalkType;
    use optionstratlib::utils::TimeFrame;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;
    use uuid::Uuid;

    // Create a mock implementation of SessionStore for testing
    mock! {
        pub SessionStore {}
        impl SessionStore for SessionStore {
            fn get(&self, id: Uuid) -> Result<Session, ChainError>;
            fn save(&self, session: Session) -> Result<(), ChainError>;
            fn delete(&self, id: Uuid) -> Result<bool, ChainError>;
            fn cleanup(&self) -> Result<usize, ChainError>;
        }
    }

    // A basic implementation of SessionStore for testing
    struct TestSessionStore {
        sessions: Arc<Mutex<HashMap<Uuid, Session>>>,
    }

    impl TestSessionStore {
        fn new() -> Self {
            TestSessionStore {
                sessions: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    impl SessionStore for TestSessionStore {
        fn get(&self, id: Uuid) -> Result<Session, ChainError> {
            let sessions = self.sessions.lock().unwrap();
            match sessions.get(&id) {
                Some(session) => Ok(session.clone()),
                None => Err(ChainError::NotFound(format!(
                    "Session with id {} not found",
                    id
                ))),
            }
        }

        fn save(&self, session: Session) -> Result<(), ChainError> {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.insert(session.id, session);
            Ok(())
        }

        fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
            let mut sessions = self.sessions.lock().unwrap();
            Ok(sessions.remove(&id).is_some())
        }

        fn cleanup(&self) -> Result<usize, ChainError> {
            let now = SystemTime::now();
            let mut sessions = self.sessions.lock().unwrap();

            // For testing, let's just remove sessions that are older than 1 hour
            let old_count = sessions.len();
            sessions.retain(|_, session| {
                match now.duration_since(session.updated_at) {
                    Ok(duration) => duration.as_secs() < 3600, // Keep sessions less than 1 hour old
                    Err(_) => true, // Keep sessions with future timestamps
                }
            });

            Ok(old_count - sessions.len())
        }
    }

    // Helper function to create a test session
    fn create_test_session(id: Option<Uuid>) -> Session {
        use crate::session::model::{SessionState, SimulationParameters};

        Session {
            id: id.unwrap_or_else(Uuid::new_v4),
            created_at: SystemTime::now(),
            updated_at: SystemTime::now(),
            parameters: SimulationParameters {
                symbol: "".to_string(),
                steps: 0,
                initial_price: Default::default(),
                days_to_expiration: Default::default(),
                volatility: Default::default(),
                risk_free_rate: Default::default(),
                dividend_yield: Default::default(),
                method: WalkType::Brownian {
                    dt: Default::default(),
                    drift: Default::default(),
                    volatility: Default::default(),
                },
                time_frame: TimeFrame::Microsecond,
                chain_size: None,
                strike_interval: None,
                skew_factor: None,
                spread: None,
            },
            current_step: 0,
            total_steps: 100,
            state: SessionState::Initialized,
        }
    }

    #[test]
    fn test_get_existing_session() {
        let store = TestSessionStore::new();
        let session = create_test_session(None);
        let session_id = session.id;

        // Save the session first
        store.save(session.clone()).unwrap();

        // Then try to get it
        let result = store.get(session_id);
        assert!(result.is_ok());

        let retrieved_session = result.unwrap();
        assert_eq!(retrieved_session.id, session_id);
        assert_eq!(retrieved_session.current_step, session.current_step);
        assert_eq!(retrieved_session.total_steps, session.total_steps);
    }

    #[test]
    fn test_get_non_existing_session() {
        let store = TestSessionStore::new();
        let non_existent_id = Uuid::new_v4();

        let result = store.get(non_existent_id);
        assert!(result.is_err());

        match result {
            Err(ChainError::NotFound(_)) => {} // This is the expected error
            _ => panic!("Expected NotFound error"),
        }
    }

    #[test]
    fn test_save_new_session() {
        let store = TestSessionStore::new();
        let session = create_test_session(None);
        let session_id = session.id;

        let save_result = store.save(session);
        assert!(save_result.is_ok());

        // Verify it was saved by retrieving it
        let get_result = store.get(session_id);
        assert!(get_result.is_ok());
    }

    #[test]
    fn test_save_existing_session() {
        let store = TestSessionStore::new();
        let mut session = create_test_session(None);
        let session_id = session.id;

        // Save the session first
        store.save(session.clone()).unwrap();

        // Update the session and save again
        session.current_step = 50;
        let save_result = store.save(session.clone());
        assert!(save_result.is_ok());

        // Verify the update by retrieving it
        let get_result = store.get(session_id).unwrap();
        assert_eq!(get_result.current_step, 50);
    }

    #[test]
    fn test_delete_existing_session() {
        let store = TestSessionStore::new();
        let session = create_test_session(None);
        let session_id = session.id;

        // Save the session first
        store.save(session).unwrap();

        // Then delete it
        let delete_result = store.delete(session_id);
        assert!(delete_result.is_ok());
        assert!(delete_result.unwrap()); // Should return true for successful deletion

        // Verify it was deleted
        let get_result = store.get(session_id);
        assert!(get_result.is_err());
    }

    #[test]
    fn test_delete_non_existing_session() {
        let store = TestSessionStore::new();
        let non_existent_id = Uuid::new_v4();

        let delete_result = store.delete(non_existent_id);
        assert!(delete_result.is_ok());
        assert!(!delete_result.unwrap()); // Should return false for non-existent session
    }

    #[test]
    fn test_cleanup_with_no_expired_sessions() {
        let store = TestSessionStore::new();

        // Add a few fresh sessions
        for _ in 0..5 {
            store.save(create_test_session(None)).unwrap();
        }

        let cleanup_result = store.cleanup();
        assert!(cleanup_result.is_ok());
        assert_eq!(cleanup_result.unwrap(), 0); // No sessions should be cleaned up
    }
}
