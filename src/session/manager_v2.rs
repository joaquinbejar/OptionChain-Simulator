//! Lifecycle for v2 rolling simulations.
//!
//! The v2 counterpart of [`crate::session::SessionManager`]: it owns the
//! [`SimulationStore`], the per-simulation factor tapes, and the bounded
//! snapshot cache, and it is the only thing the API layer talks to. Handlers
//! never reach into `domain` — that module is private, and the layering says
//! api → session → domain.
//!
//! # What it guarantees
//!
//! - **Serve-then-advance.** `advance` serves the snapshot at the current
//!   cursor and *then* moves it, so a simulation with `steps = N` serves
//!   exactly indices `0..N-1` over `N` advances. This is v1's semantics,
//!   deliberately carried forward.
//! - **A peek changes nothing.** `peek` builds the same snapshot and writes
//!   nothing back, so calling it repeatedly is safe and returns the same
//!   answer until an advance moves the cursor.
//! - **No lost advance.** Every advance persists through a compare-and-swap on
//!   the revision it read, so two concurrent advances cannot both commit: the
//!   loser gets a `Conflict` and retries.
//! - **Caches are never authoritative.** A factor tape or a snapshot can be
//!   dropped at any time; both rebuild identically from the effective
//!   parameters, so eviction changes latency and nothing else.

use crate::domain::factors::FactorTape;
use crate::domain::series::{SeriesBuilder, SeriesSnapshot, SnapshotCache};
use crate::infrastructure::SimulationV2Config;
use crate::session::manager::DEFAULT_NAMESPACE;
use crate::session::model::SessionState;
use crate::session::store::SimulationStore;
use crate::session::{SessionV2, SimulationParametersV2};
use crate::utils::{ChainError, UuidGenerator};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{debug, info, instrument};
use uuid::Uuid;

/// The UUID v5 namespace simulations are generated under.
///
/// The same namespace v1 uses, parsed once. It is a compile-time constant that
/// has been valid since the crate's first release; a parse failure here would
/// mean the constant itself was edited to something malformed, so falling back
/// to the nil namespace keeps the service up rather than aborting a request
/// path — ids stay unique either way, since the generator counts within the
/// namespace.
#[must_use]
fn default_namespace() -> Uuid {
    Uuid::parse_str(DEFAULT_NAMESPACE).unwrap_or(Uuid::nil())
}

/// One cached factor tape and the last time it was used.
struct TapeEntry {
    tape: FactorTape,
    last_access: Instant,
}

/// Owns the lifecycle of v2 rolling simulations.
pub struct SimulationManager {
    store: Arc<dyn SimulationStore>,
    uuid_generator: UuidGenerator,
    config: SimulationV2Config,
    tapes: Mutex<HashMap<Uuid, TapeEntry>>,
    snapshots: Mutex<SnapshotCache>,
}

impl SimulationManager {
    /// Creates a manager over a simulation store.
    ///
    /// The only method the binary needs; everything else is crate-internal,
    /// because it deals in `domain` types that are not part of this crate's
    /// public API. The v2 REST surface is the contract, not these signatures.
    #[must_use]
    pub fn new(store: Arc<dyn SimulationStore>, config: SimulationV2Config) -> Self {
        Self {
            store,
            uuid_generator: UuidGenerator::new(default_namespace()),
            config,
            tapes: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(SnapshotCache::with_capacity(config.max_cached_snapshots)),
        }
    }

    /// The operational configuration this manager applies.
    #[must_use]
    pub fn config(&self) -> SimulationV2Config {
        self.config
    }

    /// Creates a simulation from resolved parameters.
    ///
    /// The factor tape is **not** built here. Creation stays cheap and
    /// predictable, and the first peek or advance pays for the tape — which it
    /// would have to be able to rebuild after an eviction anyway.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::AlreadyExists`] on an id collision, or any storage
    /// failure.
    #[instrument(skip(self, parameters), level = "debug")]
    pub(crate) async fn create(
        &self,
        parameters: SimulationParametersV2,
    ) -> Result<SessionV2, ChainError> {
        let simulation = SessionV2::new(parameters, &self.uuid_generator);
        self.store.create(simulation.clone()).await?;

        info!(
            simulation_id = %simulation.id,
            steps = simulation.total_steps,
            seed = simulation.parameters.seed,
            "Created a v2 rolling simulation"
        );
        Ok(simulation)
    }

