use crate::domain::Walker;
use crate::infrastructure::{
    ClickHouseClient, ClickHouseConfig, ClickHouseHistoricalRepository, HistoricalDataRepository,
    calculate_required_duration, select_random_date,
};
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
        WalkParams,
        randomwalk::RandomWalk,
        steps::{Step, Xstep, Ystep},
    },
};
use rand::Rng;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

const DEFAULT_CHAIN_SIZE: usize = 30;

const DEFAULT_SKEW_SLOPE: Decimal = dec!(-0.2);
const DEFAULT_SMILE_CURVE: Decimal = dec!(0.4);

/// Simulator handles the generation of option chains based on simulation parameters
pub struct Simulator {
    // Cambia el tipo de Mutex
    simulation_cache: Arc<Mutex<HashMap<Uuid, RandomWalk<Positive, OptionChain>>>>,
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

        // First check if we need to create a new random walk
        let need_new_walk;
        {
            let cache = self.simulation_cache.lock().await;
            need_new_walk = !cache.contains_key(&session.id)
                || session.current_step == 0
                || session.state == SessionState::Reinitialized;

            // If the session is reinitialized, remove it from cache
            if session.state == SessionState::Reinitialized && cache.contains_key(&session.id) {
                // We need to drop and re-acquire as a mutable reference
                drop(cache);
                let mut cache = self.simulation_cache.lock().await;
                cache.remove(&session.id);
            }
        }

        // Create a new random walk if needed
        if need_new_walk {
            info!(
                session_id = %session.id,
                "Creating new simulation for session"
            );
            debug!("Reset Random Walk with Session: {}", session);

            // Create the random walk (asynchronous operation)
            let random_walk = self.create_random_walk(session).await?;

            // Insert the new random walk into the cache
            let mut cache = self.simulation_cache.lock().await;
            cache.insert(session.id, random_walk);
        }

        // Get the current step data
        let step = {
            let cache = self.simulation_cache.lock().await;

            let random_walk = cache.get(&session.id).ok_or_else(|| {
                ChainError::Internal(format!(
                    "Failed to get random walk for session {}",
                    session.id
                ))
            })?;

            // Check if the current step is within range
            if session.current_step >= random_walk.len() {
                warn!("Walker reached end of data.");
                return Err(ChainError::SimulatorError(
                    "Walker reached end of data".to_string(),
                ));
            }

            // Clone the step data so we can release the lock
            random_walk[session.current_step].clone()
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
            skew_slope,
            smile_curve,
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
    pub async fn cleanup_cache(&self, active_session_ids: &[Uuid]) -> Result<usize, ChainError> {
        let mut cache = self.simulation_cache.lock().await;

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
    use optionstratlib::{Positive, pos};
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
            initial_price: pos!(100.0),
            days_to_expiration: pos!(30.0),
            volatility: pos!(0.2),
            risk_free_rate: dec!(0.0),
            dividend_yield: pos!(0.0),
            method: SimulationMethod::GeometricBrownian {
                dt: pos!(0.004),
                drift: dec!(0.0),
                volatility: pos!(0.2),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(10),
            strike_interval: Some(pos!(5.0)),
            skew_slope: Some(dec!(-0.2)),
            smile_curve: Some(dec!(0.5)),
            spread: Some(pos!(0.01)),
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

    // Helper function to create test historical data
    fn create_test_historical_data(count: usize) -> Vec<Positive> {
        let mut data = Vec::with_capacity(count);
        for i in 0..count {
            data.push(pos!(100.0 + i as f64));
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
