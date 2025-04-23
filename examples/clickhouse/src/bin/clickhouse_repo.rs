use chrono::{DateTime, Duration, Utc};
use optionchain_simulator::infrastructure::{
    ClickHouseClient, ClickHouseConfig, ClickHouseHistoricalRepository, HistoricalDataRepository,
};
use optionstratlib::Positive;
use optionstratlib::utils::{TimeFrame, setup_logger};
use std::sync::Arc;
use std::time::Instant;
use tracing::{error, info};

// This example focuses on the HistoricalDataRepository trait implementation
// with the ClickHouseHistoricalRepository, showing clean async usage patterns
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Setup tracing for better logging
    setup_logger();
    info!("Starting ClickHouse Repository Example");

    // Create a repository that implements HistoricalDataRepository
    let repo = create_repository()?;

    // Example 1: Get all available symbols
    info!("=== Example 1: List All Available Symbols ===");
    list_all_symbols(&repo).await?;

    // Example 2: Get date range for a specific symbol
    let symbol = "CL"; // Change this to a symbol available in your database
    info!("=== Example 2: Get Date Range for Symbol '{}' ===", symbol);
    let (_start_date, end_date) = get_symbol_date_range(&repo, symbol).await?;

    // Example 3: Get historical prices with different timeframes
    info!("=== Example 3: Get Historical Prices with Different Timeframes ===");

    // Use the last 30 days from the available date range
    let query_start = end_date - Duration::days(30);
    let query_limit = 30; // Request 30 data points

    // Get and display prices with different timeframes
    get_prices_with_timeframes(&repo, symbol, &query_start, query_limit).await?;

    // Example 4: Benchmarking repository performance
    info!("=== Example 4: Performance Benchmarking ===");
    benchmark_repository_performance(&repo, symbol).await?;

    info!("ClickHouse Repository Example completed successfully");
    Ok(())
}

// Create and initialize the repository
fn create_repository() -> Result<ClickHouseHistoricalRepository, Box<dyn std::error::Error>> {
    // Create a ClickHouse config for your local database
    let config = ClickHouseConfig::default();

    info!("Connecting to ClickHouse at {}", config.host);
    let client = Arc::new(ClickHouseClient::new(config)?);

    // Create the repository
    let repo = ClickHouseHistoricalRepository::new(client);

    Ok(repo)
}

// List all available symbols in the database
async fn list_all_symbols(
    repo: &ClickHouseHistoricalRepository,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Fetching all available symbols...");

    let start_time = Instant::now();
    let symbols = repo.list_available_symbols().await?;
    let elapsed = start_time.elapsed();

    info!("Found {} symbols in {:.2?}", symbols.len(), elapsed);

    if symbols.is_empty() {
        info!("No symbols found in database");
    } else {
        info!("First 10 symbols (or all if less than 10):");
        for (i, symbol) in symbols.iter().take(10).enumerate() {
            info!("  {}: {}", i + 1, symbol);
        }

        if symbols.len() > 10 {
            info!("  ... and {} more", symbols.len() - 10);
        }
    }

    Ok(())
}

// Get date range for a specific symbol
async fn get_symbol_date_range(
    repo: &ClickHouseHistoricalRepository,
    symbol: &str,
) -> Result<(DateTime<Utc>, DateTime<Utc>), Box<dyn std::error::Error>> {
    info!("Getting date range for symbol '{}'...", symbol);

    let start_time = Instant::now();
    let date_range = repo.get_date_range_for_symbol(symbol).await?;
    let elapsed = start_time.elapsed();

    let (start_date, end_date) = date_range;

    info!(
        "Date range for '{}': {} to {} ({} days) - query took {:.2?}",
        symbol,
        start_date.format("%Y-%m-%d"),
        end_date.format("%Y-%m-%d"),
        (end_date - start_date).num_days(),
        elapsed
    );

    Ok((start_date, end_date))
}

