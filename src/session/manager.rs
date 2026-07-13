use crate::domain::Simulator;
use crate::session::SessionStore;
use crate::session::model::{Session, SessionState, SimulationParameters};
use crate::session::state_handler::StateProgressionHandler;
use crate::utils::error::ChainError;
use optionstratlib::chains::OptionChain;
use std::string::ToString;
use std::sync::Arc;
use uuid::Uuid;

/// Deterministic UUID namespace kept for the `Default for Session` impl in
/// `src/session/model.rs`. New session ids are random (`Uuid::new_v4`) so a
/// restarted manager or a second replica never reproduces an id sequence.
pub(crate) const DEFAULT_NAMESPACE: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

/// Manages the lifecycle of simulation sessions
pub struct SessionManager {
    store: Arc<dyn SessionStore>,
    state_handler: StateProgressionHandler,
    simulator: Simulator,
}

impl SessionManager {
    /// Constructs a new instance of the struct with the provided session store.
    ///
    /// # Arguments
    ///
    /// * `store` - An `Arc` containing a trait object implementing the `SessionStore` trait. This
    ///   is used to manage session-related data and facilitate persistent or in-memory storage.
    ///
    /// # Returns
    ///
    /// A new instance of the struct, initialized with:
    /// * `store` - The given session store.
    /// * `state_handler` - A `StateProgressionHandler` instance created via its `new` function,
    ///   responsible for handling state progression.
    /// * `simulator` - A `Simulator` instance created via its `new` function, which simulates
    ///   specific processes or operations as per the required functionality.
    ///
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            state_handler: StateProgressionHandler::new(),
            simulator: Simulator::new(),
        }
    }

    /// Creates a new simulation session using the provided parameters and stores it in the persistent storage.
    ///
    /// # Arguments
    ///
    /// * `params` - A `SimulationParameters` instance that contains the configuration settings for the simulation session.
    ///
    /// # Returns
    ///
    /// Returns `Ok(Session)` containing the newly created simulation session if successful, or a `ChainError` if there is
    /// an issue with saving the session to persistent storage.
    ///
    /// # Errors
    ///
    /// This function returns an error in the following cases:
    /// - When there is an error in creating or initializing the session.
    /// - When the session fails to be stored in the backend storage.
    ///
    pub fn create_session(&self, params: SimulationParameters) -> Result<Session, ChainError> {
        // Random session ids (see `Session::with_random_id`) plus a non-overwriting
        // `create` guarantee a fresh session never clobbers a live one after a restart
        // or across replicas.
        let session = Session::with_random_id(params);
        self.store.create(session.clone())?;
        Ok(session)
    }

    /// Retrieves a session corresponding to the provided UUID.
    ///
    /// # Arguments
    ///
    /// * `id` - A `Uuid` representing the unique identifier of the session to be retrieved.
    ///
    /// # Returns
    ///
    /// * `Ok(Session)` - If the session corresponding to the provided `UUID` is found.
    /// * `Err(ChainError)` - If there is an error retrieving the session, such as the session not being found or an issue with the storage layer.
    ///
    /// # Errors
    ///
    /// This function will return a `ChainError` if:
    /// - The session corresponding to the provided ID does not exist in the underlying store.
    /// - There is an error in accessing or querying the storage mechanism.
    pub fn get_session(&self, id: Uuid) -> Result<Session, ChainError> {
        self.store.get(id)
    }

    /// Advances the session by one step, serving the snapshot at the current cursor.
    ///
    /// The step cursor (`session.current_step`) is the 0-based index of the NEXT
    /// snapshot to serve. This method follows *serve-then-advance* semantics so a
    /// session with `steps = N` serves EXACTLY indices `0..N-1` over `N` advances:
    ///
    /// 1. Fetch the session for `id` from the store.
    /// 2. Guard: if the session is already `Completed` (or the cursor has reached
    ///    `total_steps`), return the terminal [`ChainError::SimulatorError`] (HTTP 410)
    ///    WITHOUT touching the store — there is no further snapshot to serve.
    /// 3. Serve the snapshot at `session.current_step` via the simulator. The
    ///    simulator sees the PRE-advance session state, so a `Reinitialized` session
    ///    rebuilds its walk correctly before serving.
    /// 4. Advance the cursor via the `state_handler`; the advance that serves the last
    ///    snapshot transitions the session to `Completed`.
    /// 5. Persist the advanced session. The save ALWAYS happens after a successfully
    ///    served snapshot, INCLUDING the advance that transitions to `Completed`.
    ///
    /// The returned `Session` reflects the post-advance state; the returned
    /// `OptionChain` is the snapshot at the pre-advance cursor.
    ///
    /// # Arguments
    ///
    /// * `id` - A `Uuid` representing the unique identifier of the session.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing:
    /// - A tuple `(Session, OptionChain)` if the operation succeeds:
    ///     - `Session`: The session after advancing its cursor.
    ///     - `OptionChain`: The snapshot served at the pre-advance cursor.
    /// - A `ChainError` if there is an error during any step of the process.
    ///
    /// # Errors
    ///
    /// This function may return the following errors encapsulated in `ChainError`:
    /// - [`ChainError::NotFound`] if the session cannot be retrieved from the store.
    /// - [`ChainError::SimulatorError`] (mapped to HTTP 410) when the session has
    ///   already served all of its snapshots; the store is left untouched.
    /// - Any error surfaced by the simulator while building the served snapshot.
    /// - Any error surfaced while advancing the state or saving the session back to
    ///   the store.
    ///
    pub async fn get_next_step(&self, id: Uuid) -> Result<(Session, OptionChain), ChainError> {
        let mut session = self.store.get(id)?;

        // Completed guard: a session that has served all of its snapshots has nothing
        // left to serve. Mirror the exhausted-advance path (410 Gone) and leave the
        // store untouched so a repeated call keeps returning the terminal error.
        if session.state == SessionState::Completed || session.current_step >= session.total_steps {
            return Err(ChainError::SimulatorError(
                "session completed; no further steps".to_string(),
            ));
        }

        // Serve the snapshot at the current cursor. The simulator sees the pre-advance
        // state, so a `Reinitialized` session rebuilds its walk before serving.
        let chain = self
            .simulator
            .simulate_next_step(&session)
            .await
            .map_err(|e| ChainError::SimulatorError(e.to_string()))?;

        // Advance the cursor; the advance that serves the last snapshot marks Completed.
        self.state_handler.advance_state(&mut session)?;

        // Always persist after a successfully served snapshot, including the advance
        // that transitions the session to Completed.
        self.store.save(session.clone())?;

        Ok((session, chain))
    }

    /// Returns the session's CURRENT chain snapshot without advancing or persisting it.
    ///
    /// This is the read-only, repeatable counterpart to [`SessionManager::get_next_step`]:
    /// it serves the snapshot at `session.current_step` and leaves the session state, the
    /// step counter, and the store untouched. Calling it repeatedly yields the same
    /// snapshot until an explicit advance ([`SessionManager::get_next_step`]) moves the
    /// cursor.
    ///
    /// The domain walk cache may be populated as a side effect of building the snapshot,
    /// but no session state ever changes and nothing is written back to the store.
    ///
    /// # Arguments
    ///
    /// * `id` - A `Uuid` identifying the session to peek.
    ///
    /// # Returns
    ///
    /// A tuple `(Session, OptionChain)` with the unchanged session and its current
    /// snapshot.
    ///
    /// # Errors
    ///
    /// This function may return the following errors encapsulated in `ChainError`:
    /// - [`ChainError::NotFound`] if the session does not exist in the store.
    /// - [`ChainError::SimulatorError`] if the session has already completed all steps
    ///   (there is no current step to serve); this maps to HTTP 410 like the
    ///   exhausted-advance path.
    /// - Any error surfaced by the simulator while building the current snapshot.
    pub async fn peek_current_step(&self, id: Uuid) -> Result<(Session, OptionChain), ChainError> {
        let session = self.store.get(id)?;

        // A completed session has no current step to serve; mirror the exhausted-advance
        // path (410 Gone) rather than returning stale data.
        if session.state == SessionState::Completed {
            return Err(ChainError::SimulatorError(
                "session completed; no current step".to_string(),
            ));
        }
        // `Error` is the other terminal state (`Session::is_active`); a peek
        // must reject it like the advance path does instead of serving a
        // snapshot from a session that can no longer progress.
        if session.state == SessionState::Error {
            return Err(ChainError::InvalidState(
                "Session is in error state".to_string(),
            ));
        }

        // Read-only: build/read the walk at the current step. This never advances the
        // counter and never persists the session.
        let chain = self
            .simulator
            .simulate_next_step(&session)
            .await
            .map_err(|e| ChainError::SimulatorError(e.to_string()))?;

        Ok((session, chain))
    }

    /// Updates an existing simulation session with new parameters.
    ///
    /// # Parameters
    ///
    /// - `id`: The unique `Uuid` identifier of the session to be updated.
    /// - `params`: The new `SimulationParameters` to apply to the session.
    ///
    /// # Returns
    ///
    /// - `Ok(Session)`: Returns the updated `Session` on success.
    /// - `Err(ChainError)`: Returns an error if the session cannot be retrieved
    ///   or saved successfully.
    ///
    /// # Errors
    ///
    /// This function will return a `ChainError` if:
    /// - The session identified by `id` does not exist in the store.
    /// - The updated session cannot be saved back to the store.
    ///
    pub fn update_session(
        &self,
        id: Uuid,
        params: SimulationParameters,
    ) -> Result<Session, ChainError> {
        let mut session = self.store.get(id)?;

        // Update parameters
        session.modify_parameters(params);

        // Reset progression
        self.state_handler.reset_progression(&mut session)?;

        // Save updated session
        self.store.save(session.clone())?;

        Ok(session)
    }

    /// Reinitializes an existing simulation session with new parameters and resets its progression.
    ///
    /// This method retrieves a session by its UUID, updates its parameters and total steps,
    /// resets its progression state, and saves the updated session in the session store.
    ///
    /// # Parameters
    ///
    /// - `id`: The unique identifier (`Uuid`) of the session to be reinitialized.
    /// - `params`: The new `SimulationParameters` to apply to the session.
    /// - `total_steps`: The total number of steps for the simulation.
    ///
    /// # Returns
    ///
    /// - `Ok(Session)`: The updated session object, if the operation is successful.
    /// - `Err(ChainError)`: An error if retrieving, updating, or saving the session fails.
    ///
    /// # Errors
    ///
    /// This function returns an error in the following scenarios:
    /// - If the session with the provided `id` cannot be found in the store.
    /// - If there is a failure in resetting the session's progression.
    /// - If there is an issue saving the updated session to the store.
    ///
    pub fn reinitialize_session(
        &self,
        id: Uuid,
        params: SimulationParameters,
    ) -> Result<Session, ChainError> {
        let mut session = self.store.get(id)?;

        // Reinitialize session
        session.reinitialize(params);

        // Reset progression
        self.state_handler.reset_progression(&mut session)?;

        // Save updated session
        self.store.save(session.clone())?;

        Ok(session)
    }

    /// Deletes a session from the store.
    ///
    /// # Arguments
    ///
    /// * `id` - A `Uuid` representing the unique identifier of the session to be deleted.
    ///
    /// # Returns
    ///
    /// * `Result<bool, ChainError>` -
    ///   - `Ok(true)` if the session was successfully deleted.
    ///   - `Ok(false)` if the session was not found in the store.
    ///   - `Err(ChainError)` if an error occurred while attempting to delete the session.
    ///
    /// # Errors
    ///
    /// This function will return a `ChainError` if there is an issue interacting with the store.
    ///
    pub fn delete_session(&self, id: Uuid) -> Result<bool, ChainError> {
        self.store.delete(id)
    }

    /// Cleans up outdated or inactive sessions in the underlying storage.
    ///
    /// This function delegates the cleanup operation to the storage backend
    /// by calling its `cleanup` method. It removes any stale or unused session
    /// data, helping to free up resources and maintain the integrity of the stored information.
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - The number of sessions that have been cleaned up.
    /// * `Err(ChainError)` - An error occurred during the cleanup operation, such as
    ///   a failure in the storage backend.
    ///
    /// # Errors
    ///
    /// This function returns a `ChainError` if the cleanup operation fails due to
    /// issues with the storage system or other related error conditions.
    ///
    /// # Notes
    ///
    /// The specific behavior of this function depends on how the `cleanup` method
    /// is implemented in the underlying `store`. Ensure that the `cleanup` logic in the
    /// storage backend is properly equipped to remove outdated or invalid sessions.
    pub fn cleanup_sessions(&self) -> Result<usize, ChainError> {
        self.store.cleanup()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{InMemorySessionStore, SimulationMethod};
    use optionstratlib::utils::TimeFrame;
    use positive::{Positive, pos_or_panic};
    use rust_decimal::Decimal;

    fn test_parameters() -> SimulationParameters {
        SimulationParameters {
            symbol: "AAPL".to_string(),
            steps: 10,
            initial_price: pos_or_panic!(100.0),
            days_to_expiration: pos_or_panic!(30.0),
            volatility: pos_or_panic!(0.2),
            risk_free_rate: Decimal::ZERO,
            dividend_yield: Positive::ZERO,
            method: SimulationMethod::Brownian {
                dt: pos_or_panic!(1.0 / 252.0),
                drift: Decimal::ZERO,
                volatility: pos_or_panic!(0.2),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(15),
            strike_interval: Some(pos_or_panic!(1.0)),
            skew_slope: None,
            smile_curve: None,
            spread: Some(pos_or_panic!(0.02)),
            seed: None,
        }
    }

    /// Deterministic parameters for tape/cursor tests: a fixed seed makes the served
    /// snapshots reproducible so index-alignment assertions are stable.
    fn seeded_parameters(steps: usize, seed: u64) -> SimulationParameters {
        SimulationParameters {
            steps,
            seed: Some(seed),
            ..test_parameters()
        }
    }

    /// Issue #5: a session with `steps = N` serves EXACTLY indices `0..N-1` over `N`
    /// advances; the N-th advance persists `Completed`; the (N+1)-th is terminal and
    /// leaves the store untouched.
    #[tokio::test]
    async fn test_n_step_session_serves_exactly_n_snapshots() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());
        let session = manager
            .create_session(seeded_parameters(3, 20260713))
            .expect("failed to create session");
        let id = session.id;

        let mut prices = Vec::new();
        for _ in 0..3 {
            let (_s, chain) = manager.get_next_step(id).await.expect("advance failed");
            prices.push(chain.underlying_price);
        }
        // Exactly three served snapshots (indices 0, 1, 2).
        assert_eq!(prices.len(), 3);

        // The 4th advance is terminal.
        match manager.get_next_step(id).await {
            Err(ChainError::SimulatorError(_)) => {}
            other => panic!("expected terminal SimulatorError, got {other:?}"),
        }

        // Completion is persisted: the cursor reached total_steps and the state is Completed.
        let stored = store.get(id).expect("session missing from store");
        assert_eq!(stored.state, SessionState::Completed);
        assert_eq!(stored.current_step, 3);
    }

    /// Issue #5: the FIRST advance serves index 0 (previously index 0 was skipped).
    /// Peek shows the snapshot the next advance will serve, and after the advance the
    /// cursor moves on to the next index.
    #[tokio::test]
    async fn test_first_advance_serves_index_zero() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());
        let session = manager
            .create_session(seeded_parameters(4, 777))
            .expect("failed to create session");
        let id = session.id;

        // Peek shows P0 (the snapshot the next advance will serve).
        let (_p, peek_chain) = manager.peek_current_step(id).await.expect("peek failed");
        let p0 = peek_chain.underlying_price;

        // The first advance serves exactly P0 (index 0), not index 1.
        let (_a, advance_chain) = manager.get_next_step(id).await.expect("advance failed");
        assert_eq!(advance_chain.underlying_price, p0);

        // Peek now reflects the moved cursor (index 1)...
        let (peek_after, peek_next) = manager
            .peek_current_step(id)
            .await
            .expect("peek after advance failed");
        assert_eq!(peek_after.current_step, 1);

        // ...and that P1 is exactly what the next advance serves, and it is a new index.
        let (_a2, advance2) = manager
            .get_next_step(id)
            .await
            .expect("second advance failed");
        assert_eq!(advance2.underlying_price, peek_next.underlying_price);
        assert_ne!(advance2.underlying_price, p0);
    }

    /// Issue #5: the advance that serves the last snapshot persists `Completed` exactly
    /// once; a further advance is terminal and must not mutate the stored session.
    #[tokio::test]
    async fn test_completion_is_persisted_once() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());
        let n = 3;
        let session = manager
            .create_session(seeded_parameters(n, 55))
            .expect("failed to create session");
        let id = session.id;

        for _ in 0..n {
            manager.get_next_step(id).await.expect("advance failed");
        }

        let after_completion = store.get(id).expect("session missing from store");
        assert_eq!(after_completion.state, SessionState::Completed);
        assert_eq!(after_completion.current_step, n);
        let updated_at = after_completion.updated_at;

        // A further advance is terminal and leaves the stored session unchanged.
        match manager.get_next_step(id).await {
            Err(ChainError::SimulatorError(_)) => {}
            other => panic!("expected terminal SimulatorError, got {other:?}"),
        }
        let unchanged = store.get(id).expect("session missing from store");
        assert_eq!(unchanged.state, SessionState::Completed);
        assert_eq!(unchanged.current_step, n);
        assert_eq!(unchanged.updated_at, updated_at);
    }

    /// Issue #5: peek and the next advance serve the SAME index across the whole tape,
    /// and both become terminal at the same point.
    #[tokio::test]
    async fn test_advance_and_peek_serve_same_index() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());
        let n = 4;
        let session = manager
            .create_session(seeded_parameters(n, 999))
            .expect("failed to create session");
        let id = session.id;

        for _ in 0..n {
            // Peek shows the snapshot the next advance will serve...
            let (_p, peeked) = manager.peek_current_step(id).await.expect("peek failed");
            // ...and the advance serves exactly that snapshot.
            let (_a, advanced) = manager.get_next_step(id).await.expect("advance failed");
            assert_eq!(advanced.underlying_price, peeked.underlying_price);
        }

        // Both peek and advance are exhausted at the same point.
        assert!(matches!(
            manager.peek_current_step(id).await,
            Err(ChainError::SimulatorError(_))
        ));
        assert!(matches!(
            manager.get_next_step(id).await,
            Err(ChainError::SimulatorError(_))
        ));
    }

    /// Reproducibility at the manager level: two sessions with identical parameters and
    /// the same seed, advanced through every step via `get_next_step`, yield identical
    /// price sequences.
    #[tokio::test]
    async fn test_manager_same_seed_produces_identical_tape() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store);
        let n = 5;
        let seed = 20260713;

        let session_a = manager
            .create_session(seeded_parameters(n, seed))
            .expect("failed to create session a");
        let session_b = manager
            .create_session(seeded_parameters(n, seed))
            .expect("failed to create session b");

        let mut tape_a = Vec::with_capacity(n);
        let mut tape_b = Vec::with_capacity(n);
        for _ in 0..n {
            let (_s, chain) = manager
                .get_next_step(session_a.id)
                .await
                .expect("advance a failed");
            tape_a.push(chain.underlying_price);
        }
        for _ in 0..n {
            let (_s, chain) = manager
                .get_next_step(session_b.id)
                .await
                .expect("advance b failed");
            tape_b.push(chain.underlying_price);
        }

        assert_eq!(tape_a, tape_b);
    }

    /// Regression for issue #7: two freshly built managers (each with its own store,
    /// as after a restart or on a second replica) must not emit the same first id.
    #[test]
    fn test_fresh_managers_produce_different_first_ids() {
        let manager_a = SessionManager::new(Arc::new(InMemorySessionStore::new()));
        let manager_b = SessionManager::new(Arc::new(InMemorySessionStore::new()));

        let session_a = manager_a
            .create_session(test_parameters())
            .expect("first manager failed to create session");
        let session_b = manager_b
            .create_session(test_parameters())
            .expect("second manager failed to create session");

        assert_ne!(
            session_a.id, session_b.id,
            "fresh managers must not reproduce the same id sequence"
        );
    }

    /// Successive creates on the same manager also yield distinct ids.
    #[test]
    fn test_sequential_creates_have_unique_ids() {
        let manager = SessionManager::new(Arc::new(InMemorySessionStore::new()));

        let first = manager.create_session(test_parameters()).unwrap();
        let second = manager.create_session(test_parameters()).unwrap();

        assert_ne!(first.id, second.id);
    }

    /// Issue #21: GET is a peek. `peek_current_step` must be repeatable and must not
    /// mutate the stored session (no cursor advance, no state change, no save).
    #[tokio::test]
    async fn test_peek_current_step_is_repeatable_and_read_only() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());
        let session = manager
            .create_session(test_parameters())
            .expect("failed to create session");
        let id = session.id;

        let (first, _) = manager
            .peek_current_step(id)
            .await
            .expect("first peek failed");
        let (second, _) = manager
            .peek_current_step(id)
            .await
            .expect("second peek failed");

        // Peek is repeatable: the served cursor does not move between calls.
        assert_eq!(first.current_step, second.current_step);
        assert_eq!(first.current_step, 0);
        assert_eq!(first.state, SessionState::Initialized);

        // Peek is read-only: the stored session is untouched.
        let stored = store.get(id).expect("session missing from store");
        assert_eq!(stored.current_step, 0);
        assert_eq!(stored.state, SessionState::Initialized);
    }

    /// After an explicit advance, peek reflects the new cursor and still does not move it.
    #[tokio::test]
    async fn test_advance_then_peek_reflects_new_cursor() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());
        let session = manager
            .create_session(test_parameters())
            .expect("failed to create session");
        let id = session.id;

        // Advance moves the cursor and persists it.
        let (advanced, _) = manager.get_next_step(id).await.expect("advance failed");
        assert_eq!(advanced.current_step, 1);

        // Peek now reflects the advanced cursor without moving it further.
        let (peeked, _) = manager
            .peek_current_step(id)
            .await
            .expect("peek after advance failed");
        assert_eq!(peeked.current_step, advanced.current_step);

        let stored = store.get(id).expect("session missing from store");
        assert_eq!(stored.current_step, 1);
        assert_eq!(stored.state, SessionState::InProgress);
    }

    /// Peek on a Completed session has no current step to serve and returns a
    /// `SimulatorError` (mapped to HTTP 410), mirroring the exhausted-advance path.
    #[tokio::test]
    async fn test_peek_on_completed_session_returns_simulator_error() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());
        let session = manager
            .create_session(test_parameters())
            .expect("failed to create session");
        let id = session.id;

        // Force the session into the Completed state directly in the store.
        let mut completed = store.get(id).expect("session missing from store");
        completed.current_step = completed.total_steps;
        completed.state = SessionState::Completed;
        store
            .save(completed)
            .expect("failed to persist completed session");

        match manager.peek_current_step(id).await {
            Err(ChainError::SimulatorError(msg)) => {
                assert_eq!(msg, "session completed; no current step");
            }
            other => panic!("expected SimulatorError for completed peek, got {other:?}"),
        }
    }

    /// `Error` is terminal too: a peek must reject it instead of serving a
    /// snapshot from a session that can no longer progress.
    #[tokio::test]
    async fn test_peek_on_error_session_returns_invalid_state() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());
        let session = manager
            .create_session(test_parameters())
            .expect("failed to create session");
        let id = session.id;

        let mut errored = store.get(id).expect("session missing from store");
        errored.state = SessionState::Error;
        store
            .save(errored)
            .expect("failed to persist errored session");

        match manager.peek_current_step(id).await {
            Err(ChainError::InvalidState(msg)) => {
                assert_eq!(msg, "Session is in error state");
            }
            other => panic!("expected InvalidState for errored peek, got {other:?}"),
        }
    }
}
