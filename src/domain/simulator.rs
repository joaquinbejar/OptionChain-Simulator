use crate::domain::Walker;
use crate::infrastructure::{
    ClickHouseClient, ClickHouseConfig, ClickHouseHistoricalRepository, HistoricalDataRepository,
    calculate_required_duration, select_random_date,
};
use crate::session::{Session, SessionState, SimulationMethod};
use crate::utils::ChainError;
use optionstratlib::utils::{Len, TimeFrame};
use optionstratlib::{
    ExpirationDate,
    chains::{
        OptionChainBuildParams, chain::OptionChain, generator_optionchain,
        utils::OptionDataPriceParams,
    },
    simulation::{
        WalkParams,
        randomwalk::RandomWalk,
        steps::{Step, Xstep, Ystep},
    },
};
use positive::{Positive, pos_or_panic};
use rand::RngExt;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

const DEFAULT_CHAIN_SIZE: usize = 30;

const DEFAULT_SKEW_SLOPE: Decimal = dec!(-0.2);
const DEFAULT_SMILE_CURVE: Decimal = dec!(0.4);

/// Default upper bound on the number of random walks held in the simulation cache.
const DEFAULT_MAX_CACHED_WALKS: usize = 1000;

/// Hard bound on the number of random walks the simulation cache may hold
/// (`OCS_MAX_CACHED_WALKS`).
///
/// Read once via [`LazyLock`]; an unset or invalid value (not an integer `>= 1`)
/// falls back to [`DEFAULT_MAX_CACHED_WALKS`] and emits a `tracing::warn!`, so a
/// misconfiguration never aborts startup. The bound is enforced with
/// least-recently-accessed eviction (see [`enforce_capacity`]). This mirrors the
/// parse-once pattern in `api::rest::limits` but lives in the domain layer to keep
/// the dependency flow api -> session -> domain intact.
static MAX_CACHED_WALKS: LazyLock<usize> =
    LazyLock::new(|| match std::env::var("OCS_MAX_CACHED_WALKS").ok() {
        None => DEFAULT_MAX_CACHED_WALKS,
        Some(value) => match value.trim().parse::<usize>() {
            Ok(parsed) if parsed >= 1 => parsed,
            _ => {
                warn!(
                    raw = %value,
                    default = DEFAULT_MAX_CACHED_WALKS,
                    "invalid OCS_MAX_CACHED_WALKS; falling back to default"
                );
                DEFAULT_MAX_CACHED_WALKS
            }
        },
    });

/// One cached random walk together with the last time it was accessed.
///
/// `last_access` drives least-recently-accessed eviction: every cache hit refreshes
/// it so active sessions survive the [`MAX_CACHED_WALKS`] bound while idle ones age out.
///
/// `evictable` gates whether the entry may be chosen as an LRU victim. It is set at
/// insert time: `false` for a `SimulationMethod::Historical` walk, `true` otherwise.
/// A historical walk is pinned because, at this point in the stack, its symbol/date
/// selection still draws from an unseeded `rand::rng()` — evicting it mid-session
/// would rebuild a DIFFERENT tape. This is a temporary conservatism: the stacked
/// seeded-historical PR makes historical selection reproducible and lifts the pin so
/// every walk becomes evictable again.
struct CacheEntry {
    walk: RandomWalk<Positive, OptionChain>,
    last_access: Instant,
    evictable: bool,
}

/// Evicts least-recently-accessed **evictable** entries until `cache` holds at most
/// `max` entries.
///
/// Pure over the cache map (no I/O, no locking) so it can be unit-tested directly.
/// The insert path calls it with `max = MAX_CACHED_WALKS - 1` BEFORE inserting the
/// new entry, so the id being inserted is absent and can never be the victim, and a
/// cache of only evictable entries never exceeds `MAX_CACHED_WALKS` after the insert.
/// An `O(n)` scan per eviction is acceptable at these cache sizes.
///
/// Only entries with `evictable == true` are considered as victims. If the cache is
/// over `max` but every remaining entry is non-evictable (active historical walks,
/// see [`CacheEntry`]), eviction is skipped and the cache is allowed to exceed the
/// bound — never evict a historical walk mid-session, because at this stack level its
/// rebuild would draw a different tape. A `tracing::warn!` names the over-capacity
/// count. This conservatism is temporary: the stacked seeded-historical PR makes all
/// walks evictable and this filter becomes a no-op.
fn enforce_capacity(cache: &mut HashMap<Uuid, CacheEntry>, max: usize) {
    while cache.len() > max {
        let victim = cache
            .iter()
            .filter(|(_, entry)| entry.evictable)
            .min_by_key(|(_, entry)| entry.last_access)
            .map(|(id, _)| *id);
        match victim {
            Some(id) => {
                cache.remove(&id);
            }
            None => {
                warn!(
                    cache_len = cache.len(),
                    max,
                    "cache over capacity but all entries are non-evictable \
                     (active historical walks); skipping eviction"
                );
                break;
            }
        }
    }
}

