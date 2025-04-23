use chrono::Duration;
use optionchain_simulator::infrastructure::{
    ClickHouseClient, ClickHouseConfig, ClickHouseHistoricalRepository, HistoricalDataRepository,
};
use optionchain_simulator::session::{
    InMemorySessionStore, SessionManager, SimulationMethod, SimulationParameters,
};
use optionchain_simulator::utils::ChainError;
use optionstratlib::utils::others::calculate_log_returns;
use optionstratlib::utils::time::convert_time_frame;
use optionstratlib::utils::{Len, TimeFrame, setup_logger};
use optionstratlib::volatility::{annualized_volatility, constant_volatility};
use optionstratlib::{Positive, pos, spos};
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::{error, info};
use uuid::Uuid;

/// Example demonstrating integration of ClickHouse historical data with option chain simulation
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    setup_logger();
    info!("Starting ClickHouse + Option Chain Simulation Example");

    // Step 1: Set up ClickHouse client
    let config = ClickHouseConfig::default();

    info!("Connecting to ClickHouse at {}", config.host);
    let client = Arc::new(ClickHouseClient::new(config)?);

    // Step 2: Create historical repository
    let repo = Arc::new(ClickHouseHistoricalRepository::new(client.clone()));

    // Step 3: Create session store and manager for option chain simulation
    let store = Arc::new(InMemorySessionStore::new());
    let session_manager = SessionManager::new(store.clone());

    // Step 4: Get historical data for a symbol
    let symbol = "CL"; // Crude oil

    // Using the async trait methods directly
    match repo.get_date_range_for_symbol(symbol).await {
        Ok((min_date, max_date)) => {
            info!(
                "Data available for {} from {} to {}",
                symbol,
                min_date.format("%Y-%m-%d"),
                max_date.format("%Y-%m-%d")
            );

            // Get a sample of historical prices to use as a basis for simulation
            let start_date = max_date - Duration::days(60); // Start from 60 days before max_date
            let limit = 60; // Retrieve up to 60 data points (approximately 60 days of daily data)

            match repo
                .get_historical_prices(symbol, &TimeFrame::Day, &start_date, limit)
                .await
            {
                Ok(historical_prices) => {
                    if historical_prices.is_empty() {
                        return Err("No historical prices retrieved".into());
                    }

                    info!(
                        "Retrieved {} historical price points for {}",
                        historical_prices.len(),
                        symbol
                    );

                    // Calculate historical volatility from the price series
                    let volatility = calculate_historical_volatility(&historical_prices)?;
                    info!("Calculated historical volatility: {}", volatility);

                    // Use the most recent price as the initial price for simulation
                    let initial_price = historical_prices.last().unwrap().clone();
                    info!("Using initial price: {}", initial_price);

                    // Step 5: Create simulation parameters based on historical data
                    let params =
                        create_simulation_parameters(symbol.to_string(), initial_price, volatility);

                    // Step 6: Create a new simulation session
                    info!(
                        "Creating simulation session based on {} historical data",
                        symbol
                    );
                    let session_result = session_manager.create_session(params);

                    match session_result {
                        Ok(session) => {
                            info!(
                                session_id = %session.id,
                                "Session created successfully with initial state: {:?}",
                                session.state
                            );

                            // Step 7: Run through a few simulation steps
                            run_simulation_steps(&session_manager, session.id, 7).await?;
                        }
                        Err(e) => {
                            error!("Failed to create session: {}", e);
                            return Err(e.to_string().into());
                        }
                    }
                }
                Err(e) => {
                    error!("Error fetching historical prices: {}", e);
                    return Err(e.into());
                }
            }
        }
        Err(e) => {
            error!("Error getting date range for {}: {}", symbol, e);
            return Err(e.into());
        }
    }

    info!("Example completed successfully");
    Ok(())
}

/// Creates simulation parameters based on historical data
fn create_simulation_parameters(
    symbol: String,
    initial_price: Positive,
    volatility: Positive,
) -> SimulationParameters {
    // Convert timeframes for simulation
    let time_frame = TimeFrame::Day;
    let dt = convert_time_frame(Positive::ONE, &time_frame, &TimeFrame::Day);

    SimulationParameters {
        symbol,
        steps: 30, // Simulate 30 days into the future
        initial_price,
        days_to_expiration: pos!(30.0), // 30-day options
        volatility,
        risk_free_rate: Decimal::new(3, 2), // 0.03 = 3%
        dividend_yield: Positive::ZERO,
        method: SimulationMethod::GeometricBrownian {
            dt,
            drift: Decimal::ZERO, // No drift
            volatility,
        },
        time_frame,
        chain_size: Some(30),                  // 30 strikes
        strike_interval: Some(pos!(1.0)),      // $1 intervals
        skew_factor: Some(Decimal::new(5, 4)), // 0.0005
        spread: spos!(0.02),                   // 2% bid-ask spread
    }
}

/// Runs multiple simulation steps and displays results
async fn run_simulation_steps(
    session_manager: &SessionManager,
    session_id: Uuid,
    steps: usize,
) -> Result<(), ChainError> {
    for i in 0..steps {
        info!(session_id = %session_id, step = i+1, "Advancing to simulation step");

        let (session, chain) = session_manager.get_next_step(session_id).await?;

        info!(
            session_id = %session_id,
            current_step = session.current_step,
            underlying_price = %chain.underlying_price,
            "Simulated option chain generated successfully"
        );

        // Display some details about the option chain
        let call_count = chain
            .get_single_iter()
            .filter(|c| c.call_middle.is_some())
            .count();
        let put_count = chain
            .get_single_iter()
            .filter(|c| c.put_middle.is_some())
            .count();

        info!(
            "Chain contains {} contracts ({} calls, {} puts)",
            chain.len(),
            call_count,
            put_count
        );

        // Display the first few option contracts
        let display_count = std::cmp::min(5, chain.len());
        if display_count > 0 {
            info!("Sample option contracts:");
            for (i, contract) in chain
                .clone()
                .get_single_iter()
                .take(display_count)
                .enumerate()
            {
                info!("Contract {}: {}", i + 1, contract);
            }
        }
    }

    Ok(())
}

/// Calculate historical volatility from a series of prices
/// This is a simplified calculation for demonstration purposes
fn calculate_historical_volatility(
    prices: &[Positive],
) -> Result<Positive, Box<dyn std::error::Error>> {
    let log_return = calculate_log_returns(prices).unwrap_or(Vec::new());
    let log_return_dec = log_return
        .iter()
        .map(|r| r.to_dec())
        .collect::<Vec<Decimal>>();

    let volatility = constant_volatility(&log_return_dec)?;
    let annualized_volatility = annualized_volatility(volatility, TimeFrame::Day)?.round_to(3);
    Ok(annualized_volatility)
}