    /// Reads a simulation's metadata without touching its cursor.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::NotFound`] when no simulation has that id.
    #[instrument(skip(self), level = "debug")]
    pub(crate) async fn get(&self, id: Uuid) -> Result<SessionV2, ChainError> {
        self.store.get(id).await
    }

    /// Builds the snapshot at the current cursor **without** advancing or
    /// persisting anything.
    ///
    /// Safe and repeatable: the same call returns the same snapshot until an
    /// advance moves the cursor. The only side effect is a warmer cache.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::NotFound`] for an unknown id,
    /// [`ChainError::SimulatorError`] when the simulation has already served
    /// every step (410 at the boundary, matching v1's exhausted path),
    /// [`ChainError::InvalidState`] for a simulation in the terminal error
    /// state, and whatever the tape or snapshot build surfaces.
    #[instrument(skip(self), level = "debug")]
    pub(crate) async fn peek(&self, id: Uuid) -> Result<(SessionV2, SeriesSnapshot), ChainError> {
        let simulation = self.store.get(id).await?;
        Self::reject_terminal(&simulation, "no current step")?;

        let snapshot = self.snapshot_at(&simulation, simulation.current_step)?;
        Ok((simulation, snapshot))
    }

    /// Serves the snapshot at the current cursor, then advances it exactly
    /// once.
    ///
    /// The advance that serves the last snapshot marks the simulation
    /// `Completed` and drops its cached state: its tape and snapshots can never
    /// be served again, and a re-created simulation would rebuild them
    /// identically anyway.
    ///
    /// # Errors
    ///
    /// As [`SimulationManager::peek`], plus [`ChainError::Conflict`] when a
    /// concurrent advance committed first — the caller re-reads and retries,
    /// and there is deliberately no silent retry loop here.
    #[instrument(skip(self), level = "debug")]
    pub(crate) async fn advance(
        &self,
        id: Uuid,
    ) -> Result<(SessionV2, SeriesSnapshot), ChainError> {
        let mut simulation = self.store.get(id).await?;

        // The revision read here is what the compare-and-swap below commits
        // against, so two concurrent advances that both read this snapshot
        // cannot both persist.
        let expected_version = simulation.version;
        Self::reject_terminal(&simulation, "no further steps")?;

        let snapshot = self.snapshot_at(&simulation, simulation.current_step)?;

        simulation.current_step = simulation
            .current_step
            .checked_add(1)
            .ok_or_else(|| ChainError::Internal("the cursor overflowed".to_string()))?;
        simulation.state = if simulation.is_complete() {
            SessionState::Completed
        } else {
            SessionState::InProgress
        };

        simulation.bump_version()?;
        self.store
            .save_cas(simulation.clone(), expected_version)
            .await?;

        if simulation.state == SessionState::Completed {
            self.evict(id);
            debug!(simulation_id = %id, "Simulation completed; cached state evicted");
        }

        Ok((simulation, snapshot))
    }

