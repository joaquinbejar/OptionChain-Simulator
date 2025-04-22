use std::string::ToString;
use crate::domain::Simulator;
use crate::session::SessionStore;
use crate::session::model::{Session, SimulationParameters};
use crate::session::state_handler::StateProgressionHandler;
use crate::utils::error::ChainError;
use optionstratlib::chains::OptionChain;
use std::sync::Arc;
use uuid::Uuid;
use crate::utils::UuidGenerator;

const DEFAULT_NAMESPACE: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

/// Manages the lifecycle of simulation sessions
pub struct SessionManager {
    store: Arc<dyn SessionStore>,
    state_handler: StateProgressionHandler,
    simulator: Simulator,
    uuid_generator: UuidGenerator
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

        // Create a new namespace for each session manager instance
        // let namespace_uuid = Uuid::new_v4().to_string();
        // let namespace = Uuid::parse_str(&namespace_uuid)
        //     .expect("Failed to parse default UUID namespace");
        
        // Create a default namespace for compatibility
        let namespace = Uuid::parse_str(DEFAULT_NAMESPACE)
            .expect("Failed to parse default UUID namespace");
        let uuid_generator = UuidGenerator::new(namespace);
        Self {
            store,
            state_handler: StateProgressionHandler::new(),
            simulator: Simulator::new(),
            uuid_generator,
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
        let session = Session::new(params, &self.uuid_generator);
        self.store.save(session.clone())?;
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

    /// Retrieves the next step in the session workflow using the provided session ID.
    ///
    /// This function performs the following operations:
    /// 1. Fetches the session corresponding to the given `id` from the session store.
    /// 2. Advances the session's state using the `state_handler`.
    /// 3. Generates an option chain based on the session's updated state using the `simulator`.
    /// 4. Saves the updated session back to the session store.
    /// 5. Returns the updated `Session` and the generated `OptionChain`.
    ///
    /// # Arguments
    ///
    /// * `id` - A `Uuid` representing the unique identifier of the session.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing:
    /// - A tuple `(Session, OptionChain)` if the operation succeeds:
    ///     - `Session`: The updated session after advancing its state.
    ///     - `OptionChain`: The generated options for the current step of the session.
    /// - A `ChainError` if there is an error during any step of the process.
    ///
    /// # Errors
    ///
    /// This function may return the following errors encapsulated in `ChainError`:
    /// - If the session cannot be retrieved from the store, an error from the store implementation is propagated.
    /// - If an issue occurs while advancing the session's state, the error is propagated.
    /// - If the simulator fails during the generation of the option chain, a `ChainError::SimulatorError`
    ///   is returned, detailing the simulation failure.
    /// - If the updated session fails to be saved back to the store, an error from the store implementation is propagated.
    ///
    pub async fn get_next_step(&self, id: Uuid) -> Result<(Session, OptionChain), ChainError> {

        let mut session = self.store.get(id)?;

        // Advance session state
        self.state_handler.advance_state(&mut session)?;

        // Generate option chain for current step
        let chain = self
            .simulator
            .simulate_next_step(&session)
            .await
            .map_err(|e| ChainError::SimulatorError(e.to_string()))?;

        // Save updated session
        self.store.save(session.clone())?;

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
