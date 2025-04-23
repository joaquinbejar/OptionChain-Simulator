use crate::session::{Session, SessionState, SimulationMethod};
use crate::utils::ChainError;
use optionstratlib::utils::{Len, TimeFrame};
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
use rust_decimal_macros::dec;
use std::sync::{Arc, Mutex};
use rand::Rng;
use tracing::{debug, error, info, instrument, warn};
use crate::infrastructure::{calculate_required_duration, select_random_date, ClickHouseClient, ClickHouseConfig, ClickHouseHistoricalRepository, HistoricalDataRepository};


const DEFAULT_CHAIN_SIZE: usize = 30;
const DEFAULT_SKEW_FACTOR: Decimal = dec!(0.0005);

/// Simulator handles the generation of option chains based on simulation parameters
pub struct Simulator {
    // Store a cache of random walks for each session ID to avoid recalculating the entire path
    simulation_cache:
        Arc<Mutex<std::collections::HashMap<uuid::Uuid, RandomWalk<Positive, OptionChain>>>>,
    database_repo: Option<Arc<dyn HistoricalDataRepository>>,
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
        let database_config = ClickHouseConfig::default();
        info!("Connecting to ClickHouse at {}", database_config.host);
        let database_repo = match ClickHouseClient::new(database_config) {
            Ok(client) => {
                let client = Arc::new(client);
                let repo: Arc<dyn HistoricalDataRepository> = Arc::new(ClickHouseHistoricalRepository::new(client));
                Some(repo)
            },
            Err(e) => {
                error!("Failed to connect to ClickHouse: {}", e);
                None
            }       
        };

        Self {
            simulation_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
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

        let mut cache = self
            .simulation_cache
            .lock()
            .map_err(|e| format!("Failed to acquire lock on simulation cache: {}", e))?;

        // Remove the entry for the session if it is in the cache and the session is reinitialized
        // force call create_random_walk to recalculate the random walk with the new parameters
        if session.state == SessionState::Reinitialized && cache.contains_key(&session.id) {
            cache.remove(&session.id);
        }

        // Check if we need to create a new RandomWalk or use an existing one
        if !cache.contains_key(&session.id)
            || session.current_step == 0
            || session.state == SessionState::Reinitialized
        {
            info!(
                session_id = %session.id,
                "Creating new simulation for session"
            );
            debug!("Reset Random Walk with Session: {}", session);
            // let random_walk = self.create_random_walk(session);

            
            let random_walk = self.create_random_walk(session).await?;
            cache.insert(session.id, random_walk);
        }

        // Get the random walk for this session
        let random_walk = cache
            .get(&session.id)
            .ok_or_else(|| format!("Failed to get random walk for session {}", session.id))?;

        // Get the chain for the current step
        if session.current_step >= random_walk.len() {
            warn!("Walker reached end of data");
            return Err(ChainError::SimulatorError(
                "Walker reached end of data".to_string(),
            ));
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
            let start_date = select_random_date(&mut thread_rng, min_date, max_date, timeframe, steps)?;

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
                    prices.len(), steps, actual_symbol
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
            SimulationMethod::Historical {timeframe, prices, symbol} => {
                if prices.is_empty() || prices.len() < params.steps {
                    // load historical prices from database
                    let prices = self.get_historical_data(symbol, timeframe, params.steps).await?;
                    SimulationMethod::Historical {
                        timeframe: timeframe.clone(),
                        prices,
                        symbol: symbol.clone()
                    }
                } else {
                    params.method.clone()
                }
            },
            _ => {
                params.method.clone()
            }
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
            walk_type: method,
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
