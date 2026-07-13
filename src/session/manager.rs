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
    pub async fn create_session(
        &self,
        params: SimulationParameters,
    ) -> Result<Session, ChainError> {
        // Random session ids (see `Session::with_random_id`) plus a non-overwriting
        // `create` guarantee a fresh session never clobbers a live one after a restart
        // or across replicas.
        let session = Session::with_random_id(params);
        self.store.create(session.clone()).await?;
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
    pub async fn get_session(&self, id: Uuid) -> Result<Session, ChainError> {
        self.store.get(id).await
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
        let mut session = self.store.get(id).await?;

        // Capture the revision we read; the compare-and-swap save below commits only
        // if the stored session is still at this version, so two concurrent advances
        // that both read the same snapshot cannot both persist (the loser gets a
        // Conflict) — no lost update, no duplicate step.
        let expected_version = session.version;

        // Completed guard: a session that has served all of its snapshots has nothing
        // left to serve. Mirror the exhausted-advance path (410 Gone) and leave the
        // store untouched so a repeated call keeps returning the terminal error.
        if session.state == SessionState::Completed || session.current_step >= session.total_steps {
            return Err(ChainError::SimulatorError(
                "session completed; no further steps".to_string(),
            ));
        }
        // `Error` is the other terminal state: reject it BEFORE simulating so a
        // dead session neither builds a walk nor serves a snapshot (matches the
        // peek path and `advance_state`'s own terminal handling).
        if session.state == SessionState::Error {
            return Err(ChainError::InvalidState(
                "Session is in error state".to_string(),
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

        // Bump the revision and persist via compare-and-swap. On a concurrent race the
        // save returns `ChainError::Conflict` (HTTP 409) and the client retries; there
        // is deliberately no silent retry loop here.
        session.bump_version()?;
        self.store
            .save_cas(session.clone(), expected_version)
            .await?;

        // The advance that serves the last snapshot marks the session `Completed`. Its
        // walk is finished — the completed guard above blocks any further simulate — so
        // evict it from the domain cache to reclaim memory (issue #9). Eviction is safe
        // for reproducibility: a re-simulate would rebuild the identical seeded walk.
        if session.state == SessionState::Completed {
            self.simulator.remove_session(&id).await;
        }

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
        let session = self.store.get(id).await?;

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

    /// Updates an existing simulation session with new parameters (PATCH).
    ///
    /// A parameter change restarts the session's tape: the new parameters may alter
    /// the walk (steps, volatility, seed, ...), so the cursor is reset to 0 and the
    /// session is marked `Reinitialized`. The next [`SessionManager::get_next_step`]
    /// then rebuilds the walk from the new parameters and serves index 0. This is
    /// implemented via [`Session::reinitialize`], which also syncs `total_steps` to
    /// `params.steps` so a PATCH that changes the step count stays coherent.
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
    pub async fn update_session(
        &self,
        id: Uuid,
        params: SimulationParameters,
    ) -> Result<Session, ChainError> {
        let mut session = self.store.get(id).await?;
        let expected_version = session.version;

        // A parameter change restarts the tape: reset the cursor, sync total_steps,
        // and mark the session Reinitialized so the next advance rebuilds the walk.
        session.reinitialize(params);

        // Persist via compare-and-swap so a PATCH cannot clobber a newer revision
        // written by a concurrent advance/update; a race yields `ChainError::Conflict`
        // (HTTP 409) for the client to retry.
        session.bump_version()?;
        self.store
            .save_cas(session.clone(), expected_version)
            .await?;

        Ok(session)
    }

    /// Reinitializes an existing simulation session with new parameters and resets its
    /// progression (PUT).
    ///
    /// This method retrieves a session by its UUID and applies
    /// [`Session::reinitialize`], which replaces the parameters, syncs `total_steps`
    /// to `params.steps`, resets `current_step` to 0, and marks the session
    /// `Reinitialized`. The next [`SessionManager::get_next_step`] observes the
    /// `Reinitialized` state, rebuilds the walk from the new parameters/seed, and
    /// serves index 0.
    ///
    /// # Parameters
    ///
    /// - `id`: The unique identifier (`Uuid`) of the session to be reinitialized.
    /// - `params`: The new `SimulationParameters` to apply to the session.
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
    /// - If there is an issue saving the updated session to the store.
    ///
    pub async fn reinitialize_session(
        &self,
        id: Uuid,
        params: SimulationParameters,
    ) -> Result<Session, ChainError> {
        let mut session = self.store.get(id).await?;
        let expected_version = session.version;

        // Reinitialize session: reset cursor, sync total_steps, mark Reinitialized.
        session.reinitialize(params);

        // Persist via compare-and-swap so a PUT cannot clobber a newer revision
        // written by a concurrent advance/update; a race yields `ChainError::Conflict`
        // (HTTP 409) for the client to retry.
        session.bump_version()?;
        self.store
            .save_cas(session.clone(), expected_version)
            .await?;

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
    pub async fn delete_session(&self, id: Uuid) -> Result<bool, ChainError> {
        let deleted = self.store.delete(id).await?;

        // Evict the cached walk regardless of whether the store held the session:
        // removing a non-cached id is a cheap no-op, and this keeps the domain cache
        // from outliving the session it served (issue #9).
        self.simulator.remove_session(&id).await;

        Ok(deleted)
    }

    /// Returns the number of random walks currently cached by the domain simulator.
    ///
    /// Read-only helper the API layer uses to publish the `simulation_cache_size`
    /// gauge after operations that grow or shrink the cache (advance, delete). It
    /// delegates to [`crate::domain`]'s simulator without touching session state or
    /// the store.
    pub async fn simulation_cache_len(&self) -> usize {
        self.simulator.cache_len().await
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
    pub async fn cleanup_sessions(&self) -> Result<usize, ChainError> {
        self.store.cleanup().await
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
            .await
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
        let stored = store.get(id).await.expect("session missing from store");
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
            .await
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
            .await
            .expect("failed to create session");
        let id = session.id;

        for _ in 0..n {
            manager.get_next_step(id).await.expect("advance failed");
        }

        let after_completion = store.get(id).await.expect("session missing from store");
        assert_eq!(after_completion.state, SessionState::Completed);
        assert_eq!(after_completion.current_step, n);
        let updated_at = after_completion.updated_at;

        // A further advance is terminal and leaves the stored session unchanged.
        match manager.get_next_step(id).await {
            Err(ChainError::SimulatorError(_)) => {}
            other => panic!("expected terminal SimulatorError, got {other:?}"),
        }
        let unchanged = store.get(id).await.expect("session missing from store");
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
            .await
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
            .await
            .expect("failed to create session a");
        let session_b = manager
            .create_session(seeded_parameters(n, seed))
            .await
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

    /// Issue #9 (DELETE path): deleting a session evicts its cached walk from the
    /// domain simulator, not just the session store.
    #[tokio::test]
    async fn test_delete_session_evicts_cached_walk() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store);
        let session = manager
            .create_session(seeded_parameters(5, 20260713))
            .await
            .expect("failed to create session");
        let id = session.id;

        // One advance populates the domain cache for this session.
        manager.get_next_step(id).await.expect("advance failed");
        assert_eq!(manager.simulation_cache_len().await, 1);

        // DELETE evicts the walk.
        assert!(manager.delete_session(id).await.expect("delete failed"));
        assert_eq!(manager.simulation_cache_len().await, 0);
    }

    /// Issue #9 (completion path): the advance that transitions a session to
    /// `Completed` evicts its cached walk (the walk is finished and can never be
    /// simulated again).
    #[tokio::test]
    async fn test_completion_evicts_cached_walk() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store);
        let session = manager
            .create_session(seeded_parameters(2, 4242))
            .await
            .expect("failed to create session");
        let id = session.id;

        // First advance populates the cache but does not complete the 2-step tape.
        manager
            .get_next_step(id)
            .await
            .expect("first advance failed");
        assert_eq!(manager.simulation_cache_len().await, 1);

        // Second advance serves the last snapshot, marks Completed, and evicts.
        let (completed, _chain) = manager
            .get_next_step(id)
            .await
            .expect("second advance failed");
        assert_eq!(completed.state, SessionState::Completed);
        assert_eq!(manager.simulation_cache_len().await, 0);
    }

    /// Regression for issue #7: two freshly built managers (each with its own store,
    /// as after a restart or on a second replica) must not emit the same first id.
    #[tokio::test]
    async fn test_fresh_managers_produce_different_first_ids() {
        let manager_a = SessionManager::new(Arc::new(InMemorySessionStore::new()));
        let manager_b = SessionManager::new(Arc::new(InMemorySessionStore::new()));

        let session_a = manager_a
            .create_session(test_parameters())
            .await
            .expect("first manager failed to create session");
        let session_b = manager_b
            .create_session(test_parameters())
            .await
            .expect("second manager failed to create session");

        assert_ne!(
            session_a.id, session_b.id,
            "fresh managers must not reproduce the same id sequence"
        );
    }

    /// Successive creates on the same manager also yield distinct ids.
    #[tokio::test]
    async fn test_sequential_creates_have_unique_ids() {
        let manager = SessionManager::new(Arc::new(InMemorySessionStore::new()));

        let first = manager.create_session(test_parameters()).await.unwrap();
        let second = manager.create_session(test_parameters()).await.unwrap();

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
            .await
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
        let stored = store.get(id).await.expect("session missing from store");
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
            .await
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

        let stored = store.get(id).await.expect("session missing from store");
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
            .await
            .expect("failed to create session");
        let id = session.id;

        // Force the session into the Completed state directly in the store.
        let mut completed = store.get(id).await.expect("session missing from store");
        completed.current_step = completed.total_steps;
        completed.state = SessionState::Completed;
        store
            .save(completed)
            .await
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
            .await
            .expect("failed to create session");
        let id = session.id;

        let mut errored = store.get(id).await.expect("session missing from store");
        errored.state = SessionState::Error;
        store
            .save(errored)
            .await
            .expect("failed to persist errored session");

        match manager.peek_current_step(id).await {
            Err(ChainError::InvalidState(msg)) => {
                assert_eq!(msg, "Session is in error state");
            }
            other => panic!("expected InvalidState for errored peek, got {other:?}"),
        }
    }

    /// Advance on an `Error`-state session must reject before simulating,
    /// matching the peek path and never touching the store.
    #[tokio::test]
    async fn test_advance_on_error_session_returns_invalid_state() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());
        let session = manager
            .create_session(test_parameters())
            .await
            .expect("failed to create session");
        let id = session.id;

        let mut errored = store.get(id).await.expect("session missing from store");
        errored.state = SessionState::Error;
        store
            .save(errored)
            .await
            .expect("failed to persist errored session");

        match manager.get_next_step(id).await {
            Err(ChainError::InvalidState(msg)) => {
                assert_eq!(msg, "Session is in error state");
            }
            other => panic!("expected InvalidState for errored advance, got {other:?}"),
        }
        let stored = store.get(id).await.expect("session missing from store");
        assert_eq!(stored.state, SessionState::Error);
        assert_eq!(stored.current_step, 0);
    }

    /// Collects the full `steps`-long tape of a freshly created session by advancing it
    /// to completion via `get_next_step`.
    async fn collect_full_tape(
        manager: &SessionManager,
        params: SimulationParameters,
    ) -> Vec<Positive> {
        let steps = params.steps;
        let session = manager
            .create_session(params)
            .await
            .expect("failed to create session for reference tape");
        let mut tape = Vec::with_capacity(steps);
        for _ in 0..steps {
            let (_s, chain) = manager
                .get_next_step(session.id)
                .await
                .expect("reference advance failed");
            tape.push(chain.underlying_price);
        }
        tape
    }

    /// Issue #4 (PUT path): after `reinitialize_session` a session must NOT stick at
    /// step 0. Two consecutive advances serve indices 0 then 1 of the rebuilt walk,
    /// the cursor increases monotonically, and the stored state leaves `Reinitialized`
    /// after the FIRST advance — proving the walk is evicted/rebuilt exactly once
    /// rather than on every request.
    #[tokio::test]
    async fn test_reinitialized_session_advances_after_put() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());

        // Reference tape for the NEW seed so we can assert index alignment after reset.
        let ref_tape = collect_full_tape(&manager, seeded_parameters(3, 22)).await;

        // Original session (seed 11); advance once so it is mid-tape before the reset.
        let session = manager
            .create_session(seeded_parameters(3, 11))
            .await
            .expect("failed to create session");
        let id = session.id;
        manager
            .get_next_step(id)
            .await
            .expect("initial advance failed");

        // PUT: reinitialize with the new seed. Cursor resets to 0, state Reinitialized.
        let reinit = manager
            .reinitialize_session(id, seeded_parameters(3, 22))
            .await
            .expect("reinitialize failed");
        assert_eq!(reinit.current_step, 0);
        assert_eq!(reinit.state, SessionState::Reinitialized);

        // First advance after reset serves index 0 of the rebuilt walk, moves the
        // cursor to 1, and persists InProgress (Reinitialized left after ONE call).
        let (after_first, first_chain) = manager
            .get_next_step(id)
            .await
            .expect("first post-reset advance failed");
        assert_eq!(after_first.current_step, 1);
        assert_eq!(after_first.state, SessionState::InProgress);
        assert_eq!(first_chain.underlying_price, ref_tape[0]);
        let stored = store.get(id).await.expect("session missing from store");
        assert_eq!(stored.state, SessionState::InProgress);

        // Second advance serves index 1 from the CACHED walk (no rebuild), cursor -> 2.
        let (after_second, second_chain) = manager
            .get_next_step(id)
            .await
            .expect("second post-reset advance failed");
        assert_eq!(after_second.current_step, 2);
        assert_eq!(second_chain.underlying_price, ref_tape[1]);
        // Monotonic, no repeats: index 1 differs from index 0.
        assert_ne!(second_chain.underlying_price, first_chain.underlying_price);
    }

    /// Issue #4 (PATCH path): `update_session` restarts the tape, and the session
    /// advances normally afterwards (indices 0 then 1), leaving `Reinitialized` after
    /// a single advance — the same non-stuck behavior as the PUT path.
    #[tokio::test]
    async fn test_reinitialized_session_advances_after_patch() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());

        let ref_tape = collect_full_tape(&manager, seeded_parameters(3, 44)).await;

        let session = manager
            .create_session(seeded_parameters(3, 33))
            .await
            .expect("failed to create session");
        let id = session.id;
        manager
            .get_next_step(id)
            .await
            .expect("initial advance failed");

        // PATCH: update parameters. Cursor resets to 0, state Reinitialized.
        let patched = manager
            .update_session(id, seeded_parameters(3, 44))
            .await
            .expect("update failed");
        assert_eq!(patched.current_step, 0);
        assert_eq!(patched.state, SessionState::Reinitialized);

        let (after_first, first_chain) = manager
            .get_next_step(id)
            .await
            .expect("first post-reset advance failed");
        assert_eq!(after_first.current_step, 1);
        assert_eq!(after_first.state, SessionState::InProgress);
        assert_eq!(first_chain.underlying_price, ref_tape[0]);
        let stored = store.get(id).await.expect("session missing from store");
        assert_eq!(stored.state, SessionState::InProgress);

        let (after_second, second_chain) = manager
            .get_next_step(id)
            .await
            .expect("second post-reset advance failed");
        assert_eq!(after_second.current_step, 2);
        assert_eq!(second_chain.underlying_price, ref_tape[1]);
        assert_ne!(second_chain.underlying_price, first_chain.underlying_price);
    }

    /// Issue #4: reinitializing with a DIFFERENT seed rebuilds the walk from the new
    /// parameters. Index 0 of the walk is always the initial chain (seed-independent),
    /// so the difference shows from index 1 onward: the post-reset tape matches the
    /// new seed's reference tape and diverges from the original seed's tape — proving
    /// the rebuild picked up the new seed instead of replaying the old cached walk.
    #[tokio::test]
    async fn test_reinitialize_with_different_seed_changes_tape() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());

        // Reference tapes for two very different seeds.
        let ref_tape_old = collect_full_tape(&manager, seeded_parameters(3, 1000)).await;
        let ref_tape_new = collect_full_tape(&manager, seeded_parameters(3, 987654321)).await;
        // Sanity: the two seeds really do produce different tapes.
        assert_ne!(ref_tape_old, ref_tape_new);

        // Create a session on the old seed and populate its cached walk via a peek.
        let session = manager
            .create_session(seeded_parameters(3, 1000))
            .await
            .expect("failed to create session");
        let id = session.id;
        manager.peek_current_step(id).await.expect("peek failed");

        // Reinitialize with the very different seed; then walk the whole rebuilt tape.
        manager
            .reinitialize_session(id, seeded_parameters(3, 987654321))
            .await
            .expect("reinitialize failed");
        let mut post_reset = Vec::with_capacity(3);
        for _ in 0..3 {
            let (_s, chain) = manager
                .get_next_step(id)
                .await
                .expect("post-reset advance failed");
            post_reset.push(chain.underlying_price);
        }

        // The rebuilt tape matches the NEW seed and differs from the OLD seed.
        assert_eq!(post_reset, ref_tape_new);
        assert_ne!(post_reset, ref_tape_old);
    }

    /// Regression for issue #6: PATCHing `steps` must keep `parameters.steps`
    /// and `total_steps` equal, and the session must serve exactly the NEW
    /// number of snapshots — both when growing and when shrinking the tape.
    #[tokio::test]
    async fn test_patch_steps_syncs_total_steps_up_and_down() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());

        // Start with 2 steps, PATCH up to 5.
        let session = manager
            .create_session(seeded_parameters(2, 7))
            .await
            .expect("failed to create session");
        let id = session.id;

        let patched = manager
            .update_session(id, seeded_parameters(5, 7))
            .await
            .expect("update failed");
        assert_eq!(patched.parameters.steps, patched.total_steps);
        assert_eq!(patched.total_steps, 5);

        // The regenerated walk and the progression limit agree: exactly 5 advances.
        for expected_cursor in 1..=5 {
            let (s, _chain) = manager.get_next_step(id).await.expect("advance failed");
            assert_eq!(s.current_step, expected_cursor);
        }
        assert!(manager.get_next_step(id).await.is_err());
        let stored = store.get(id).await.expect("session missing from store");
        assert_eq!(stored.state, SessionState::Completed);
        assert_eq!(stored.total_steps, 5);

        // PATCH down to 3: metadata stays consistent and only 3 advances succeed.
        let patched_down = manager
            .update_session(id, seeded_parameters(3, 7))
            .await
            .expect("downward update failed");
        assert_eq!(patched_down.parameters.steps, patched_down.total_steps);
        assert_eq!(patched_down.total_steps, 3);

        for expected_cursor in 1..=3 {
            let (s, _chain) = manager.get_next_step(id).await.expect("advance failed");
            assert_eq!(s.current_step, expected_cursor);
        }
        assert!(manager.get_next_step(id).await.is_err());
    }

    /// Issue #19: with the async store trait, two `create_session` calls for
    /// DIFFERENT sessions can be in flight concurrently over one shared manager
    /// and both complete — no head-of-line blocking on a shared connection lock.
    /// `tokio::join!` drives both futures on a single task; the test proves the
    /// call sites are `&self` async and compose concurrently, and that both
    /// sessions land independently with distinct ids.
    #[tokio::test]
    async fn test_concurrent_create_sessions_do_not_serialize() {
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::new(store.clone());

        let (res_a, res_b) = tokio::join!(
            manager.create_session(test_parameters()),
            manager.create_session(test_parameters()),
        );

        let session_a = res_a.expect("concurrent create a failed");
        let session_b = res_b.expect("concurrent create b failed");

        // Two distinct sessions were persisted concurrently.
        assert_ne!(session_a.id, session_b.id);
        assert!(store.get(session_a.id).await.is_ok());
        assert!(store.get(session_b.id).await.is_ok());
    }

    /// Test store that wraps `InMemorySessionStore` and blocks every `get` on a
    /// shared `tokio::sync::Barrier` before delegating. This forces two concurrent
    /// manager mutations to BOTH read the same snapshot before either reaches its
    /// compare-and-swap save, making the lost-update race deterministic instead of
    /// timing-dependent. Every other operation passes straight through, and tests
    /// verify final state through the inner store (which never touches the barrier),
    /// so the single-generation barrier never deadlocks.
    struct BarrierGetStore {
        inner: Arc<InMemorySessionStore>,
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[async_trait::async_trait]
    impl SessionStore for BarrierGetStore {
        async fn get(&self, id: Uuid) -> Result<Session, ChainError> {
            // Read first, THEN rendezvous: both concurrent readers must complete
            // their load (observing the same stored version) before either is
            // released to proceed to its compare-and-swap save. Waiting before the
            // read would instead let the first arrival finish its whole mutation
            // before the second even reads, defeating the race we want to exercise.
            let result = self.inner.get(id).await;
            self.barrier.wait().await;
            result
        }

        async fn create(&self, session: Session) -> Result<(), ChainError> {
            self.inner.create(session).await
        }

        async fn save(&self, session: Session) -> Result<(), ChainError> {
            self.inner.save(session).await
        }

        async fn save_cas(
            &self,
            session: Session,
            expected_version: u64,
        ) -> Result<(), ChainError> {
            self.inner.save_cas(session, expected_version).await
        }

        async fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
            self.inner.delete(id).await
        }

        async fn cleanup(&self) -> Result<usize, ChainError> {
            self.inner.cleanup().await
        }
    }

    /// Issue #8 acceptance: two `get_next_step` calls that TRULY overlap (both read
    /// the session at version 0 before either saves, forced by the barrier) must not
    /// both persist. Exactly one advance wins with cursor 1; the other loses the
    /// compare-and-swap with `Conflict`. The persisted session shows a single
    /// advance — cursor 1, version 1 — so there is no lost update and no duplicate
    /// step.
    #[tokio::test]
    async fn test_concurrent_advances_one_wins_one_conflicts() {
        let inner = Arc::new(InMemorySessionStore::new());
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let store = Arc::new(BarrierGetStore {
            inner: inner.clone(),
            barrier,
        });
        let manager = Arc::new(SessionManager::new(store));

        // A session at cursor 0 with room for well over two steps.
        let session = manager
            .create_session(seeded_parameters(5, 1234))
            .await
            .expect("failed to create session");
        let id = session.id;

        let m1 = manager.clone();
        let m2 = manager.clone();
        let (r1, r2) = tokio::join!(async move { m1.get_next_step(id).await }, async move {
            m2.get_next_step(id).await
        },);

        let results = [r1, r2];
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        let conflict_count = results
            .iter()
            .filter(|r| matches!(r, Err(ChainError::Conflict(_))))
            .count();
        assert_eq!(ok_count, 1, "exactly one concurrent advance must win");
        assert_eq!(
            conflict_count, 1,
            "exactly one concurrent advance must conflict"
        );

        // The winner served exactly one step.
        let winner = results
            .iter()
            .find_map(|r| r.as_ref().ok())
            .expect("a winning advance");
        assert_eq!(winner.0.current_step, 1);

        // Persisted state proves a single, non-duplicated advance.
        let stored = inner.get(id).await.expect("session missing from store");
        assert_eq!(stored.current_step, 1);
        assert_eq!(stored.version, 1);
    }

    /// Issue #8: a PATCH racing an advance is the same lost-update hazard. With both
    /// forced to read version 0, exactly one of `update_session` / `get_next_step`
    /// commits and the other gets `Conflict`; the store ends at version 1 (a single
    /// successful mutation), so neither silently clobbers the other.
    #[tokio::test]
    async fn test_concurrent_patch_and_advance_one_conflicts() {
        let inner = Arc::new(InMemorySessionStore::new());
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let store = Arc::new(BarrierGetStore {
            inner: inner.clone(),
            barrier,
        });
        let manager = Arc::new(SessionManager::new(store));

        let session = manager
            .create_session(seeded_parameters(5, 4321))
            .await
            .expect("failed to create session");
        let id = session.id;

        let m1 = manager.clone();
        let m2 = manager.clone();
        let (advance, patch) = tokio::join!(
            async move { m1.get_next_step(id).await.map(|_| ()) },
            async move {
                m2.update_session(id, seeded_parameters(7, 4321))
                    .await
                    .map(|_| ())
            },
        );

        let results = [advance, patch];
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        let conflict_count = results
            .iter()
            .filter(|r| matches!(r, Err(ChainError::Conflict(_))))
            .count();
        assert_eq!(ok_count, 1, "exactly one of PATCH/advance must win");
        assert_eq!(
            conflict_count, 1,
            "exactly one of PATCH/advance must conflict"
        );

        // A single successful mutation left the stored revision at 1.
        let stored = inner.get(id).await.expect("session missing from store");
        assert_eq!(stored.version, 1);
    }
}
