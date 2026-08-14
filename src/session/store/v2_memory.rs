//! In-memory [`SimulationStore`] for v2 rolling simulations.
//!
//! A **separate map** from [`crate::session::InMemorySessionStore`], not a
//! second key space inside it. That is what makes ADR 0001 §12.2's isolation
//! structural rather than conventional: a v2 id cannot resolve a v1 session
//! because the two never share a container.

use crate::session::model_v2::SessionV2;
use crate::session::store::v2_interface::SimulationStore;
use crate::utils::error::ChainError;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

/// Default retention for a v2 simulation, in seconds — shared by **both**
/// backends and by the configuration.
///
/// Re-exported from [`crate::infrastructure::DEFAULT_RETENTION_SECS`] rather
/// than restated, because ADR 0001 §9.1 requires the in-memory and Redis stores
/// to agree on expiry and a second literal is exactly how that agreement
/// drifts. An operator changes it through `OCS_V2_RETENTION_SECS`; this is only
/// the value a store built without one applies.
///
/// The window is measured from the **last write**, not the last access: ADR
/// 0001 §6 defines the snapshot endpoint as a safe peek that persists nothing,
/// so a client that only peeks does not refresh it. Both backends behave the
/// same way.
pub use crate::infrastructure::DEFAULT_RETENTION_SECS as DEFAULT_V2_RETENTION_SECS;

/// In-memory store for v2 rolling simulations.
pub struct InMemorySimulationStore {
    simulations: Arc<Mutex<HashMap<Uuid, SessionV2>>>,
    idle_retention: Duration,
}

impl Default for InMemorySimulationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySimulationStore {
    /// Creates a store with the default idle retention.
    #[must_use]
    pub fn new() -> Self {
        Self::with_idle_retention(Duration::from_secs(DEFAULT_V2_RETENTION_SECS))
    }

    /// Creates a store with an explicit idle retention window.
    #[must_use]
    pub fn with_idle_retention(idle_retention: Duration) -> Self {
        Self {
            simulations: Arc::new(Mutex::new(HashMap::new())),
            idle_retention,
        }
    }

    /// The idle retention window this store applies.
    #[must_use]
    pub fn idle_retention(&self) -> Duration {
        self.idle_retention
    }

    /// Locks the map, mapping a poisoned lock into the error boundary rather
    /// than panicking on a request path.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<Uuid, SessionV2>>, ChainError> {
        self.simulations.lock().map_err(|_| {
            ChainError::Internal("Failed to acquire lock on simulation store".to_string())
        })
    }
}

#[async_trait]
impl SimulationStore for InMemorySimulationStore {
    async fn get(&self, id: Uuid) -> Result<SessionV2, ChainError> {
        let simulations = self.lock()?;

        simulations
            .get(&id)
            .cloned()
            .ok_or_else(|| ChainError::NotFound(format!("Simulation with id {id} not found")))
    }

    async fn create(&self, simulation: SessionV2) -> Result<(), ChainError> {
        // The in-memory backend serves what it was handed, so an unvalidated
        // document would be served for the rest of the process's life without
        // a round trip through serde ever catching it.
        simulation.validate()?;

        let mut simulations = self.lock()?;

        if simulations.contains_key(&simulation.id) {
            return Err(ChainError::AlreadyExists(format!(
                "Simulation with id {} already exists",
                simulation.id
            )));
        }

        simulations.insert(simulation.id, simulation);
        Ok(())
    }

    async fn save_cas(
        &self,
        simulation: SessionV2,
        expected_version: u64,
    ) -> Result<(), ChainError> {
        simulation.validate()?;

        // The whole compare-and-swap runs under the map lock, held only for
        // this synchronous section (no `.await` inside), so the read of the
        // stored revision and the conditional write are one atomic step.
        let mut simulations = self.lock()?;

        match simulations.get(&simulation.id) {
            None => Err(ChainError::NotFound(format!(
                "Simulation with id {} not found",
                simulation.id
            ))),
            Some(existing) if existing.version != expected_version => {
                Err(ChainError::Conflict(format!(
                    "Simulation {} was modified concurrently (expected version {}, found {})",
                    simulation.id, expected_version, existing.version
                )))
            }
            Some(_) => {
                simulations.insert(simulation.id, simulation);
                Ok(())
            }
        }
    }

    async fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
        let mut simulations = self.lock()?;

