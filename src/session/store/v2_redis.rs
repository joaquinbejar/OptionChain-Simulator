//! Redis-backed [`SimulationStore`] for v2 rolling simulations.
//!
//! Uses its own key prefix — `optionchain:simulation:v2:` by default — so a v2
//! document and a v1 session can never collide, and a v1 document is never
//! read back as rolling configuration (ADR 0001 §12.2). The compare-and-swap
//! itself is the same server-side Lua script the v1 store uses
//! ([`SAVE_CAS_SCRIPT`]), which is parameterised entirely by its keys, so both
//! key spaces share one already-reviewed implementation.
//!
//! # Why there is an index set
//!
//! Redis expires keys on its own, invisibly to this process. That is fine for
//! v1, whose `cleanup` honestly reports `0`. It is not fine for v2: the caller
//! owns heavyweight per-simulation domain caches and needs the **ids** that
//! went away in order to evict them, which is exactly what
//! [`SimulationStore::cleanup`] promises. So every live simulation id is also
//! held in a set, and cleanup reports the members whose documents have since
//! expired, pruning the set as it goes.

use crate::infrastructure::RedisClient;
use crate::session::model_v2::SessionV2;
use crate::session::store::in_redis::SAVE_CAS_SCRIPT;
use crate::session::store::v2_interface::SimulationStore;
use crate::utils::error::ChainError;
use async_trait::async_trait;
use redis::RedisError;
use std::sync::Arc;
use tracing::{debug, error, info, instrument};
use uuid::Uuid;

/// Default Redis key prefix for v2 simulations.
///
/// Distinct from v1's `session:` / `optionchain:session:` prefixes, which is
/// what keeps the two key spaces from ever meeting.
pub const DEFAULT_V2_KEY_PREFIX: &str = "optionchain:simulation:v2:";

/// Default idle retention for a stored v2 simulation, in seconds.
///
/// Longer than v1's, because a simulation whose simulated clock spans years is
/// still walked one request at a time. Issue #48 makes it configurable.
pub const DEFAULT_V2_SESSION_TTL_SECS: u64 = 3_600;

/// Redis-backed store for v2 rolling simulations.
pub struct InRedisSimulationStore {
    client: Arc<RedisClient>,
    key_prefix: String,
    session_ttl: u64,
}

impl InRedisSimulationStore {
    /// Creates a Redis-backed simulation store.
    ///
    /// `key_prefix` defaults to [`DEFAULT_V2_KEY_PREFIX`] and `session_ttl` to
    /// [`DEFAULT_V2_SESSION_TTL_SECS`].
    #[instrument(skip(client), level = "debug")]
    pub fn new(
        client: Arc<RedisClient>,
        key_prefix: Option<String>,
        session_ttl: Option<u64>,
    ) -> Self {
        let prefix = key_prefix.unwrap_or_else(|| DEFAULT_V2_KEY_PREFIX.to_string());
        let ttl = session_ttl.unwrap_or(DEFAULT_V2_SESSION_TTL_SECS);

        info!(
            key_prefix = %prefix,
            session_ttl = ttl,
            "Created new Redis simulation store"
        );

        Self {
            client,
            key_prefix: prefix,
            session_ttl: ttl,
        }
    }

    /// The key holding a simulation document.
    fn simulation_key(&self, id: Uuid) -> String {
        format!("{}{}", self.key_prefix, id)
    }

    /// The companion key holding a simulation's revision as an integer string.
    ///
    /// Kept out of the JSON for the same reason as v1: `cjson` would round a
    /// `u64` through an IEEE-754 double, collapsing adjacent revisions above
    /// `2^53` and letting a stale writer pass the compare-and-swap.
    fn version_key(&self, id: Uuid) -> String {
        format!("{}:ver", self.simulation_key(id))
    }

    /// The set holding every live simulation id.
    fn index_key(&self) -> String {
        format!("{}index", self.key_prefix)
    }

    /// The retention window applied to stored simulations, in seconds.
    #[must_use]
    pub fn session_ttl(&self) -> u64 {
        self.session_ttl
    }

    /// Maps a Redis error into the crate's error boundary.
    ///
    /// The message carries the driver's text but never the connection string,
    /// which `RedisClient` keeps to itself.
    #[cold]
    fn map_redis_error(err: RedisError) -> ChainError {
        ChainError::Internal(format!("Redis error: {err}"))
    }

