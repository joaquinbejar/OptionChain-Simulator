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
