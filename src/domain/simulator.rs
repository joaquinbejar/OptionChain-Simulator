use crate::session::{Session, SessionState};
use crate::utils::ChainError;
use optionstratlib::utils::Len;
use optionstratlib::{
    ExpirationDate, Positive,
    chains::{
        OptionChainBuildParams, chain::OptionChain, generator_optionchain,
        utils::OptionDataPriceParams,
    },
    pos,
    simulation::{
        WalkParams, WalkTypeAble,
        randomwalk::RandomWalk,
        steps::{Step, Xstep, Ystep},
    },
};
use rust_decimal::Decimal;
use std::sync::{Arc, Mutex};
use rust_decimal_macros::dec;
use tracing::{debug, error, info, instrument};

const DEFAULT_CHAIN_SIZE: usize = 30;
const DEFAULT_SKEW_FACTOR: Decimal = dec!(0.0005);

/// Simulator handles the generation of option chains based on simulation parameters
pub struct Simulator {
    // Store a cache of random walks for each session ID to avoid recalculating the entire path
    simulation_cache:
        Arc<Mutex<std::collections::HashMap<uuid::Uuid, RandomWalk<Positive, OptionChain>>>>,
}

/// Walker struct for implementing WalkTypeAble
struct Walker {}

impl Walker {
    fn new() -> Self {
        Walker {}
    }
}

impl WalkTypeAble<Positive, OptionChain> for Walker {}

impl Simulator {
    /// Creates a new simulator instance
    pub fn new() -> Self {
        info!("Creating new simulator instance");
        Self {
            simulation_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Simulates the next step based on the session parameters and returns an OptionChain
    #[instrument(skip(self, session), level = "debug")]
    pub fn simulate_next_step(&self, session: &Session) -> Result<OptionChain, ChainError> {
        debug!(
            session_id = %session.id,
            current_step = session.current_step,
            "Simulating next step"
        );

        let mut cache = self
            .simulation_cache
            .lock()
            .map_err(|e| format!("Failed to acquire lock on simulation cache: {}", e))?;

        // Check if we need to create a new RandomWalk or use an existing one
        if !cache.contains_key(&session.id)
            || session.current_step == 0
            || session.state == SessionState::Reinitialized
        {
            info!(
                session_id = %session.id,
                "Creating new simulation for session"
            );

            let random_walk = self.create_random_walk(session)?;
            cache.insert(session.id, random_walk);
        }

        // Get the random walk for this session
        let random_walk = cache
            .get(&session.id)
            .ok_or_else(|| format!("Failed to get random walk for session {}", session.id))?;

        // Get the chain for the current step
        if session.current_step >= random_walk.len() {
            error!("Walker reached end of random walk");
            return Err(ChainError::SimulatorError("Walker reached end of random walk".to_string()));
        }
        let step = random_walk[session.current_step].clone();

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

    /// Creates a new RandomWalk for a session
    #[instrument(skip(self, session), level = "debug")]
    fn create_random_walk(
        &self,
        session: &Session,
    ) -> Result<RandomWalk<Positive, OptionChain>, ChainError> {
        let params = &session.parameters;

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
        let strike_interval = params.strike_interval.unwrap_or(Positive::ONE);
        let skew_factor = params.skew_factor.unwrap_or(DEFAULT_SKEW_FACTOR);
        let spread = params.spread.unwrap_or(pos!(0.01));

        // Create option data price parameters
        let price_params = OptionDataPriceParams::new(
            initial_price,
            ExpirationDate::Days(days_to_expiration),
            Some(volatility),
            risk_free_rate,
            dividend_yield,
            Some(symbol.clone()),
        );

        // Create option chain build parameters
        let build_params = OptionChainBuildParams::new(
            symbol.clone(),
            Some(Positive::ONE), // Default volume
            chain_size,
            strike_interval,
            skew_factor,
            spread,
            2, // Decimal places
            price_params,
        );

        // Build the initial chain
        let initial_chain = OptionChain::build_chain(&build_params);

        // Create walker for a random walk
        let walker = Box::new(Walker::new());

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
            walk_type: params.method.clone(),
            walker,
        };

        // Create the random walk
        let random_walk = RandomWalk::new(
            format!("Session_{}", session.id),
            &walk_params,
            generator_optionchain,
        );

        info!(
            session_id = %session.id,
            steps = random_walk.len(),
            "Created random walk for session"
        );

        Ok(random_walk)
    }

    /// Cleans up the simulation cache by removing entries for sessions that are no longer active
    #[instrument(skip(self), level = "debug")]
    pub fn cleanup_cache(&self, active_session_ids: &[uuid::Uuid]) -> Result<usize, ChainError> {
        let mut cache = self
            .simulation_cache
            .lock()
            .map_err(|e| format!("Failed to acquire lock on simulation cache: {}", e))?;

        let initial_size = cache.len();

        // Create a set of active session IDs for faster lookups
        let active_set: std::collections::HashSet<_> = active_session_ids.iter().collect();

        // Remove entries for sessions that are no longer active
        cache.retain(|id, _| active_set.contains(id));

        let removed_count = initial_size - cache.len();
        debug!("Cleaned up {} entries from simulation cache", removed_count);

        Ok(removed_count)
    }
}