// Get historical prices with different timeframes
async fn get_prices_with_timeframes(
    repo: &ClickHouseHistoricalRepository,
    symbol: &str,
    start_date: &DateTime<Utc>,
    limit: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let timeframes = vec![
        (TimeFrame::Minute, "Minute"),
        (TimeFrame::Hour, "Hour"),
        (TimeFrame::Day, "Day"),
        (TimeFrame::Week, "Week"),
    ];

    info!(
        "Fetching historical {} prices for '{}' from {}",
        limit,
        symbol,
        start_date.format("%Y-%m-%d"),
    );

    for (timeframe, name) in timeframes {
        info!("Querying with {} timeframe...", name);

        let start_time = Instant::now();
        match repo
            .get_historical_prices(symbol, &timeframe, start_date, limit)
            .await
        {
            Ok(prices) => {
                let elapsed = start_time.elapsed();

                info!(
                    "{} timeframe: Retrieved {} price points in {:.2?}",
                    name,
                    prices.len(),
                    elapsed
                );

                if !prices.is_empty() {
                    let sample_count = std::cmp::min(3, prices.len());
                    info!("Sample prices (first {}):", sample_count);

                    for (i, price) in prices.iter().take(sample_count).enumerate() {
                        info!("  Sample {}: {}", i + 1, price);
                    }

                    if prices.len() > 1 {
                        // Calculate basic stats
                        let min = prices.iter().min().unwrap();
                        let max = prices.iter().max().unwrap();
                        let range = *max - min.to_dec();
                        let avg: Positive = prices.iter().sum::<Positive>() / prices.len() as f64;

                        info!(
                            "Price statistics - Min: {}, Max: {}, Avg: {}, Range: {}",
                            min, max, avg, range
                        );
                    }
                }
            }
            Err(e) => {
                error!("Error fetching {} timeframe data: {}", name, e);
            }
        }
    }

    Ok(())
}

// Benchmark repository performance
async fn benchmark_repository_performance(
    repo: &ClickHouseHistoricalRepository,
    symbol: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Running performance benchmarks...");

    // Get the full date range first
    let (full_start, full_end) = repo.get_date_range_for_symbol(symbol).await?;

    // Define different time window sizes to test
    let window_sizes = vec![
        Duration::days(1),
        Duration::days(7),
        Duration::days(30),
        Duration::days(90),
        Duration::days(365),
    ];

    // Run the benchmark for each window size
    for window in window_sizes {
        // Make sure we don't exceed the full date range
        let start_date = if full_end - window > full_start {
            full_end - window
        } else {
            full_start
        };

        // Calculate appropriate limits for each window size
        let daily_limit = window.num_days() as usize;
        let hourly_limit = if window.num_days() <= 30 {
            (window.num_days() * 24) as usize // 24 hours per day
        } else {
            0 // Skip hourly data for large windows
        };

        info!(
            "Benchmarking {} day window ({} to {})",
            window.num_days(),
            start_date.format("%Y-%m-%d"),
            full_end.format("%Y-%m-%d")
        );

        // Test daily timeframe
        let start_time = Instant::now();
        let daily_result = repo
            .get_historical_prices(symbol, &TimeFrame::Day, &start_date, daily_limit)
            .await;
        let daily_elapsed = start_time.elapsed();

        match daily_result {
            Ok(prices) => {
                info!(
                    "Daily data: {} points in {:.2?} ({:.2} points/ms)",
                    prices.len(),
                    daily_elapsed,
                    prices.len() as f64 / daily_elapsed.as_millis() as f64
                );
            }
            Err(e) => {
                error!("Error in daily benchmark: {}", e);
            }
        }

        // Test hourly timeframe (if window is 30 days or less)
        if hourly_limit > 0 {
            let start_time = Instant::now();
            let hourly_result = repo
                .get_historical_prices(symbol, &TimeFrame::Hour, &start_date, hourly_limit)
                .await;
            let hourly_elapsed = start_time.elapsed();

            match hourly_result {
                Ok(prices) => {
                    info!(
                        "Hourly data: {} points in {:.2?} ({:.2} points/ms)",
                        prices.len(),
                        hourly_elapsed,
                        prices.len() as f64 / hourly_elapsed.as_millis() as f64
                    );
                }
                Err(e) => {
                    error!("Error in hourly benchmark: {}", e);
                }
            }
        }
    }

    Ok(())
}