    /// Translates the integer result code returned by [`SAVE_CAS_SCRIPT`].
    ///
    /// Pure and side-effect free so the mapping is unit-testable without a live
    /// Redis. `1` committed, `-1` means the key is gone, `-2` means another
    /// writer won the race.
    fn map_cas_result(code: i64, id: Uuid, expected_version: u64) -> Result<(), ChainError> {
        match code {
            1 => Ok(()),
            -1 => Err(ChainError::NotFound(format!(
                "Simulation with id {id} not found"
            ))),
            -2 => Err(ChainError::Conflict(format!(
                "Simulation {id} was modified concurrently (expected version {expected_version})"
            ))),
            other => Err(ChainError::Internal(format!(
                "Unexpected CAS result code {other} for simulation {id}"
            ))),
        }
    }
}

#[async_trait]
impl SimulationStore for InRedisSimulationStore {
    #[instrument(skip(self), level = "debug")]
    async fn get(&self, id: Uuid) -> Result<SessionV2, ChainError> {
        let key = self.simulation_key(id);
        debug!(simulation_id = %id, key = %key, "Getting simulation from Redis");

        match self.client.get::<String>(&key).await {
            Ok(Some(json)) => serde_json::from_str::<SessionV2>(&json).map_err(|e| {
                error!(simulation_id = %id, error = %e, "Failed to deserialize simulation");
                ChainError::Internal(format!("Failed to deserialize simulation: {e}"))
            }),
            Ok(None) => Err(ChainError::NotFound(format!(
                "Simulation with id {id} not found"
            ))),
            Err(e) => {
                error!(simulation_id = %id, error = %e, "Redis error while getting simulation");
                Err(Self::map_redis_error(e))
            }
        }
    }

