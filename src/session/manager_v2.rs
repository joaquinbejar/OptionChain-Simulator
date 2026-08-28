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
use crate::infrastructure::{SimulationSnapshotRepository, SimulationV2Config, SnapshotRecord};
use crate::session::model::SessionState;
use crate::session::snapshot_record::{snapshot_quote_count, snapshot_record};
use crate::session::store::SimulationStore;
use crate::session::{SessionV2, SimulationParametersV2};
use crate::utils::ChainError;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

/// One cached factor tape and the last time it was used.
struct TapeEntry {
    tape: FactorTape,
    last_access: Instant,
}

/// Owns the lifecycle of v2 rolling simulations.
pub struct SimulationManager {
    store: Arc<dyn SimulationStore>,
    config: SimulationV2Config,
    tapes: Arc<Mutex<HashMap<Uuid, TapeEntry>>>,
    /// Tape builds currently running, one entry per simulation.
    ///
    /// Without it, N concurrent first reads of one simulation start N identical
    /// builds — and a build is the one place a v2 request does seconds of CPU,
    /// so the duplicates are not a wasted allocation, they are the machine.
    /// `spawn_blocking` does not bound that: its pool grows to hundreds of
    /// threads, so the cache would still be cold while every core was busy
    /// filling it with the same answer.
    builds: Mutex<HashMap<Uuid, broadcast::Sender<Result<FactorTape, String>>>>,
    snapshots: Mutex<SnapshotCache>,
    /// Where served snapshots are queued for filing, when the operator turned
    /// persistence on. `None` is the default and the whole feature is then
    /// absent from the serving path — no connection, no latency, no failure
    /// mode.
    warehouse: Option<Warehouse>,
}

/// The queue in front of the warehouse, and what is currently in it.
struct Warehouse {
    /// The repository itself, so a reader — the export — can consult the same
    /// warehouse the writer fills without being handed a second handle to it.
    repository: Arc<dyn SimulationSnapshotRepository>,
    sender: mpsc::Sender<SnapshotRecord>,
    /// Quote rows queued but not yet written. Incremented before a send and
    /// decremented by the writer once the record leaves the queue, so it
    /// measures what is resident rather than what has been served.
    queued_contracts: Arc<AtomicUsize>,
}

/// How many snapshots may be waiting to be filed.
///
/// The queue is what keeps a degraded warehouse from becoming a memory leak. An
/// unbounded spawn-per-advance cannot delay a response, but at a sustained
/// advance rate against a warehouse that is timing out it accumulates records
/// until the process dies — which fails every request, not just the write.
const SNAPSHOT_QUEUE_DEPTH: usize = 1_024;