/// Simulator handles the generation of option chains based on simulation parameters.
///
/// It owns a bounded, per-session cache of [`RandomWalk`]s keyed by session id. The
/// cache is evicted on three lifecycle triggers so it never outlives the sessions
/// it serves (issue #9):
/// - DELETE / completion, via [`Simulator::remove_session`] driven by the session
///   manager;
/// - a `Reinitialized` session, which drops and rebuilds its walk in
///   [`Simulator::simulate_next_step`];
/// - the [`MAX_CACHED_WALKS`] LRU bound, enforced by [`enforce_capacity`] on insert.
///
/// Eviction never affects reproducibility: a re-simulate after eviction rebuilds the
/// walk from the same seed, yielding the identical tape.
pub struct Simulator {
    simulation_cache: Arc<Mutex<HashMap<Uuid, CacheEntry>>>,
    database_repo: Option<Arc<dyn HistoricalDataRepository>>,
}

impl Simulator {
    /// Creates a new simulator instance
    pub fn new() -> Self {
        info!("Creating new simulator instance");
        let database_config = ClickHouseConfig::default();
        info!("Connecting to ClickHouse at {}", database_config.host);
        let database_repo = match ClickHouseClient::new(database_config) {
            Ok(client) => {
                let client = Arc::new(client);
                let repo: Arc<dyn HistoricalDataRepository> =
                    Arc::new(ClickHouseHistoricalRepository::new(client));
                Some(repo)
            }
            Err(e) => {
                error!("Failed to connect to ClickHouse: {}", e);
                None
            }
        };

        Self {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo,
        }
    }

    /// Simulates the next step based on the session parameters and returns an OptionChain
    #[instrument(skip(self, session), level = "debug")]
    pub async fn simulate_next_step(&self, session: &Session) -> Result<OptionChain, ChainError> {
        debug!(
            session_id = %session.id,
            current_step = session.current_step,
            "Simulating next step"
        );

        // First check if we need to create a new random walk.
        //
        // Under serve-then-advance the cursor no longer needs a `current_step == 0`
        // trigger: a fresh session is simply not cached yet, so `!contains_key` builds
        // it once. Dropping the `== 0` trigger keeps peek(cursor 0) and the next advance
        // serving the SAME cached walk (they would otherwise rebuild — and, for an
        // unseeded walker, diverge — on every step-0 access).
        //
        // The lock here is held only for these cheap map ops, never across the walk
        // build below. A Reinitialized session's stale walk is evicted so the next
        // build rebuilds it from the (possibly new) seed; `remove` on an absent id is
        // a no-op.
        let need_new_walk;
        {
            let mut cache = self.simulation_cache.lock().await;
            need_new_walk =
                !cache.contains_key(&session.id) || session.state == SessionState::Reinitialized;

            if session.state == SessionState::Reinitialized {
                cache.remove(&session.id);
            }
        }

        // Build the walk OUTSIDE any lock (this awaits ClickHouse). We keep it in an
        // Option so the single critical section below can insert it. Reproducibility is
        // preserved: a seeded rebuild reproduces the identical tape.
        let random_walk_opt = if need_new_walk {
            info!(
                session_id = %session.id,
                "Creating new simulation for session"
            );
            debug!("Reset Random Walk with Session: {}", session);
            Some(self.create_random_walk(session).await?)
        } else {
            None
        };

        // ONE critical section for the cache: on a fresh build, enforce capacity,
        // insert, then range-check and clone the step; on a hit, refresh recency,
        // range-check and clone — all under a single lock. Collapsing insert and step
        // lookup into one lock closes the window in which a concurrent cold insertion
        // could evict this entry between the two, which would otherwise surface as a
        // spurious `Internal` error.
        let step = {
            let mut cache = self.simulation_cache.lock().await;

            let entry = if let Some(random_walk) = random_walk_opt {
                // Non-historical walks may be LRU-evicted; a Historical walk must not
                // at this stack level, because its symbol/date selection still uses an
                // unseeded RNG (see `CacheEntry`) — evicting mid-session would rebuild a
                // DIFFERENT tape. The stacked seeded-historical PR lifts this pin.
                let evictable = !matches!(
                    session.parameters.method,
                    SimulationMethod::Historical { .. }
                );

                // Evicting down to `max - 1` before inserting keeps a cache of evictable
                // entries at or below `MAX_CACHED_WALKS` afterwards. `MAX_CACHED_WALKS`
                // is validated `>= 1` at parse time (see its definition), so `max - 1`
                // cannot underflow — no saturating arithmetic (rules forbid it).
                let max = *MAX_CACHED_WALKS;
                debug_assert!(max >= 1, "MAX_CACHED_WALKS is validated >= 1 at parse time");
                enforce_capacity(&mut cache, max - 1);

                cache.entry(session.id).or_insert(CacheEntry {
                    walk: random_walk,
                    last_access: Instant::now(),
                    evictable,
                })
            } else {
                cache.get_mut(&session.id).ok_or_else(|| {
                    ChainError::Internal(format!(
                        "Failed to get random walk for session {}",
                        session.id
                    ))
                })?
            };

            // Refresh recency so an actively served session survives the LRU bound
            // while idle sessions age out.
            entry.last_access = Instant::now();

            // Check if the current step is within range
            if session.current_step >= entry.walk.len() {
                warn!("Walker reached end of data.");
                return Err(ChainError::SimulatorError(
                    "Walker reached end of data".to_string(),
                ));
            }

            // Clone the step data so we can release the lock
            entry.walk[session.current_step].clone()
        };

        // Process the chain data outside the lock
        let chain = step.y.value().clone();

        debug!(
            session_id = %session.id,
            current_step = session.current_step,
            underlying_price = %chain.underlying_price,
            contracts_count = chain.len(),
            "Retrieved option chain for step"
        );

        Ok(chain)
    }