    #[instrument(skip(self, simulation), level = "debug")]
    async fn create(&self, simulation: SessionV2) -> Result<(), ChainError> {
        let id = simulation.id;
        let key = self.simulation_key(id);
        let version_key = self.version_key(id);
        debug!(simulation_id = %id, key = %key, "Creating simulation in Redis");

        let json = serde_json::to_string(&simulation).map_err(|e| {
            error!(simulation_id = %id, error = %e, "Failed to serialize simulation");
            ChainError::Internal(format!("Failed to serialize simulation: {e}"))
        })?;

        // SET NX guarantees we never overwrite an existing id.
        match self.client.set_nx(&key, json, Some(self.session_ttl)).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(ChainError::AlreadyExists(format!(
                    "Simulation with id {id} already exists"
                )));
            }
            Err(e) => {
                error!(simulation_id = %id, error = %e, "Redis error while creating simulation");
                return Err(Self::map_redis_error(e));
            }
        }

        // Seed the companion revision key the CAS script compares against.
        // Neither this nor the index write is a race surface — only `save_cas`
        // races — so plain sequential commands are correct here, and the script
        // treats a missing `:ver` key as revision 0.
        self.client
            .set(
                &version_key,
                simulation.version.to_string(),
                Some(self.session_ttl),
            )
            .await
            .map_err(Self::map_redis_error)?;

        let mut conn = self.client.connection_manager();
        let _: () = redis::cmd("SADD")
            .arg(self.index_key())
            .arg(id.to_string())
            .query_async(&mut conn)
            .await
            .map_err(Self::map_redis_error)?;

        debug!(simulation_id = %id, "Simulation created successfully");
        Ok(())
    }

    #[instrument(skip(self, simulation), level = "debug")]
    async fn save_cas(
        &self,
        simulation: SessionV2,
        expected_version: u64,
    ) -> Result<(), ChainError> {
        let id = simulation.id;
        let key = self.simulation_key(id);
        let version_key = self.version_key(id);
        debug!(simulation_id = %id, expected_version, "CAS-saving simulation to Redis");

        let json = serde_json::to_string(&simulation).map_err(|e| {
            error!(simulation_id = %id, error = %e, "Failed to serialize simulation");
            ChainError::Internal(format!("Failed to serialize simulation: {e}"))
        })?;

        // Versions cross the boundary as STRINGS so the Lua comparison is exact
        // for every `u64`. `simulation.version` was already bumped past
        // `expected_version` by the caller.
        let mut conn = self.client.connection_manager();
        let code: i64 = redis::Script::new(SAVE_CAS_SCRIPT)
            .key(&key)
            .key(&version_key)
            .arg(json)
            .arg(expected_version.to_string())
            .arg(simulation.version.to_string())
            .arg(self.session_ttl)
            .invoke_async(&mut conn)
            .await
            .map_err(Self::map_redis_error)?;

        Self::map_cas_result(code, id, expected_version).inspect_err(|e| {
            debug!(simulation_id = %id, error = %e, "CAS save rejected");
        })
    }

    #[instrument(skip(self), level = "debug")]
    async fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
        let key = self.simulation_key(id);
        let version_key = self.version_key(id);
        debug!(simulation_id = %id, key = %key, "Deleting simulation from Redis");

        // The document's own delete result decides whether a simulation
        // existed; removing an already-absent companion or index member is
        // harmless.
        let deleted = self
            .client
            .delete(&key)
            .await
            .map_err(Self::map_redis_error)?;
        self.client
            .delete(&version_key)
            .await
            .map_err(Self::map_redis_error)?;

        let mut conn = self.client.connection_manager();
        let _: () = redis::cmd("SREM")
            .arg(self.index_key())
            .arg(id.to_string())
            .query_async(&mut conn)
            .await
            .map_err(Self::map_redis_error)?;

        debug!(simulation_id = %id, deleted, "Simulation delete result");
        Ok(deleted)
    }

    #[instrument(skip(self), level = "debug")]
    async fn cleanup(&self) -> Result<Vec<Uuid>, ChainError> {
        let index_key = self.index_key();
        let mut conn = self.client.connection_manager();

        let members: Vec<String> = redis::cmd("SMEMBERS")
            .arg(&index_key)
            .query_async(&mut conn)
            .await
            .map_err(Self::map_redis_error)?;

        let mut expired = Vec::new();
        for member in members {
            // A member that is not a uuid cannot correspond to a document;
            // drop it from the index rather than carrying it forever.
            let Ok(id) = Uuid::parse_str(&member) else {
                let _: () = redis::cmd("SREM")
                    .arg(&index_key)
                    .arg(&member)
                    .query_async(&mut conn)
                    .await
                    .map_err(Self::map_redis_error)?;
                continue;
            };

            let exists = self
                .client
                .exists(&self.simulation_key(id))
                .await
                .map_err(Self::map_redis_error)?;
            if exists {
                continue;
            }

            // Redis already expired the document; drop the leftovers and report
            // the id so the caller can evict its domain caches.
            self.client
                .delete(&self.version_key(id))
                .await
                .map_err(Self::map_redis_error)?;
            let _: () = redis::cmd("SREM")
                .arg(&index_key)
                .arg(&member)
                .query_async(&mut conn)
                .await
                .map_err(Self::map_redis_error)?;
            expired.push(id);
        }

        if !expired.is_empty() {
            info!(count = expired.len(), "Reaped expired simulations");
        }
        Ok(expired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any_id() -> Uuid {
        Uuid::nil()
    }

    /// A committed compare-and-swap maps to `Ok`.
    #[test]
    fn test_map_cas_result_committed_is_ok() {
        assert!(InRedisSimulationStore::map_cas_result(1, any_id(), 3).is_ok());
    }

    /// A missing key maps to `NotFound`, naming the simulation.
    #[test]
    fn test_map_cas_result_missing_key_is_not_found() {
        match InRedisSimulationStore::map_cas_result(-1, any_id(), 3) {
            Err(ChainError::NotFound(message)) => assert!(message.contains("Simulation")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// A revision mismatch maps to `Conflict`, carrying the expected revision.
    #[test]
    fn test_map_cas_result_version_mismatch_is_conflict() {
        match InRedisSimulationStore::map_cas_result(-2, any_id(), 7) {
            Err(ChainError::Conflict(message)) => assert!(message.contains('7')),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    /// An unexpected code is an internal error rather than a silent success.
    #[test]
    fn test_map_cas_result_unknown_code_is_internal() {
        match InRedisSimulationStore::map_cas_result(42, any_id(), 0) {
            Err(ChainError::Internal(message)) => assert!(message.contains("42")),
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