/// How many quote rows may be waiting to be filed, across every queued record.
///
/// A depth in *records* is the wrong unit for the same reason an entry count
/// was the wrong unit for the snapshot cache: a record is a few hundred quotes
/// in the reference configuration and up to the per-snapshot cap in a large
/// one, so 1 024 of them is anywhere from a hundred thousand to two hundred
/// million rows. This bounds what is actually resident.
///
/// Like the cache bound, the count is derived from a byte budget of roughly
/// 850 MB rather than chosen: `size_of::<QuoteRow>()` is 620 bytes since issue
/// #74 gave it both greek snapshots (212 bytes before), so 1 350 000 × 620 B ≈
/// 837 MB. The count came down from 4 000 000 in the same change, to hold the
/// budget the previous count expressed.
///
/// Neither bound is a knob. A deployment that needs to tune them is one whose
/// warehouse cannot keep up with its advance rate, and the answer there is the
/// warehouse, not a deeper buffer in front of it.
const SNAPSHOT_QUEUE_CONTRACTS: usize = 1_350_000;

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
            config,
            tapes: Arc::new(Mutex::new(HashMap::new())),
            builds: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(SnapshotCache::with_bounds(
                config.max_cached_snapshots,
                config.max_cached_snapshot_contracts,
            )),
            warehouse: None,
        }
    }

    /// Files every served snapshot in `warehouse`.
    ///
    /// Opt-in, and deliberately a separate constructor rather than an argument
    /// to [`SimulationManager::new`]: a deployment without ClickHouse should not
    /// have to name the feature to not use it, and the serving path should not
    /// branch on a config flag it can express as a missing dependency.
    #[must_use]
    pub fn with_warehouse(mut self, repository: Arc<dyn SimulationSnapshotRepository>) -> Self {
        let (sender, mut receiver) = mpsc::channel::<SnapshotRecord>(SNAPSHOT_QUEUE_DEPTH);
        let queued_contracts = Arc::new(AtomicUsize::new(0));
        let writer_contracts = Arc::clone(&queued_contracts);
        let warehouse = Arc::clone(&repository);

        // One writer, not one task per advance: the queue bounds what a slow
        // warehouse can accumulate, and serialising the writes means two steps
        // of one simulation reach the warehouse in the order they were served.
        tokio::spawn(async move {
            while let Some(record) = receiver.recv().await {
                let simulation = record.simulation;
                let step = record.step;
                let contracts = record.quote_count();

                let result = warehouse.persist(record).await;
                writer_contracts.fetch_sub(contracts, Ordering::SeqCst);

                if let Err(error) = result {
                    warn!(
                        simulation_id = %simulation,
                        step,
                        error = %error,
                        "Could not file the snapshot; the step can be replayed and rewritten"
                    );
                }
            }
        });

        self.warehouse = Some(Warehouse {
            repository,
            sender,
            queued_contracts,
        });
        self
    }

    /// The warehouse this manager files into, if any.
    ///
    /// Exists so the export can prefer persisted snapshots over replay without
    /// the binary threading a second handle through the server: the manager
    /// already owns the one the writer uses, and two handles could drift to two
    /// different configurations.
    #[must_use]
    pub fn warehouse(&self) -> Option<Arc<dyn SimulationSnapshotRepository>> {
        self.warehouse
            .as_ref()
            .map(|warehouse| Arc::clone(&warehouse.repository))
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
        let simulation = SessionV2::new(parameters);
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

        let snapshot = self
            .snapshot_at(&simulation, simulation.current_step)
            .await?;
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

        let snapshot = self
            .snapshot_at(&simulation, simulation.current_step)
            .await?;

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

        // After the commit, never before: a snapshot is only real once the
        // cursor that served it is durable, and persisting first would leave a
        // row for a step a losing writer never served.
        self.file_snapshot(&simulation, &snapshot);

        if simulation.state == SessionState::Completed {
            self.evict(id);
            debug!(simulation_id = %id, "Simulation completed; cached state evicted");
        }

        Ok((simulation, snapshot))
    }

    /// Queues a served snapshot for filing, if a warehouse is configured.
    ///
    /// **Off the request's clock.** A failure cannot fail the advance — the
    /// cursor has already committed and the client already has its snapshot —
    /// and neither can a slow one delay it: the record goes into a bounded
    /// queue that one writer task drains.
    ///
    /// A full queue **drops** the record with a `WARN` naming the step. That is
    /// the same trade as a failed write, made explicit: the step stays
    /// reproducible, replay rebuilds it, and a retry writes the same rows. What
    /// it costs is a gap, and the honest way to find one is to compare a
    /// simulation's cursor against what `read_range` returns — a log line can be
    /// lost with the process, a missing row cannot.
    ///
    /// Filing is idempotent because both tables sort on
    /// `(simulation, generation, step, …)` and their `ReplacingMergeTree` engine
    /// collapses on that sorting key, so a retry of a step that did land
    /// replaces its rows rather than adding a second copy. The derived
    /// `snapshot_id` rides along as a payload column and is verified on read; it
    /// is not what does the replacing.
    ///
    /// The trade this accepts: a snapshot filed after the response means a
    /// client that advances and immediately queries the warehouse may not find
    /// the step yet. Deterministic replay is the read path that is always
    /// current; the warehouse is the one that is durable.
    fn file_snapshot(&self, simulation: &SessionV2, snapshot: &SeriesSnapshot) {
        let Some(warehouse) = &self.warehouse else {
            return;
        };

        // Decide before building anything. Materialising a record clones every
        // quote, so doing it and *then* discovering the queue is full would put
        // the cost of a degraded warehouse back on the advance — which is the
        // one thing this path exists to avoid.
        let incoming = snapshot_quote_count(snapshot);
        let queued = warehouse.queued_contracts.load(Ordering::SeqCst);
        if warehouse.sender.capacity() == 0
            || queued.saturating_add(incoming) > SNAPSHOT_QUEUE_CONTRACTS
        {
            warn!(
                simulation_id = %simulation.id,
                step = snapshot.step,
                queued,
                "The snapshot queue is full; the step was not filed and can be replayed"
            );
            return;
        }

        let record = snapshot_record(simulation.id, &simulation.parameters.symbol, snapshot);
        warehouse
            .queued_contracts
            .fetch_add(incoming, Ordering::SeqCst);

        if let Err(error) = warehouse.sender.try_send(record) {
            // Lost the race with another advance; undo the reservation.
            warehouse
                .queued_contracts
                .fetch_sub(incoming, Ordering::SeqCst);
            warn!(
                simulation_id = %simulation.id,
                step = snapshot.step,
                error = %error,
                "The snapshot queue is full; the step was not filed and can be replayed"
            );
        }
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
    async fn snapshot_at(
        &self,
        simulation: &SessionV2,
        step: usize,
    ) -> Result<SeriesSnapshot, ChainError> {
        if let Some(cached) = self.cached_snapshot(simulation.id, step) {
            return Ok(cached);
        }

        let tape = self.tape_for(simulation).await?;
        // The greek snapshots are built only when something will read them:
        // a registered warehouse files every step, and a filed step has to
        // carry what a replayed one does (issue #74). Without a warehouse the
        // API prices them per request instead, and only when asked.
        let greek_snapshots = self.warehouse.is_some();
        let parameters = simulation.parameters.clone();

        // Off the runtime and under the shared bound, for the same reason
        // `tape_for` is: pricing a snapshot is real synchronous CPU — up to
        // `DEFAULT_MAX_SNAPSHOT_CONTRACTS` priced contracts, and about 1.54x
        // that again once the greek snapshots are on — and a worker holding it
        // stalls every other request that worker owns.
        //
        // The bound is the one the API renderers use, not a second one: they
        // compete for the same cores, and a warehouse-backed deployment prices
        // greeks HERE on every peek and advance, so admitting only the render
        // side would leave the heavier half unbounded.
        let snapshot = crate::utils::admission::admit_blocking(move || {
            SeriesBuilder::new(&parameters, &tape)?
                .with_greek_snapshots(greek_snapshots)
                .snapshot(step)
        })
        .await?;

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
    ///
    /// Built outside the lock — holding the map while it runs would serialise
    /// every other simulation behind it — and off the runtime. `FactorTape::build`
    /// is pure and synchronous, and it is the one place a v2 request does real
    /// CPU work up front: a historical walk estimates a volatility per step,
    /// which at the 10 000-step cap measures over three seconds. Left on a
    /// worker that would stall every other request the worker holds, so it goes
    /// to the blocking pool, exactly as the export path already does with the
    /// same call.
    ///
    /// The result is filed **inside** the blocking task rather than after the
    /// await. A `spawn_blocking` task cannot be cancelled, but awaiting it can:
    /// a client that disconnects or times out mid-build drops this future, and
    /// filing afterwards would throw away a build that ran to completion
    /// anyway. At three seconds a caller retrying under a shorter timeout would
    /// then never warm the cache and would pin a blocking thread on every
    /// attempt.
    async fn tape_for(&self, simulation: &SessionV2) -> Result<FactorTape, ChainError> {
        if let Some(tape) = self.cached_tape(simulation.id) {
            return Ok(tape);
        }

        let id = simulation.id;

        // Either this call owns the build or it waits on the one already
        // running. Decided under the lock, so two callers cannot both decide
        // they are the owner.
        let subscription = {
            let mut builds = match self.builds.lock() {
                Ok(builds) => builds,
                Err(poisoned) => poisoned.into_inner(),
            };

            match builds.get(&id) {
                Some(running) => Some(running.subscribe()),
                None => {
                    let (sender, _) = broadcast::channel(1);
                    builds.insert(id, sender);
                    None
                }
            }
        };

        if let Some(mut waiting) = subscription {
            return match waiting.recv().await {
                Ok(Ok(tape)) => Ok(tape),
                // The owner failed; report what it reported rather than
                // starting a second build that would fail the same way.
                Ok(Err(reason)) => Err(ChainError::Internal(reason)),
                // The owner's task died without publishing. Rare, and the
                // honest answer is to build it here rather than hang.
                Err(_) => self.build_tape(simulation).await,
            };
        }

        let result = self.build_tape(simulation).await;

        // Publish to whoever is waiting and stop being the owner, in that
        // order: a caller that subscribes after the removal misses the
        // broadcast, retries, and finds the tape in the cache.
        let sender = {
            let mut builds = match self.builds.lock() {
                Ok(builds) => builds,
                Err(poisoned) => poisoned.into_inner(),
            };
            builds.remove(&id)
        };
        if let Some(sender) = sender {
            let published = match &result {
                Ok(tape) => Ok(tape.clone()),
                Err(error) => Err(error.to_string()),
            };
            // An error means nobody was waiting, which is the common case.
            let _ = sender.send(published);
        }

        result
    }

    /// Builds a tape off the runtime and files it.
    ///
    /// `FactorTape::build` is pure and synchronous, and it is the one place a
    /// v2 request does real CPU work up front: a historical walk estimates a
    /// volatility per step, which at the 10 000-step cap measures over three
    /// seconds. Left on a worker it would stall every other request that worker
    /// holds, so it goes to the blocking pool, exactly as the export path
    /// already does with the same call.
    ///
    /// The result is filed **inside** the blocking task rather than after the
    /// await. A `spawn_blocking` task cannot be cancelled, but awaiting it can:
    /// a client that disconnects or times out mid-build drops that future, and
    /// filing afterwards would throw away a build that ran to completion
    /// anyway.
    async fn build_tape(&self, simulation: &SessionV2) -> Result<FactorTape, ChainError> {
        let parameters = simulation.parameters.clone();
        let id = simulation.id;
        let tapes = Arc::clone(&self.tapes);
        let max_cached_tapes = self.config.max_cached_tapes;

        tokio::task::spawn_blocking(move || {
            let tape = FactorTape::build(&parameters, &parameters.method)?;
            Self::cache_tape(&tapes, max_cached_tapes, id, tape.clone());
            Ok(tape)
        })
        .await
        .map_err(|e| ChainError::Internal(format!("the factor tape build did not finish: {e}")))?
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
    ///
    /// Takes the map rather than `&self` so the builder can file its result
    /// from inside the blocking task, where no caller can drop it.
    ///
    /// One race that follows from filing there, recorded because it is benign
    /// only under the current routes: a build already running when the
    /// simulation is deleted, completed or reaped will file afterwards, leaving
    /// a tape for an id the store no longer knows. Nothing can serve it — every
    /// path reads the store before the cache — so it costs memory until the LRU
    /// pushes it out. It would stop being benign the day v2 gains a route that
    /// changes a simulation's parameters in place, because the stale tape would
    /// then be a tape of the *old* parameters under a live id.
    fn cache_tape(
        tapes: &Mutex<HashMap<Uuid, TapeEntry>>,
        max_cached_tapes: usize,
        id: Uuid,
        tape: FactorTape,
    ) {
        let mut tapes = match tapes.lock() {
            Ok(tapes) => tapes,
            Err(poisoned) => poisoned.into_inner(),
        };

        tapes.remove(&id);
        // The capacity is validated `>= 1` when the configuration loads, so
        // `- 1` cannot underflow. Evicting before the insert keeps the id being
        // inserted out of the running for victim.
        let max = max_cached_tapes;
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
    use crate::infrastructure::{ContractQuote, ContractSeriesQuery, SnapshotRecord};
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

    /// A warehouse that records what it was asked to file, and can be told to
    /// fail — the two behaviours the wiring promises something about.
    #[derive(Default)]
    struct RecordingWarehouse {
        filed: Mutex<Vec<SnapshotRecord>>,
        fail: bool,
    }

    impl RecordingWarehouse {
        fn failing() -> Self {
            Self {
                filed: Mutex::new(Vec::new()),
                fail: true,
            }
        }

        /// The records it was handed, whole — so a test can assert on what is
        /// inside one, not just that one arrived.
        fn records(&self) -> Vec<SnapshotRecord> {
            match self.filed.lock() {
                Ok(filed) => filed.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }

        fn filed(&self) -> Vec<(Uuid, usize)> {
            self.records()
                .iter()
                .map(|record| (record.simulation, record.step))
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl SimulationSnapshotRepository for RecordingWarehouse {
        async fn persist(&self, record: SnapshotRecord) -> Result<(), ChainError> {
            if self.fail {
                return Err(ChainError::Internal("the warehouse is down".to_string()));
            }
            match self.filed.lock() {
                Ok(mut filed) => filed.push(record),
                Err(poisoned) => poisoned.into_inner().push(record),
            }
            Ok(())
        }

        async fn get(
            &self,
            _simulation: Uuid,
            _generation: u64,
            _step: usize,
        ) -> Result<Option<SnapshotRecord>, ChainError> {
            Ok(None)
        }

        async fn read_range(
            &self,
            _simulation: Uuid,
            _generation: u64,
            _from_step: usize,
            _to_step: usize,
        ) -> Result<Vec<SnapshotRecord>, ChainError> {
            Ok(Vec::new())
        }

        async fn contract_series(
            &self,
            _query: ContractSeriesQuery,
        ) -> Result<Vec<ContractQuote>, ChainError> {
            Ok(Vec::new())
        }
    }

    /// A warehouse whose first write never completes — the shape a degraded
    /// deployment has, and the one an unbounded queue cannot survive.
    #[derive(Default)]
    struct StallingWarehouse {
        started: AtomicUsize,
    }

    impl StallingWarehouse {
        fn started(&self) -> usize {
            self.started.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl SimulationSnapshotRepository for StallingWarehouse {
        async fn persist(&self, _record: SnapshotRecord) -> Result<(), ChainError> {
            self.started.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
            Ok(())
        }

        async fn get(
            &self,
            _simulation: Uuid,
            _generation: u64,
            _step: usize,
        ) -> Result<Option<SnapshotRecord>, ChainError> {
            Ok(None)
        }

        async fn read_range(
            &self,
            _simulation: Uuid,
            _generation: u64,
            _from_step: usize,
            _to_step: usize,
        ) -> Result<Vec<SnapshotRecord>, ChainError> {
            Ok(Vec::new())
        }

        async fn contract_series(
            &self,
            _query: ContractSeriesQuery,
        ) -> Result<Vec<ContractQuote>, ChainError> {
            Ok(Vec::new())
        }
    }

    /// Filing is detached, so a test has to let the spawned write run before it
    /// can observe it. One yield is enough on the current-thread runtime the
    /// tests use; the loop keeps it from being a race on a busier one.
    async fn settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    /// The simulation id is not an input to anything seeded.
    ///
    /// This is what the switch to random ids rests on. Two simulations built
    /// from one set of parameters have different ids and must still produce the
    /// same snapshots, strike for strike — `SeriesSnapshot`'s equality compares
    /// premiums, Greeks and the underlying price, not lengths. If the id ever
    /// leaked into the tape, the planner or the chain build, this fails.
    #[tokio::test]
    async fn test_the_simulation_id_does_not_reach_the_tape() {
        let manager = manager();

        let first = created(&manager, 3).await;
        let second = created(&manager, 3).await;
        assert_ne!(first.id, second.id, "ids are random, so two differ");
        assert_eq!(
            first.parameters.seed, second.parameters.seed,
            "the fixture must pin the seed, or this proves nothing"
        );

        for _ in 0..3 {
            let left = match manager.advance(first.id).await {
                Ok((_, snapshot)) => snapshot,
                Err(error) => panic!("the first simulation must advance: {error}"),
            };
            let right = match manager.advance(second.id).await {
                Ok((_, snapshot)) => snapshot,
                Err(error) => panic!("the second simulation must advance: {error}"),
            };

            assert_eq!(
                left, right,
                "step {} differs between two simulations that share every parameter",
                left.step
            );
        }
    }

    /// Concurrent first reads of one simulation share a single build.
    ///
    /// The build is the one place a v2 request does seconds of CPU, so N
    /// concurrent peeks starting N identical builds is not wasted allocation,
    /// it is the machine. What proves the sharing is the snapshots: every
    /// caller gets the same tape, and only one entry is cached.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_first_reads_share_one_build() {
        let manager = Arc::new(manager());
        let simulation = created(&manager, 3).await;

        let mut readers = Vec::new();
        for _ in 0..8 {
            let manager = Arc::clone(&manager);
            let id = simulation.id;
            readers.push(tokio::spawn(async move { manager.peek(id).await }));
        }

        let mut snapshots = Vec::new();
        for reader in readers {
            match reader.await {
                Ok(Ok((_, snapshot))) => snapshots.push(snapshot),
                Ok(Err(error)) => panic!("every reader must be served: {error}"),
                Err(error) => panic!("a reader panicked: {error}"),
            }
        }

        assert_eq!(snapshots.len(), 8);
        for snapshot in &snapshots {
            assert_eq!(
                snapshot, &snapshots[0],
                "every reader must see the same tape"
            );
        }
        assert_eq!(
            manager.cached_tapes(),
            1,
            "eight readers of one simulation must leave one tape"
        );
    }

    /// An advance files exactly the step it served.
    #[tokio::test]
    async fn test_an_advance_files_the_step_it_served() {
        let warehouse = Arc::new(RecordingWarehouse::default());
        let manager = SimulationManager::new(
            Arc::new(InMemorySimulationStore::new()),
            SimulationV2Config::default(),
        )
        .with_warehouse(Arc::clone(&warehouse) as Arc<dyn SimulationSnapshotRepository>);

        let simulation = created(&manager, 3).await;
        match manager.advance(simulation.id).await {
            Ok(_) => {}
            Err(error) => panic!("the advance must serve: {error}"),
        }
        settle().await;

        assert_eq!(
            warehouse.filed(),
            vec![(simulation.id, 0)],
            "the step the advance served is the step that is filed"
        );
    }

    /// A registered warehouse is what turns the greek snapshots on.
    ///
    /// The wiring that makes issue #74 work: `SeriesBuilder` builds them only
    /// when asked, and the manager asks exactly when there is a warehouse to
    /// file them into. A filed record with empty snapshots would persist a tape
    /// strictly poorer than a replayed one — the asymmetry the issue removes —
    /// and every existing test would still pass, because none of them looks
    /// inside a filed record.
    #[tokio::test]
    async fn test_a_registered_warehouse_files_the_greeks() {
        let warehouse = Arc::new(RecordingWarehouse::default());
        let manager = SimulationManager::new(
            Arc::new(InMemorySimulationStore::new()),
            SimulationV2Config::default(),
        )
        .with_warehouse(Arc::clone(&warehouse) as Arc<dyn SimulationSnapshotRepository>);

        let simulation = created(&manager, 3).await;
        match manager.advance(simulation.id).await {
            Ok(_) => {}
            Err(error) => panic!("the advance must serve: {error}"),
        }
        settle().await;

        let records = warehouse.records();
        let record = match records.first() {
            Some(record) => record,
            None => panic!("the advance must file a record"),
        };
        let quotes: Vec<_> = record
            .expirations
            .iter()
            .flat_map(|expiration| expiration.quotes.iter())
            .collect();
        assert!(!quotes.is_empty(), "the filed record must carry quotes");
        assert!(
            quotes
                .iter()
                .all(|quote| quote.greeks_call.is_some() && quote.greeks_put.is_some()),
            "every filed quote must carry both snapshots"
        );
    }

    /// A registered warehouse does not change the tape.
    ///
    /// The flag it turns on selects a different upstream branch inside
    /// `OptionChain::build_chain`, so two deployments of the same build, same
    /// parameters and same seed take two different code paths to the same
    /// market. If they ever disagreed, whether a step was filed would change
    /// what a client was served — the worst regression this service can have,
    /// and one no other test would see, because every other comparison is
    /// between two managers configured the same way.
    #[tokio::test]
    async fn test_a_warehouse_does_not_change_the_served_market() {
        let filing = SimulationManager::new(
            Arc::new(InMemorySimulationStore::new()),
            SimulationV2Config::default(),
        )
        .with_warehouse(
            Arc::new(RecordingWarehouse::default()) as Arc<dyn SimulationSnapshotRepository>
        );
        let plain = SimulationManager::new(
            Arc::new(InMemorySimulationStore::new()),
            SimulationV2Config::default(),
        );

        let served = async |manager: &SimulationManager| {
            let simulation = created(manager, 3).await;
            match manager.peek(simulation.id).await {
                Ok((_, snapshot)) => snapshot,
                Err(error) => panic!("the peek must serve: {error}"),
            }
        };
        let with_warehouse = served(&filing).await;
        let without = served(&plain).await;

        assert_eq!(
            with_warehouse.spot, without.spot,
            "the seeded price path must not depend on the warehouse"
        );
        assert_eq!(with_warehouse.base_volatility, without.base_volatility);
        assert_eq!(with_warehouse.chains.len(), without.chains.len());

        // Every priced value, strike by strike. `PartialEq` on `ExpiryChain`
        // compares the whole chain including the greek snapshots, which DO
        // differ by design, so the comparison is over what is served.
        //
        // Counted, not just zipped: a `zip` over two chains of different
        // lengths truncates silently, and over two EMPTY ones it asserts
        // nothing at all — which is the outcome at the low volatilities where a
        // chain serves no valid strike.
        let mut compared = 0_usize;
        for (filed, replayed) in with_warehouse.chains.iter().zip(without.chains.iter()) {
            assert_eq!(filed.expires_at, replayed.expires_at);
            assert_eq!(filed.days_to_expiration, replayed.days_to_expiration);
            assert_eq!(
                filed.chain.iter().count(),
                replayed.chain.iter().count(),
                "the two deployments must quote the same strikes"
            );
            for (left, right) in filed.chain.iter().zip(replayed.chain.iter()) {
                compared += 1;
                assert_eq!(left.strike_price, right.strike_price);
                assert_eq!(left.implied_volatility, right.implied_volatility);
                assert_eq!(left.call_bid, right.call_bid);
                assert_eq!(left.call_ask, right.call_ask);
                assert_eq!(left.call_middle, right.call_middle);
                assert_eq!(left.put_bid, right.put_bid);
                assert_eq!(left.put_ask, right.put_ask);
                assert_eq!(left.put_middle, right.put_middle);
                assert_eq!(left.delta_call, right.delta_call);
                assert_eq!(left.delta_put, right.delta_put);
                assert_eq!(left.gamma, right.gamma);
            }
        }
        assert!(compared > 0, "the fixture must actually quote something");

        // And the one thing that IS meant to differ.
        assert!(
            with_warehouse
                .chains
                .iter()
                .flat_map(|chain| chain.chain.iter())
                .all(|data| data.greeks_call.is_some())
        );
        assert!(
            without
                .chains
                .iter()
                .flat_map(|chain| chain.chain.iter())
                .all(|data| data.greeks_call.is_none())
        );
    }

    /// Without a warehouse nothing pays for the greeks.
    ///
    /// The other half of the same wiring: a deployment that files nothing and
    /// whose clients never ask must not be charged about 1.5x a chain build on
    /// every advance. When a client does ask, the API prices them per request
    /// instead.
    #[tokio::test]
    async fn test_a_manager_without_a_warehouse_does_not_price_the_greeks() {
        let manager = SimulationManager::new(
            Arc::new(InMemorySimulationStore::new()),
            SimulationV2Config::default(),
        );

        let simulation = created(&manager, 3).await;
        let (_, snapshot) = match manager.peek(simulation.id).await {
            Ok(served) => served,
            Err(error) => panic!("the peek must serve: {error}"),
        };

        let contracts: Vec<_> = snapshot
            .chains
            .iter()
            .flat_map(|chain| chain.chain.iter())
            .collect();
        assert!(!contracts.is_empty(), "the snapshot must quote something");
        assert!(
            contracts
                .iter()
                .all(|data| data.greeks_call.is_none() && data.greeks_put.is_none()),
            "no snapshot should have been priced"
        );
    }

    /// A warehouse that never drains stops receiving, rather than accumulating
    /// records until the process dies.
    ///
    /// The bound that matters is rows, not records: a record is a few hundred
    /// quotes in this fixture and up to the per-snapshot cap in a large
    /// configuration, so a depth in records says nothing about what is
    /// resident.
    #[tokio::test]
    async fn test_a_stalled_warehouse_stops_being_queued() {
        let warehouse = Arc::new(StallingWarehouse::default());
        let manager = SimulationManager::new(
            Arc::new(InMemorySimulationStore::new()),
            SimulationV2Config::default(),
        )
        .with_warehouse(Arc::clone(&warehouse) as Arc<dyn SimulationSnapshotRepository>);

        // More advances than the queue can hold, against a warehouse whose
        // first write never returns.
        for _ in 0..(SNAPSHOT_QUEUE_DEPTH + 8) {
            let simulation = created(&manager, 2).await;
            match manager.advance(simulation.id).await {
                Ok(_) => {}
                Err(error) => panic!("the advance must serve regardless: {error}"),
            }
        }
        settle().await;

        assert!(
            warehouse.started() <= SNAPSHOT_QUEUE_DEPTH + 1,
            "a stalled warehouse must stop receiving, got {} starts",
            warehouse.started()
        );
    }

    /// A warehouse that is down does not fail the advance. This is the whole
    /// point of filing after the commit and off the request's clock.
    #[tokio::test]
    async fn test_a_failing_warehouse_does_not_fail_the_advance() {
        let warehouse = Arc::new(RecordingWarehouse::failing());
        let manager = SimulationManager::new(
            Arc::new(InMemorySimulationStore::new()),
            SimulationV2Config::default(),
        )
        .with_warehouse(warehouse as Arc<dyn SimulationSnapshotRepository>);

        let simulation = created(&manager, 3).await;

        match manager.advance(simulation.id).await {
            Ok((advanced, _)) => assert_eq!(advanced.current_step, 1, "the cursor still moved"),
            Err(error) => panic!("a warehouse failure must not fail the advance: {error}"),
        }
        settle().await;
    }

    /// A peek serves a snapshot and files nothing: it moves no cursor, so there
    /// is no step to file.
    #[tokio::test]
    async fn test_a_peek_files_nothing() {
        let warehouse = Arc::new(RecordingWarehouse::default());
        let manager = SimulationManager::new(
            Arc::new(InMemorySimulationStore::new()),
            SimulationV2Config::default(),
        )
        .with_warehouse(Arc::clone(&warehouse) as Arc<dyn SimulationSnapshotRepository>);

        let simulation = created(&manager, 3).await;
        match manager.peek(simulation.id).await {
            Ok(_) => {}
            Err(error) => panic!("the peek must serve: {error}"),
        }
        settle().await;

        assert!(warehouse.filed().is_empty(), "a peek persists nothing");
    }

    /// Without a warehouse the serving path is unchanged — there is nothing to
    /// call and nothing to fail.
    #[tokio::test]
    async fn test_a_manager_without_a_warehouse_serves_normally() {
        let manager = manager();
        let simulation = created(&manager, 2).await;

        match manager.advance(simulation.id).await {
            Ok((advanced, snapshot)) => {
                assert_eq!(advanced.current_step, 1);
                assert_eq!(snapshot.step, 0);
            }
            Err(error) => panic!("the advance must serve: {error}"),
        }
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

    /// The tape cache still honours the configured capacity now that the build
    /// files its own result from inside the blocking task and the cap travels
    /// as a parameter rather than through `&self`.
    #[tokio::test]
    async fn test_the_tape_cache_still_honours_its_capacity() {
        let config = SimulationV2Config {
            max_cached_tapes: 2,
            ..SimulationV2Config::default()
        };
        let manager = SimulationManager::new(Arc::new(InMemorySimulationStore::new()), config);

        for _ in 0..4 {
            let created = created(&manager, 5).await;
            if let Err(error) = manager.peek(created.id).await {
                panic!("the peek must succeed: {error}");
            }
        }

        assert_eq!(
            manager.cached_tapes(),
            2,
            "four tapes were built under a cap of two"
        );
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