    /// Fetches historical data for a given symbol and timeframe with random date range
    /// If symbol is None, selects a random symbol from available symbols
    #[instrument(skip(self), level = "debug")]
    pub async fn get_historical_data(
        &self,
        symbol: &Option<String>,
        timeframe: &TimeFrame,
        steps: usize,
    ) -> Result<Vec<Positive>, ChainError> {
        if let Some(repo) = &self.database_repo {
            let mut thread_rng = rand::rng();

            let actual_symbol = if let Some(sym) = symbol {
                // Use provided symbol
                sym.clone()
            } else {
                // Get list of available symbols and choose one randomly
                let available_symbols = repo
                    .list_available_symbols()
                    .await
                    .map_err(|e| ChainError::ClickHouseError(e.to_string()))?;

                if available_symbols.is_empty() {
                    return Err(ChainError::NotFound(
                        "No symbols available in the database".to_string(),
                    ));
                }

                let random_index = thread_rng.random_range(0..available_symbols.len());
                available_symbols[random_index].clone()
            };

            debug!("Selected symbol: {}", actual_symbol);

            // Get the available date range for the selected symbol
            let (min_date, max_date) = repo
                .get_date_range_for_symbol(&actual_symbol)
                .await
                .map_err(|e| ChainError::ClickHouseError(e.to_string()))?;
            debug!("Available date range: {} - {}", min_date, max_date);

            // Select random start date ensuring enough data for all steps
            let start_date =
                select_random_date(&mut thread_rng, min_date, max_date, timeframe, steps)?;

            // Calculate end date based on required duration
            let duration = calculate_required_duration(timeframe, steps);
            let end_date = start_date + duration;

            debug!(
                "Fetching data from {} to {} for symbol {}",
                start_date, end_date, actual_symbol
            );

            // Fetch the historical prices
            let prices = repo
                .get_historical_prices(&actual_symbol, timeframe, &start_date, steps)
                .await
                .map_err(|e| ChainError::ClickHouseError(e.to_string()))?;

            // Ensure we have enough data points
            if prices.len() < steps {
                return Err(ChainError::NotEnoughData(format!(
                    "Retrieved {} data points but {} required for symbol {}",
                    prices.len(),
                    steps,
                    actual_symbol
                )));
            }

            // Return exactly the number of steps requested
            Ok(prices.into_iter().take(steps).collect())
        } else {
            Err(ChainError::SimulatorError(
                "Database not available".to_string(),
            ))
        }
    }

