//! Persistence contract for v2 rolling simulations.
//!
//! Deliberately a **separate trait** from [`crate::session::SessionStore`]
//! rather than a generic parameter on it. The two contracts store different
//! documents under different key spaces, and keeping them apart is what
//! guarantees ADR 0001 §12.2: a v2 id can never resolve a v1 session, and a v1
//! document is never reinterpreted as rolling configuration. The v1 trait,
//! including its `cleanup() -> usize` signature, is untouched.

use crate::session::model_v2::SessionV2;
use crate::utils::error::ChainError;
use async_trait::async_trait;
use uuid::Uuid;

/// A storage backend for v2 rolling simulations.
///
/// Implementations must be thread-safe and shareable (`Send + Sync`).
#[async_trait]
pub trait SimulationStore: Send + Sync {
    /// Retrieves a simulation by id.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::NotFound`] when no simulation has that id, or
    /// another [`ChainError`] on a storage or deserialization failure.
    async fn get(&self, id: Uuid) -> Result<SessionV2, ChainError>;

    /// Inserts a brand-new simulation, failing on an id collision.
    ///
    /// Never overwrites: a colliding id is rejected rather than silently
    /// clobbering a live simulation, which is what makes a fresh manager safe
    /// after a restart or on a second replica.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::AlreadyExists`] on an id collision, or another
    /// [`ChainError`] on a storage or serialization failure.
    async fn create(&self, simulation: SessionV2) -> Result<(), ChainError>;

    /// Atomically persists `simulation` only if the stored revision still
    /// equals `expected_version`.
    ///
    /// The v2 counterpart of `SessionStore::save_cas`, and the **only** write
    /// path for an existing simulation — there is no blind upsert, because a v2
    /// simulation is immutable after creation and the sole mutation is the
    /// cursor advance, which must not lose a concurrent writer's work.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::NotFound`] when the id is absent,
    /// [`ChainError::Conflict`] when the stored revision differs (the store is
    /// left untouched), or another [`ChainError`] on a backend failure.
    async fn save_cas(
        &self,
        simulation: SessionV2,
        expected_version: u64,
    ) -> Result<(), ChainError>;

    /// Deletes a simulation.
    ///
    /// # Errors
    ///
    /// Returns a [`ChainError`] when the deletion fails; a missing id is
    /// `Ok(false)`, not an error.
    async fn delete(&self, id: Uuid) -> Result<bool, ChainError>;

    /// Removes simulations that have been idle past their retention window and
    /// returns **their ids**.
    ///
    /// Returning the ids rather than a count is the point: the caller owns
    /// heavyweight per-simulation domain caches, and without the ids it cannot
    /// evict the entries whose sessions have just gone away. Issue #48 makes
    /// the retention window configurable and wires the eviction; this
    /// signature is what lets it.
    ///
    /// # Errors
    ///
    /// Returns a [`ChainError`] when the cleanup itself fails.
    async fn cleanup(&self) -> Result<Vec<Uuid>, ChainError>;
}