        Ok(simulations.remove(&id).is_some())
    }

    async fn cleanup(&self) -> Result<Vec<Uuid>, ChainError> {
        let now = SystemTime::now();
        let mut simulations = self.lock()?;

        // A document stamped in the future — a clock step, or a peer with a
        // skewed clock — is kept rather than expired, matching v1's behaviour.
        let expired: Vec<Uuid> = simulations
            .iter()
            .filter_map(
                |(id, simulation)| match now.duration_since(simulation.updated_at) {
                    Ok(idle) if idle > self.idle_retention => Some(*id),
                    _ => None,
                },
            )
            .collect();

        for id in &expired {
            simulations.remove(id);
        }

        Ok(expired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
    use crate::api::rest::requests_v2::CreateSimulationRequest;
    use crate::domain::expiry::{ExpiryRule, ExpiryRuleKind};
    use crate::session::model::SessionState;
    use crate::session::model_v2::SimulationParametersV2;
    use crate::utils::UuidGenerator;
    use chrono::{TimeZone, Utc, Weekday};

    fn request() -> CreateSimulationRequest {
        let rule = match ExpiryRule::new("zero_dte", ExpiryRuleKind::Daily, 1) {
            Ok(rule) => rule,
            Err(error) => panic!("test rule must be valid: {error}"),
        };
        let weeklies = match ExpiryRule::new(
            "weeklies",
            ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Fri]),
            2,
        ) {
            Ok(rule) => rule,
            Err(error) => panic!("test rule must be valid: {error}"),
        };
        let start_at = match Utc.with_ymd_and_hms(2026, 1, 5, 14, 30, 0).single() {
            Some(instant) => instant,
            None => panic!("test instant must be valid"),
        };

        CreateSimulationRequest {
            symbol: "SPX".to_string(),
            steps: 10,
            start_at: Some(start_at),
            step_interval_seconds: Some(86_400),
            timezone: "America/New_York".to_string(),
            calendar: None,
            expiration_time: "17:00".to_string(),
            schedules: vec![rule, weeklies],
            initial_price: 5000.0,
            volatility: 0.18,
            risk_free_rate: 0.04,
            dividend_yield: 0.0,
            method: ApiWalkType::Brownian {
                dt: 0.004,
                drift: 0.0,
                volatility: 0.18,
            },
            time_frame: ApiTimeFrame::Day,
            chain_size: Some(15),
            strike_interval: Some(25.0),
            skew_slope: None,
            smile_curve: None,
            spread: Some(0.02),
            seed: Some(42),
        }
    }

    fn simulation() -> SessionV2 {
        let parameters = match SimulationParametersV2::try_from(request()) {
            Ok(parameters) => parameters,
            Err(error) => panic!("the request must convert: {error}"),
        };
        let namespace = match uuid::Uuid::parse_str(crate::session::manager::DEFAULT_NAMESPACE) {
            Ok(namespace) => namespace,
            Err(error) => panic!("the default namespace must parse: {error}"),
        };
        SessionV2::new(parameters, &UuidGenerator::new(namespace))
    }

    /// An invalid simulation is refused on the way in.
    ///
    /// `SessionV2` has public, mutable fields, so a crate caller can hand the
    /// store a document no constructor would produce. This backend never
    /// round-trips through serde, so without the check it would keep serving
    /// that document for the life of the process.
    #[tokio::test]
    async fn test_create_rejects_an_invalid_simulation() {
        let store = InMemorySimulationStore::new();
        let mut invalid = simulation();
        invalid.current_step = invalid.total_steps + 1;

        match store.create(invalid.clone()).await {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "current_step"),
            other => panic!("must reject the invalid cursor, got {other:?}"),
        }

        match store.get(invalid.id).await {
            Err(ChainError::NotFound(_)) => {}
            other => panic!("nothing must have been stored, got {other:?}"),
        }
    }

    /// The same refusal on the compare-and-swap path.
    #[tokio::test]
    async fn test_save_cas_rejects_an_invalid_simulation() {
        let store = InMemorySimulationStore::new();
        let original = simulation();
        match store.create(original.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("must create: {error}"),
        }

        let mut invalid = original.clone();
        invalid.state = SessionState::Completed;

        match store.save_cas(invalid, original.version).await {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "state"),
            other => panic!("must reject the contradictory state, got {other:?}"),
        }
    }

    /// The reference configuration round-trips through the store unchanged.
    #[tokio::test]
    async fn test_create_then_get_round_trips_the_simulation() {
        let store = InMemorySimulationStore::new();
        let original = simulation();

        match store.create(original.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("must create: {error}"),
        }

        match store.get(original.id).await {
            Ok(loaded) => assert_eq!(loaded, original),
            Err(error) => panic!("must load: {error}"),
        }
    }

    /// A missing id is `NotFound`, not a default document.
    #[tokio::test]
    async fn test_get_missing_id_is_not_found() {
        let store = InMemorySimulationStore::new();

        match store.get(Uuid::new_v4()).await {
            Err(ChainError::NotFound(message)) => assert!(message.contains("Simulation")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// A duplicate id is rejected rather than overwriting a live simulation.
    #[tokio::test]
    async fn test_duplicate_id_is_rejected() {
        let store = InMemorySimulationStore::new();
        let original = simulation();

        match store.create(original.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("must create: {error}"),
        }
        match store.create(original).await {
            Err(ChainError::AlreadyExists(_)) => {}
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    /// A compare-and-swap at the stored revision commits.
    #[tokio::test]
    async fn test_save_cas_at_the_stored_revision_commits() {
        let store = InMemorySimulationStore::new();
        let mut sim = simulation();
        match store.create(sim.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("must create: {error}"),
        }

        // Advancing the cursor moves the lifecycle with it — the pair is
        // validated on every write now.
        sim.current_step = 1;
        sim.state = SessionState::InProgress;
        let expected = match sim.bump_version() {
            Ok(expected) => expected,
            Err(error) => panic!("must bump: {error}"),
        };

        match store.save_cas(sim.clone(), expected).await {
            Ok(()) => {}
            Err(error) => panic!("must commit: {error}"),
        }
        match store.get(sim.id).await {
            Ok(loaded) => {
                assert_eq!(loaded.current_step, 1);
                assert_eq!(loaded.version, 1);
            }
            Err(error) => panic!("must load: {error}"),
        }
    }

    /// Two writers that read the same revision produce one winner and one
    /// documented conflict.
    #[tokio::test]
    async fn test_concurrent_save_cas_yields_one_winner() {
        let store = InMemorySimulationStore::new();
        let sim = simulation();
        match store.create(sim.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("must create: {error}"),
        }

        let mut first = sim.clone();
        first.current_step = 1;
        first.state = SessionState::InProgress;
        let first_expected = match first.bump_version() {
            Ok(expected) => expected,
            Err(error) => panic!("must bump: {error}"),
        };

        let mut second = sim;
        second.current_step = 1;
        second.state = SessionState::InProgress;
        let second_expected = match second.bump_version() {
            Ok(expected) => expected,
            Err(error) => panic!("must bump: {error}"),
        };

        match store.save_cas(first, first_expected).await {
            Ok(()) => {}
            Err(error) => panic!("the first writer must commit: {error}"),
        }
        match store.save_cas(second, second_expected).await {
            Err(ChainError::Conflict(message)) => assert!(message.contains("concurrently")),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    /// A compare-and-swap against a missing id is `NotFound`.
    #[tokio::test]
    async fn test_save_cas_on_a_missing_id_is_not_found() {
        let store = InMemorySimulationStore::new();

        match store.save_cas(simulation(), 0).await {
            Err(ChainError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// Delete reports whether anything was removed, and a second delete is not
    /// an error.
    #[tokio::test]
    async fn test_delete_reports_whether_it_removed_anything() {
        let store = InMemorySimulationStore::new();
        let sim = simulation();
        match store.create(sim.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("must create: {error}"),
        }

        match store.delete(sim.id).await {
            Ok(removed) => assert!(removed),
            Err(error) => panic!("must delete: {error}"),
        }
        match store.delete(sim.id).await {
            Ok(removed) => assert!(!removed),
            Err(error) => panic!("a second delete must not error: {error}"),
        }
    }

    /// Cleanup returns the ids it expired, which is what lets the caller evict
    /// the matching domain caches.
    #[tokio::test]
    async fn test_cleanup_returns_the_expired_ids() {
        let store = InMemorySimulationStore::with_idle_retention(Duration::from_secs(1));
        let mut stale = simulation();
        stale.updated_at = SystemTime::now() - Duration::from_secs(3_600);
        let fresh = simulation();
        let fresh = SessionV2 {
            id: Uuid::new_v4(),
            ..fresh
        };

        match store.create(stale.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("must create: {error}"),
        }
        match store.create(fresh.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("must create: {error}"),
        }

        match store.cleanup().await {
            Ok(expired) => assert_eq!(expired, vec![stale.id]),
            Err(error) => panic!("must clean up: {error}"),
        }

        assert!(store.get(stale.id).await.is_err());
        assert!(store.get(fresh.id).await.is_ok());
    }

    /// A simulation whose idle time is inside the retention window survives, so
    /// a long simulated horizon is not cut short by a real-time timeout.
    #[tokio::test]
    async fn test_cleanup_keeps_a_simulation_inside_its_retention_window() {
        let store = InMemorySimulationStore::with_idle_retention(Duration::from_secs(3_600));
        let mut sim = simulation();
        sim.updated_at = SystemTime::now() - Duration::from_secs(600);
        match store.create(sim.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("must create: {error}"),
        }

        match store.cleanup().await {
            Ok(expired) => assert!(expired.is_empty()),
            Err(error) => panic!("must clean up: {error}"),
        }
        assert!(store.get(sim.id).await.is_ok());
    }

    /// A document stamped in the future is kept rather than expired, matching
    /// the v1 store's behaviour under a clock step.
    #[tokio::test]
    async fn test_cleanup_keeps_a_future_stamped_simulation() {
        let store = InMemorySimulationStore::with_idle_retention(Duration::from_secs(1));
        let mut sim = simulation();
        sim.updated_at = SystemTime::now() + Duration::from_secs(3_600);
        match store.create(sim.clone()).await {
            Ok(()) => {}
            Err(error) => panic!("must create: {error}"),
        }

        match store.cleanup().await {
            Ok(expired) => assert!(expired.is_empty()),
            Err(error) => panic!("must clean up: {error}"),
        }
    }

    /// The default retention is longer than v1's, because a v2 simulation is
    /// walked one request at a time over a long simulated horizon.
    #[tokio::test]
    async fn test_default_retention_is_the_documented_window() {
        let store = InMemorySimulationStore::new();

        assert_eq!(
            store.idle_retention(),
            Duration::from_secs(DEFAULT_V2_RETENTION_SECS)
        );
    }
}