    /// Creates a new RandomWalk for a session
    #[instrument(skip(self, session), level = "debug")]
    async fn create_random_walk(
        &self,
        session: &Session,
    ) -> Result<RandomWalk<Positive, OptionChain>, ChainError> {
        let params = &session.parameters;
        let method: SimulationMethod = match &params.method {
            SimulationMethod::Historical {
                timeframe,
                prices,
                symbol,
            } => {
                if prices.is_empty() || prices.len() < params.steps {
                    // load historical prices from database
                    let prices = self
                        .get_historical_data(symbol, timeframe, params.steps)
                        .await?;
                    SimulationMethod::Historical {
                        timeframe: *timeframe,
                        prices,
                        symbol: symbol.clone(),
                    }
                } else {
                    params.method.clone()
                }
            }
            _ => params.method.clone(),
        };

        // Extract parameters from session
        let initial_price = params.initial_price;
        let days_to_expiration = params.days_to_expiration;
        let volatility = params.volatility;
        let risk_free_rate = params.risk_free_rate;
        let dividend_yield = params.dividend_yield;
        let symbol = params.symbol.clone();
        let time_frame = params.time_frame;

        // Set default values if not provided
        let chain_size = params.chain_size.unwrap_or(DEFAULT_CHAIN_SIZE);
        let strike_interval = params.strike_interval;
        let skew_slope = params.skew_slope.unwrap_or(DEFAULT_SKEW_SLOPE);
        let smile_curve = params.smile_curve.unwrap_or(DEFAULT_SMILE_CURVE);
        let spread = params.spread.unwrap_or(pos_or_panic!(0.01));

        // Create option data price parameters
        let price_params = OptionDataPriceParams::new(
            Some(Box::new(initial_price)),
            Some(ExpirationDate::Days(days_to_expiration)),
            Some(risk_free_rate),
            Some(dividend_yield),
            Some(symbol.clone()),
        );

        // Create option chain build parameters
        let build_params = OptionChainBuildParams::new(
            symbol.clone(),
            Some(Positive::ONE), // Default volume
            chain_size,
            strike_interval,
            skew_slope,
            smile_curve,
            spread,
            2, // Decimal places
            price_params,
            volatility,
        );

        // Build the initial chain
        let initial_chain = OptionChain::build_chain(&build_params)
            .map_err(|e| ChainError::Internal(format!("Failed to build option chain: {}", e)))?;

        // Create walker for a random walk, seeded when the session requests
        // reproducibility so the same seed always yields the same walk
        let walker = Box::new(match params.seed {
            Some(seed) => Walker::new_with_seed(seed),
            None => Walker::new(),
        });

        // Create step parameters for a random walk
        let walk_params = WalkParams {
            size: params.steps,
            init_step: Step {
                x: Xstep::new(
                    Positive::ONE,
                    time_frame,
                    ExpirationDate::Days(days_to_expiration),
                ),
                y: Ystep::new(0, initial_chain),
            },
            walk_type: method,
            walker,
        };

        // Create the random walk
        let random_walk = RandomWalk::new(
            format!("Session_{}", session.id),
            &walk_params,
            generator_optionchain,
        )
        .map_err(|e| ChainError::Internal(format!("Failed to create random walk: {}", e)))?;

        info!(
            session_id = %session.id,
            steps = random_walk.len(),
            "Created random walk for session"
        );

        Ok(random_walk)
    }

    /// Removes a session's cached random walk, returning whether one was present.
    ///
    /// Driven by the session lifecycle: the manager calls this on DELETE and when an
    /// advance transitions the session to `Completed`, so a deleted or finished
    /// session does not retain its walk (issue #9). Removing an id that is not cached
    /// is a cheap no-op returning `false`.
    ///
    /// Eviction never affects reproducibility: a later re-simulate of a seeded session
    /// rebuilds the identical walk from the same seed.
    #[instrument(skip(self), level = "debug")]
    pub async fn remove_session(&self, id: &Uuid) -> bool {
        let mut cache = self.simulation_cache.lock().await;
        let removed = cache.remove(id).is_some();
        if removed {
            debug!(session_id = %id, "Evicted cached random walk");
        }
        removed
    }

    /// Returns the number of random walks currently held in the simulation cache.
    ///
    /// Read-only; used by the API layer (via the session manager) to publish the
    /// `simulation_cache_size` gauge after operations that grow or shrink the cache.
    pub async fn cache_len(&self) -> usize {
        self.simulation_cache.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SimulationMethod, SimulationParameters};
    use crate::utils::UuidGenerator;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use mockall::predicate::*;
    use mockall::*;
    use optionstratlib::utils::TimeFrame;
    use positive::{Positive, pos_or_panic};
    use rust_decimal_macros::dec;
    use std::sync::Arc;
    use uuid::Uuid;

    // Mock for HistoricalDataRepository
    mock! {
        pub HistoricalRepository {}

        #[async_trait]
        impl HistoricalDataRepository for HistoricalRepository {
            async fn get_historical_prices(
                &self,
                symbol: &str,
                timeframe: &TimeFrame,
                start_date: &DateTime<Utc>,
                limit: usize,
            ) -> Result<Vec<Positive>, ChainError>;

            async fn list_available_symbols(&self) -> Result<Vec<String>, ChainError>;

            async fn get_date_range_for_symbol(
                &self,
                symbol: &str,
            ) -> Result<(DateTime<Utc>, DateTime<Utc>), ChainError>;
        }
    }