    /// Deletes a simulation and everything cached for it.
    ///
    /// # Errors
    ///
    /// Returns any storage failure. A missing id is `Ok(false)`, not an error.
    #[instrument(skip(self), level = "debug")]
    pub(crate) async fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
        let deleted = self.store.delete(id).await?;
        // Evict regardless: a delete that found nothing may still be cleaning
        // up after a simulation the store expired on its own.
        self.evict(id);
        Ok(deleted)
    }

    /// Expires idle simulations and evicts everything cached for them.
    ///
    /// Returns the ids that went, which is what makes the eviction possible at
    /// all — a count could not tell the caches which entries to drop.
    ///
    /// # Errors
    ///
    /// Returns any storage failure.
    #[instrument(skip(self), level = "debug")]
    pub async fn cleanup(&self) -> Result<Vec<Uuid>, ChainError> {
        let expired = self.store.cleanup().await?;
        for id in &expired {
            self.evict(*id);
        }
        Ok(expired)
    }

    /// The number of factor tapes currently cached.
    #[must_use]
    pub fn cached_tapes(&self) -> usize {
        match self.tapes.lock() {
            Ok(tapes) => tapes.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// The number of snapshots currently cached.
    #[must_use]
    pub fn cached_snapshots(&self) -> usize {
        match self.snapshots.lock() {
            Ok(snapshots) => snapshots.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Rejects a simulation that can no longer serve a snapshot.
    ///
    /// `Completed` maps to `410 Gone` at the boundary, matching v1's exhausted
    /// path; the terminal error state maps to `400`.
    fn reject_terminal(simulation: &SessionV2, what: &str) -> Result<(), ChainError> {
        if simulation.state == SessionState::Completed || simulation.is_complete() {
            return Err(ChainError::SimulatorError(format!(
                "simulation completed; {what}"
            )));
        }
        if simulation.state == SessionState::Error {
            return Err(ChainError::InvalidState(
                "simulation is in error state".to_string(),
            ));
        }
        Ok(())
    }

    /// Returns the snapshot at `step`, building whatever is missing.
    ///
    /// Locks are held only for the map operations, never across a build: the
    /// tape and the snapshot are produced outside any critical section, so a
    /// slow build cannot stall another simulation's request.
    fn snapshot_at(
        &self,
        simulation: &SessionV2,
        step: usize,
    ) -> Result<SeriesSnapshot, ChainError> {
        if let Some(cached) = self.cached_snapshot(simulation.id, step) {
            return Ok(cached);
        }

        let tape = self.tape_for(simulation)?;
        let snapshot = SeriesBuilder::new(&simulation.parameters, &tape)?.snapshot(step)?;

        self.cache_snapshot(simulation.id, snapshot.clone());
        Ok(snapshot)
    }

    /// Reads a cached snapshot, refreshing its recency.
    fn cached_snapshot(&self, id: Uuid, step: usize) -> Option<SeriesSnapshot> {
        let mut snapshots = match self.snapshots.lock() {
            Ok(snapshots) => snapshots,
            Err(poisoned) => poisoned.into_inner(),
        };
        snapshots.get(id, step).cloned()
    }

    /// Stores a built snapshot.
    fn cache_snapshot(&self, id: Uuid, snapshot: SeriesSnapshot) {
        let mut snapshots = match self.snapshots.lock() {
            Ok(snapshots) => snapshots,
            Err(poisoned) => poisoned.into_inner(),
        };
        snapshots.insert(id, snapshot);
    }

    /// Returns the simulation's factor tape, building it on a miss.
    fn tape_for(&self, simulation: &SessionV2) -> Result<FactorTape, ChainError> {
        if let Some(tape) = self.cached_tape(simulation.id) {
            return Ok(tape);
        }

        // Built outside the lock. `FactorTape::build` is pure and synchronous —
        // it is the one place a v2 request does real CPU work up front — and
        // holding the map while it runs would serialise every other simulation
        // behind it.
        let tape = FactorTape::build(&simulation.parameters, &simulation.parameters.method)?;
        self.cache_tape(simulation.id, tape.clone());
        Ok(tape)
    }

    /// Reads a cached tape, refreshing its recency.
    fn cached_tape(&self, id: Uuid) -> Option<FactorTape> {
        let mut tapes = match self.tapes.lock() {
            Ok(tapes) => tapes,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = tapes.get_mut(&id)?;
        entry.last_access = Instant::now();
        Some(entry.tape.clone())
    }

    /// Stores a built tape, evicting the least recently used first.
    fn cache_tape(&self, id: Uuid, tape: FactorTape) {
        let mut tapes = match self.tapes.lock() {
            Ok(tapes) => tapes,
            Err(poisoned) => poisoned.into_inner(),
        };

        tapes.remove(&id);
        // The capacity is validated `>= 1` when the configuration loads, so
        // `- 1` cannot underflow. Evicting before the insert keeps the id being
        // inserted out of the running for victim.
        let max = self.config.max_cached_tapes;
        debug_assert!(
            max >= 1,
            "the configured capacity is validated >= 1 at load"
        );
        while tapes.len() > max - 1 {
            let victim = tapes
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(id, _)| *id);
            match victim {
                Some(victim) => {
                    tapes.remove(&victim);
                }
                None => break,
            }
        }

        tapes.insert(
            id,
            TapeEntry {
                tape,
                last_access: Instant::now(),
            },
        );
    }

    /// Drops everything cached for a simulation.
    fn evict(&self, id: Uuid) {
        match self.tapes.lock() {
            Ok(mut tapes) => {
                tapes.remove(&id);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove(&id);
            }
        }
        match self.snapshots.lock() {
            Ok(mut snapshots) => {
                snapshots.evict_simulation(id);
            }
            Err(poisoned) => {
                poisoned.into_inner().evict_simulation(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
    use crate::api::rest::requests_v2::CreateSimulationRequest;
    use crate::session::store::InMemorySimulationStore;
    use crate::session::{ExpiryRule, ExpiryRuleKind};
    use chrono::{TimeZone, Utc, Weekday};

    fn request(steps: usize) -> CreateSimulationRequest {
        let rules = vec![
            match ExpiryRule::new("zero_dte", ExpiryRuleKind::Daily, 1) {
                Ok(rule) => rule,
                Err(error) => panic!("the test rule must be valid: {error}"),
            },
            match ExpiryRule::new(
                "weeklies",
                ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Fri]),
                2,
            ) {
                Ok(rule) => rule,
                Err(error) => panic!("the test rule must be valid: {error}"),
            },
        ];
        let start_at = match Utc.with_ymd_and_hms(2026, 1, 5, 14, 30, 0).single() {
            Some(instant) => instant,
            None => panic!("the test instant must be valid"),
        };

        CreateSimulationRequest {
            symbol: "SPX".to_string(),
            steps,
            start_at: Some(start_at),
            step_interval_seconds: Some(86_400),
            timezone: "America/New_York".to_string(),
            calendar: None,
            expiration_time: "17:00".to_string(),
            schedules: rules,
            initial_price: 5000.0,
            volatility: 0.18,
            risk_free_rate: 0.04,
            dividend_yield: 0.0,
            method: ApiWalkType::Brownian {
                dt: 1.0 / 252.0,
                drift: 0.0,
                volatility: 0.18,
            },
            time_frame: ApiTimeFrame::Day,
            chain_size: Some(3),
            strike_interval: Some(25.0),
            skew_slope: None,
            smile_curve: None,
            spread: Some(0.02),
            seed: Some(42),
        }
    }

    fn parameters(steps: usize) -> SimulationParametersV2 {
        match SimulationParametersV2::try_from(request(steps)) {
            Ok(parameters) => parameters,
            Err(error) => panic!("the request must convert: {error}"),
        }
    }

    fn manager() -> SimulationManager {
        SimulationManager::new(
            Arc::new(InMemorySimulationStore::new()),
            SimulationV2Config::default(),
        )
    }

    async fn created(manager: &SimulationManager, steps: usize) -> SessionV2 {
        match manager.create(parameters(steps)).await {
            Ok(simulation) => simulation,
            Err(error) => panic!("the simulation must be created: {error}"),
        }
    }

    /// A created simulation starts at cursor zero and is readable back.
    #[tokio::test]
    async fn test_create_then_get_returns_the_simulation() {
        let manager = manager();
        let created = created(&manager, 5).await;

        match manager.get(created.id).await {
            Ok(loaded) => {
                assert_eq!(loaded, created);
                assert_eq!(loaded.current_step, 0);
                assert_eq!(loaded.state, SessionState::Initialized);
            }
            Err(error) => panic!("the simulation must load: {error}"),
        }
    }

    /// Creation does not build the factor tape: it stays cheap and predictable,
    /// and the first peek pays for it.
    #[tokio::test]
    async fn test_creation_does_not_build_the_tape() {
        let manager = manager();
        let created = created(&manager, 5).await;

        assert_eq!(manager.cached_tapes(), 0);

        match manager.peek(created.id).await {
            Ok(_) => assert_eq!(manager.cached_tapes(), 1),
            Err(error) => panic!("the peek must succeed: {error}"),
        }
    }

    /// A peek is repeatable and changes nothing.
    #[tokio::test]
    async fn test_a_peek_is_repeatable_and_changes_nothing() {
        let manager = manager();
        let created = created(&manager, 5).await;

        let first = match manager.peek(created.id).await {
            Ok((_, snapshot)) => snapshot,
            Err(error) => panic!("the peek must succeed: {error}"),
        };
        let second = match manager.peek(created.id).await {
            Ok((_, snapshot)) => snapshot,
            Err(error) => panic!("the peek must succeed: {error}"),
        };

        assert_eq!(first, second);
        match manager.get(created.id).await {
            Ok(loaded) => {
                assert_eq!(loaded.current_step, 0, "a peek must not move the cursor");
                assert_eq!(loaded.version, created.version, "a peek must not persist");
                assert_eq!(loaded.state, SessionState::Initialized);
            }
            Err(error) => panic!("the simulation must load: {error}"),
        }
    }

    /// An advance serves the current snapshot and then moves the cursor.
    #[tokio::test]
    async fn test_an_advance_serves_then_advances() {
        let manager = manager();
        let created = created(&manager, 5).await;

        let peeked = match manager.peek(created.id).await {
            Ok((_, snapshot)) => snapshot,
            Err(error) => panic!("the peek must succeed: {error}"),
        };
        let (advanced, served) = match manager.advance(created.id).await {
            Ok(result) => result,
            Err(error) => panic!("the advance must succeed: {error}"),
        };

        assert_eq!(
            served, peeked,
            "the advance must serve the snapshot the peek showed"
        );
        assert_eq!(advanced.current_step, 1);
        assert_eq!(advanced.state, SessionState::InProgress);
    }

    /// Walking a simulation serves indices 0..N-1 and then completes.
    #[tokio::test]
    async fn test_walking_serves_every_index_once_then_completes() {
        let manager = manager();
        let created = created(&manager, 3).await;

        let mut served = Vec::new();
        for _ in 0..3 {
            match manager.advance(created.id).await {
                Ok((_, snapshot)) => served.push(snapshot.step),
                Err(error) => panic!("the advance must succeed: {error}"),
            }
        }
        assert_eq!(served, vec![0, 1, 2]);

        match manager.get(created.id).await {
            Ok(loaded) => assert_eq!(loaded.state, SessionState::Completed),
            Err(error) => panic!("the simulation must load: {error}"),
        }

        // A completed simulation has nothing left to serve, on either path.
        match manager.advance(created.id).await {
            Err(ChainError::SimulatorError(message)) => assert!(message.contains("completed")),
            other => panic!("expected the exhausted path, got {other:?}"),
        }
        match manager.peek(created.id).await {
            Err(ChainError::SimulatorError(message)) => assert!(message.contains("completed")),
            other => panic!("expected the exhausted path, got {other:?}"),
        }
    }

    /// Completing a simulation drops everything cached for it.
    #[tokio::test]
    async fn test_completion_evicts_the_cached_state() {
        let manager = manager();
        let created = created(&manager, 1).await;

        match manager.advance(created.id).await {
            Ok(_) => {}
            Err(error) => panic!("the advance must succeed: {error}"),
        }

        assert_eq!(manager.cached_tapes(), 0);
        assert_eq!(manager.cached_snapshots(), 0);
    }

    /// Two advances that read the same revision produce one winner.
    #[tokio::test]
    async fn test_a_lost_race_is_a_conflict() {
        let store = Arc::new(InMemorySimulationStore::new());
        let manager = SimulationManager::new(store.clone(), SimulationV2Config::default());
        let created = created(&manager, 5).await;

        // Advance once through the manager, then replay an advance built from
        // the pre-advance revision — exactly what a concurrent caller holds.
        match manager.advance(created.id).await {
            Ok(_) => {}
            Err(error) => panic!("the first advance must succeed: {error}"),
        }

        // The mutation a concurrent caller would hold: it read the simulation
        // before the advance, so it carries the pre-advance revision but an
        // otherwise valid post-advance state.
        let mut stale = created.clone();
        stale.current_step = 1;
        stale.state = SessionState::InProgress;
        let expected = match stale.bump_version() {
            Ok(expected) => expected,
            Err(error) => panic!("must bump: {error}"),
        };
        match store.save_cas(stale, expected).await {
            Err(ChainError::Conflict(_)) => {}
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    /// Deleting removes the simulation and its cached state.
    #[tokio::test]
    async fn test_delete_removes_the_simulation_and_its_caches() {
        let manager = manager();
        let created = created(&manager, 5).await;
        match manager.peek(created.id).await {
            Ok(_) => {}
            Err(error) => panic!("the peek must succeed: {error}"),
        }
        assert_eq!(manager.cached_tapes(), 1);

        match manager.delete(created.id).await {
            Ok(deleted) => assert!(deleted),
            Err(error) => panic!("the delete must succeed: {error}"),
        }

        assert_eq!(manager.cached_tapes(), 0);
        assert_eq!(manager.cached_snapshots(), 0);
        assert!(manager.get(created.id).await.is_err());
    }

    /// Deleting something that is not there is not an error, and still clears
    /// any cache left behind by a store that expired it on its own.
    #[tokio::test]
    async fn test_deleting_a_missing_simulation_is_not_an_error() {
        let manager = manager();

        match manager.delete(Uuid::new_v4()).await {
            Ok(deleted) => assert!(!deleted),
            Err(error) => panic!("a missing delete must not error: {error}"),
        }
    }

    /// An unknown id is not found, on every read path.
    #[tokio::test]
    async fn test_an_unknown_id_is_not_found() {
        let manager = manager();
        let missing = Uuid::new_v4();

        assert!(matches!(
            manager.get(missing).await,
            Err(ChainError::NotFound(_))
        ));
        assert!(matches!(
            manager.peek(missing).await,
            Err(ChainError::NotFound(_))
        ));
        assert!(matches!(
            manager.advance(missing).await,
            Err(ChainError::NotFound(_))
        ));
    }

    /// Cleanup expires idle simulations and evicts what they left cached.
    #[tokio::test]
    async fn test_cleanup_expires_and_evicts() {
        let store = Arc::new(InMemorySimulationStore::with_idle_retention(
            std::time::Duration::from_secs(1),
        ));
        let manager = SimulationManager::new(store, SimulationV2Config::default());
        let created = created(&manager, 5).await;
        match manager.peek(created.id).await {
            Ok(_) => {}
            Err(error) => panic!("the peek must succeed: {error}"),
        }
        assert_eq!(manager.cached_tapes(), 1);

        // Age the stored document past its retention window.
        let mut aged = created.clone();
        aged.updated_at = std::time::SystemTime::now() - std::time::Duration::from_secs(3_600);
        // Save at the same revision it was read at: the point is to age the
        // document, not to change it.
        let expected = aged.version;
        match manager.store.save_cas(aged, expected).await {
            Ok(()) => {}
            Err(error) => panic!("the aged document must save: {error}"),
        }

        match manager.cleanup().await {
            Ok(expired) => assert_eq!(expired, vec![created.id]),
            Err(error) => panic!("the cleanup must succeed: {error}"),
        }
        assert_eq!(manager.cached_tapes(), 0);
        assert_eq!(manager.cached_snapshots(), 0);
    }

    /// A snapshot survives an eviction of its tape, because both rebuild.
    #[tokio::test]
    async fn test_an_evicted_tape_rebuilds_identically() {
        let manager = manager();
        let created = created(&manager, 4).await;

        let before = match manager.peek(created.id).await {
            Ok((_, snapshot)) => snapshot,
            Err(error) => panic!("the peek must succeed: {error}"),
        };

        manager.evict(created.id);
        assert_eq!(manager.cached_tapes(), 0);

        let after = match manager.peek(created.id).await {
            Ok((_, snapshot)) => snapshot,
            Err(error) => panic!("the peek must succeed: {error}"),
        };
        assert_eq!(before, after, "a rebuild must be indistinguishable");
    }
}