    // Helper function to create a test session
    fn create_test_session(id: Option<Uuid>) -> Session {
        let params = SimulationParameters {
            symbol: "TEST".to_string(),
            steps: 10,
            initial_price: pos_or_panic!(100.0),
            days_to_expiration: pos_or_panic!(30.0),
            volatility: pos_or_panic!(0.2),
            risk_free_rate: dec!(0.0),
            dividend_yield: pos_or_panic!(0.0),
            method: SimulationMethod::GeometricBrownian {
                dt: pos_or_panic!(0.004),
                drift: dec!(0.0),
                volatility: pos_or_panic!(0.2),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(10),
            strike_interval: Some(pos_or_panic!(5.0)),
            skew_slope: Some(dec!(-0.2)),
            smile_curve: Some(dec!(0.5)),
            spread: Some(pos_or_panic!(0.01)),
            seed: None,
        };

        let namespace = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
        let uuid_generator = UuidGenerator::new(namespace);

        let mut session = Session::new(params, &uuid_generator);
        // Override the generated ID with the provided one if it exists
        if let Some(id) = id {
            session.id = id;
        }
        session
    }

    // Helper that walks a session through every step and returns the
    // underlying price observed at each snapshot
    async fn collect_tape(simulator: &Simulator, session: &mut Session) -> Vec<Positive> {
        let steps = session.parameters.steps;
        let mut tape = Vec::with_capacity(steps);
        for step in 0..steps {
            session.current_step = step;
            if step > 0 {
                session.state = SessionState::InProgress;
            }
            let chain = simulator
                .simulate_next_step(session)
                .await
                .expect("Simulation step failed");
            tape.push(chain.underlying_price);
        }
        tape
    }

    #[tokio::test]
    async fn test_same_seed_produces_identical_tape() {
        // Complete-tape test: two sessions with identical parameters and the
        // same seed must produce the same sequence of snapshots. Distinct ids keep
        // them as independent cache entries (as real sessions always are).
        let mut session_a = create_test_session(Some(Uuid::new_v4()));
        let mut session_b = create_test_session(Some(Uuid::new_v4()));
        session_a.parameters.seed = Some(20260713);
        session_b.parameters.seed = Some(20260713);

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        let tape_a = collect_tape(&simulator, &mut session_a).await;
        let tape_b = collect_tape(&simulator, &mut session_b).await;

        assert_eq!(tape_a, tape_b);
    }

    #[tokio::test]
    async fn test_different_seeds_produce_different_tapes() {
        // Distinct ids keep the two walks in independent cache entries so the seeds,
        // not a shared cache slot, drive the difference.
        let mut session_a = create_test_session(Some(Uuid::new_v4()));
        let mut session_b = create_test_session(Some(Uuid::new_v4()));
        session_a.parameters.seed = Some(1);
        session_b.parameters.seed = Some(2);

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        let tape_a = collect_tape(&simulator, &mut session_a).await;
        let tape_b = collect_tape(&simulator, &mut session_b).await;

        assert_ne!(tape_a, tape_b);
    }

    // JumpDiffusion method with lambda_dt = intensity * dt = 1.0 * 0.004 < 1,
    // so the Bernoulli jump approximation is valid (issue #11).
    fn jump_diffusion_method() -> SimulationMethod {
        SimulationMethod::JumpDiffusion {
            dt: pos_or_panic!(0.004),
            drift: dec!(0.0),
            volatility: pos_or_panic!(0.2),
            intensity: pos_or_panic!(1.0),
            jump_mean: dec!(0.0),
            jump_volatility: pos_or_panic!(0.1),
        }
    }

    #[tokio::test]
    async fn test_jump_diffusion_same_seed_same_tape() {
        // Issue #11: the corrected Bernoulli jump draw stays deterministic —
        // two JumpDiffusion sessions with the same seed produce the identical
        // tape through the full Simulator/generator path. Distinct ids keep
        // them as independent cache entries.
        let mut session_a = create_test_session(Some(Uuid::new_v4()));
        let mut session_b = create_test_session(Some(Uuid::new_v4()));
        session_a.parameters.method = jump_diffusion_method();
        session_b.parameters.method = jump_diffusion_method();
        session_a.parameters.seed = Some(20260713);
        session_b.parameters.seed = Some(20260713);

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        let tape_a = collect_tape(&simulator, &mut session_a).await;
        let tape_b = collect_tape(&simulator, &mut session_b).await;

        assert_eq!(tape_a, tape_b);
    }

    #[tokio::test]
    async fn test_jump_diffusion_different_seeds_different_tapes() {
        // Distinct seeds must drive distinct JumpDiffusion tapes.
        let mut session_a = create_test_session(Some(Uuid::new_v4()));
        let mut session_b = create_test_session(Some(Uuid::new_v4()));
        session_a.parameters.method = jump_diffusion_method();
        session_b.parameters.method = jump_diffusion_method();
        session_a.parameters.seed = Some(1);
        session_b.parameters.seed = Some(2);

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        let tape_a = collect_tape(&simulator, &mut session_a).await;
        let tape_b = collect_tape(&simulator, &mut session_b).await;

        assert_ne!(tape_a, tape_b);
    }

    #[tokio::test]
    async fn test_remove_session_evicts_cached_walk() {
        // Issue #9: remove_session drops the cached walk and reports presence.
        let session = create_test_session(Some(Uuid::new_v4()));
        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        // Populate the cache via a simulate, then evict.
        simulator
            .simulate_next_step(&session)
            .await
            .expect("initial simulate failed");
        assert_eq!(simulator.cache_len().await, 1);

        assert!(simulator.remove_session(&session.id).await);
        assert_eq!(simulator.cache_len().await, 0);

        // Removing again is a no-op reporting absence.
        assert!(!simulator.remove_session(&session.id).await);
    }

    #[tokio::test]
    async fn test_eviction_preserves_seeded_tape() {
        // Issue #9 reproducibility guard: evicting a seeded session's walk and
        // rebuilding it from the same seed yields the identical snapshot.
        let mut session = create_test_session(Some(Uuid::new_v4()));
        session.parameters.seed = Some(20260713);
        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        let before = simulator
            .simulate_next_step(&session)
            .await
            .expect("first simulate failed");
        assert_eq!(simulator.cache_len().await, 1);

        // Evict, then rebuild from the same seed on the next simulate.
        assert!(simulator.remove_session(&session.id).await);
        assert_eq!(simulator.cache_len().await, 0);

        let after = simulator
            .simulate_next_step(&session)
            .await
            .expect("rebuild simulate failed");
        assert_eq!(simulator.cache_len().await, 1);

        // The rebuilt walk reproduces the pre-eviction snapshot exactly.
        assert_eq!(before.underlying_price, after.underlying_price);
    }

    #[tokio::test]
    async fn test_enforce_capacity_evicts_least_recently_accessed() {
        // Bound logic in isolation: with three staggered entries and max 2, the
        // least-recently-accessed entry is the one evicted.
        use std::time::Duration;

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };
        let mut small = create_test_session(None);
        small.parameters.steps = 2;

        let id_old = Uuid::new_v4();
        let id_mid = Uuid::new_v4();
        let id_new = Uuid::new_v4();

        let now = Instant::now();
        let mut cache: HashMap<Uuid, CacheEntry> = HashMap::new();
        cache.insert(
            id_old,
            CacheEntry {
                walk: simulator.create_random_walk(&small).await.unwrap(),
                last_access: now - Duration::from_secs(3),
                evictable: true,
            },
        );
        cache.insert(
            id_mid,
            CacheEntry {
                walk: simulator.create_random_walk(&small).await.unwrap(),
                last_access: now - Duration::from_secs(2),
                evictable: true,
            },
        );
        cache.insert(
            id_new,
            CacheEntry {
                walk: simulator.create_random_walk(&small).await.unwrap(),
                last_access: now - Duration::from_secs(1),
                evictable: true,
            },
        );

        enforce_capacity(&mut cache, 2);

        assert_eq!(cache.len(), 2);
        assert!(
            !cache.contains_key(&id_old),
            "least-recently-accessed entry must be evicted"
        );
        assert!(cache.contains_key(&id_mid));
        assert!(cache.contains_key(&id_new));
    }

    #[tokio::test]
    async fn test_enforce_capacity_noop_when_within_bound() {
        // Below/at the bound, enforce_capacity evicts nothing.
        use std::time::Duration;

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };
        let mut small = create_test_session(None);
        small.parameters.steps = 2;

        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let now = Instant::now();
        let mut cache: HashMap<Uuid, CacheEntry> = HashMap::new();
        cache.insert(
            id_a,
            CacheEntry {
                walk: simulator.create_random_walk(&small).await.unwrap(),
                last_access: now - Duration::from_secs(2),
                evictable: true,
            },
        );
        cache.insert(
            id_b,
            CacheEntry {
                walk: simulator.create_random_walk(&small).await.unwrap(),
                last_access: now - Duration::from_secs(1),
                evictable: true,
            },
        );

        enforce_capacity(&mut cache, 5);
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test]
    async fn test_enforce_capacity_skips_non_evictable_and_evicts_oldest_non_historical() {
        // Fix 1: a non-evictable (historical) entry is never chosen as the victim,
        // even when it is the least-recently-accessed. With three staggered entries
        // and max 2, the oldest is historical (pinned) so the victim is the oldest
        // NON-historical entry instead. The `evictable` flag alone drives this, so the
        // entries are built from a non-historical walk and their flags set directly.
        use std::time::Duration;

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };
        let mut small = create_test_session(None);
        small.parameters.steps = 2;

        let id_hist_old = Uuid::new_v4();
        let id_nonhist_mid = Uuid::new_v4();
        let id_nonhist_new = Uuid::new_v4();

        let now = Instant::now();
        let mut cache: HashMap<Uuid, CacheEntry> = HashMap::new();
        cache.insert(
            id_hist_old,
            CacheEntry {
                walk: simulator.create_random_walk(&small).await.unwrap(),
                last_access: now - Duration::from_secs(3),
                evictable: false,
            },
        );
        cache.insert(
            id_nonhist_mid,
            CacheEntry {
                walk: simulator.create_random_walk(&small).await.unwrap(),
                last_access: now - Duration::from_secs(2),
                evictable: true,
            },
        );
        cache.insert(
            id_nonhist_new,
            CacheEntry {
                walk: simulator.create_random_walk(&small).await.unwrap(),
                last_access: now - Duration::from_secs(1),
                evictable: true,
            },
        );

        enforce_capacity(&mut cache, 2);

        assert_eq!(cache.len(), 2);
        assert!(
            cache.contains_key(&id_hist_old),
            "non-evictable historical entry must survive even as least-recently-accessed"
        );
        assert!(
            !cache.contains_key(&id_nonhist_mid),
            "oldest non-historical entry must be evicted"
        );
        assert!(cache.contains_key(&id_nonhist_new));
    }

    #[tokio::test]
    async fn test_enforce_capacity_skips_eviction_when_all_non_evictable() {
        // Fix 1: when every entry is non-evictable (all active historical walks),
        // eviction is skipped and the cache is left OVER the bound (with a warn),
        // rather than evicting a historical walk mid-session.
        use std::time::Duration;

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };
        let mut small = create_test_session(None);
        small.parameters.steps = 2;

        let now = Instant::now();
        let mut cache: HashMap<Uuid, CacheEntry> = HashMap::new();
        for secs in 1..=3 {
            cache.insert(
                Uuid::new_v4(),
                CacheEntry {
                    walk: simulator.create_random_walk(&small).await.unwrap(),
                    last_access: now - Duration::from_secs(secs),
                    evictable: false,
                },
            );
        }

        enforce_capacity(&mut cache, 2);

        // Nothing evictable: the cache stays at 3, exceeding the bound of 2.
        assert_eq!(cache.len(), 3);
    }

    // Helper function to create test historical data
    fn create_test_historical_data(count: usize) -> Vec<Positive> {
        let mut data = Vec::with_capacity(count);
        for i in 0..count {
            data.push(pos_or_panic!(100.0 + i as f64));
        }
        data
    }

    #[tokio::test]
    async fn test_new_simulator_without_db() {
        // Test that a simulator can be created without a database
        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        assert!(simulator.database_repo.is_none());
        assert_eq!(simulator.simulation_cache.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn test_new_simulator_with_mock_db() {
        // Test simulator creation with a mock database
        let mut mock_repo = MockHistoricalRepository::new();
        mock_repo
            .expect_list_available_symbols()
            .returning(|| Ok(vec!["TEST".to_string()]));

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: Some(Arc::new(mock_repo)),
        };

        assert!(simulator.database_repo.is_some());
        let symbols = simulator
            .database_repo
            .as_ref()
            .unwrap()
            .list_available_symbols()
            .await
            .unwrap();
        assert_eq!(symbols, vec!["TEST".to_string()]);
    }

    #[tokio::test]
    async fn test_simulate_next_step_new_session() {
        // Test simulating the next step for a brand new session
        let session = create_test_session(None);
        let session_id = session.id;

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        // The first call should create a new random walk
        let result = simulator.simulate_next_step(&session).await;
        assert!(result.is_ok());

        // Check that the session was added to the cache
        let cache = simulator.simulation_cache.lock().await;
        assert!(cache.contains_key(&session_id));
    }

    #[tokio::test]
    async fn test_simulate_next_step_existing_session() {
        // Test simulating the next step for an existing session
        let mut session = create_test_session(None);
        let session_id = session.id;

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        // First call to initialize
        let _ = simulator.simulate_next_step(&session).await.unwrap();

        // Update session for next step
        session.current_step = 1;
        session.state = SessionState::InProgress;

        // Second call should use the cached random walk
        let result = simulator.simulate_next_step(&session).await;
        assert!(result.is_ok());

        // Check that there's still only one entry in the cache
        let cache = simulator.simulation_cache.lock().await;
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key(&session_id));
    }

    #[tokio::test]
    async fn test_simulate_next_step_reinitialized_session() {
        // Test simulating with a reinitialized session
        let mut session = create_test_session(None);
        let session_id = session.id;

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        // First call to initialize
        let _ = simulator.simulate_next_step(&session).await.unwrap();

        // Update session to reinitialized state
        session.state = SessionState::Reinitialized;

        // Next call should create a new random walk
        let result = simulator.simulate_next_step(&session).await;
        assert!(result.is_ok());

        // Check that there's still only one entry in the cache (the old one was replaced)
        let cache = simulator.simulation_cache.lock().await;
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key(&session_id));
    }

    #[tokio::test]
    async fn test_simulate_next_step_out_of_range() {
        // Test simulating a step that's out of range
        let mut session = create_test_session(None);

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        // First call to initialize
        let _ = simulator.simulate_next_step(&session).await.unwrap();

        // Update session to a step beyond the total
        session.current_step = session.parameters.steps + 1;

        // This should return an error
        let result = simulator.simulate_next_step(&session).await;
        assert!(result.is_err());

        match result {
            Err(ChainError::SimulatorError(msg)) => {
                assert_eq!(msg, "Walker reached end of data");
            }
            _ => panic!("Expected SimulatorError"),
        }
    }

    #[tokio::test]
    async fn test_get_historical_data_with_symbol() {
        // Test getting historical data with a specified symbol
        let symbol = Some("TEST".to_string());
        let timeframe = TimeFrame::Day;
        let steps = 5;
        let expected_data = create_test_historical_data(steps);

        let mut mock_repo = MockHistoricalRepository::new();
        mock_repo
            .expect_get_date_range_for_symbol()
            .with(eq("TEST"))
            .returning(|_| Ok((Utc::now() - chrono::Duration::days(30), Utc::now())));

        mock_repo
            .expect_get_historical_prices()
            .returning(move |_, _, _, _| Ok(expected_data.clone()));

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: Some(Arc::new(mock_repo)),
        };

        let result = simulator
            .get_historical_data(&symbol, &timeframe, steps)
            .await;
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.len(), steps);
    }

    #[tokio::test]
    async fn test_get_historical_data_without_symbol() {
        // Test getting historical data with no symbol specified (random selection)
        let symbol = None;
        let timeframe = TimeFrame::Day;
        let steps = 5;
        let expected_data = create_test_historical_data(steps);

        let mut mock_repo = MockHistoricalRepository::new();
        mock_repo
            .expect_list_available_symbols()
            .returning(|| Ok(vec!["RANDOM1".to_string(), "RANDOM2".to_string()]));

        mock_repo
            .expect_get_date_range_for_symbol()
            .returning(|_| Ok((Utc::now() - chrono::Duration::days(30), Utc::now())));

        mock_repo
            .expect_get_historical_prices()
            .returning(move |_, _, _, _| Ok(expected_data.clone()));

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: Some(Arc::new(mock_repo)),
        };

        let result = simulator
            .get_historical_data(&symbol, &timeframe, steps)
            .await;
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.len(), steps);
    }

    #[tokio::test]
    async fn test_get_historical_data_no_db() {
        // Test getting historical data when no database is available
        let symbol = Some("TEST".to_string());
        let timeframe = TimeFrame::Day;
        let steps = 5;

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        let result = simulator
            .get_historical_data(&symbol, &timeframe, steps)
            .await;
        assert!(result.is_err());

        match result {
            Err(ChainError::SimulatorError(msg)) => {
                assert_eq!(msg, "Database not available");
            }
            _ => panic!("Expected SimulatorError"),
        }
    }

    #[tokio::test]
    async fn test_get_historical_data_not_enough_data() {
        // Test getting historical data when not enough data is available
        let symbol = Some("TEST".to_string());
        let timeframe = TimeFrame::Day;
        let steps = 10;
        let expected_data = create_test_historical_data(5); // Not enough data

        let mut mock_repo = MockHistoricalRepository::new();
        mock_repo
            .expect_get_date_range_for_symbol()
            .returning(|_| Ok((Utc::now() - chrono::Duration::days(30), Utc::now())));

        mock_repo
            .expect_get_historical_prices()
            .returning(move |_, _, _, _| Ok(expected_data.clone()));

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: Some(Arc::new(mock_repo)),
        };

        let result = simulator
            .get_historical_data(&symbol, &timeframe, steps)
            .await;
        assert!(result.is_err());

        match result {
            Err(ChainError::NotEnoughData(_)) => {
                // Expected error
            }
            _ => panic!("Expected NotEnoughData error"),
        }
    }

    #[tokio::test]
    async fn test_create_random_walk() {
        // Test creating a random walk for a session
        let session = create_test_session(None);

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: None,
        };

        let result = simulator.create_random_walk(&session).await;
        assert!(result.is_ok());

        let random_walk = result.unwrap();
        assert_eq!(random_walk.len(), session.parameters.steps);
    }

    #[tokio::test]
    async fn test_create_random_walk_historical() {
        // Test creating a random walk with historical method
        let mut session = create_test_session(None);
        let steps = 5;
        session.parameters.steps = steps;
        session.parameters.method = SimulationMethod::Historical {
            timeframe: TimeFrame::Day,
            prices: vec![], // Empty prices to trigger database fetch
            symbol: Some("TEST".to_string()),
        };

        let expected_data = create_test_historical_data(steps);

        let mut mock_repo = MockHistoricalRepository::new();
        mock_repo
            .expect_get_date_range_for_symbol()
            .returning(|_| Ok((Utc::now() - chrono::Duration::days(30), Utc::now())));

        mock_repo
            .expect_get_historical_prices()
            .returning(move |_, _, _, _| Ok(expected_data.clone()));

        let simulator = Simulator {
            simulation_cache: Arc::new(Mutex::new(HashMap::new())),
            database_repo: Some(Arc::new(mock_repo)),
        };

        let result = simulator.create_random_walk(&session).await;
        assert!(result.is_ok());

        let random_walk = result.unwrap();
        assert_eq!(random_walk.len(), steps);
    }
}
