use chrono::Duration;
use optionchain_simulator::infrastructure::{
    ClickHouseClient, ClickHouseConfig, ClickHouseHistoricalRepository, HistoricalDataRepository,
    PriceType,
};
use optionstratlib::utils::{TimeFrame, setup_logger};
use optionstratlib::{Positive, pos};
use std::sync::Arc;
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    setup_logger();
    info!("Starting ClickHouse client example");
    let config = ClickHouseConfig::default();

    // Create the ClickHouse client
    info!("Connecting to ClickHouse at {}", config.host);
    let client = Arc::new(ClickHouseClient::new(config)?);

    // Create the repository
    let repo = ClickHouseHistoricalRepository::new(client.clone());

    // Check available symbols
    info!("Checking available symbols...");
    match repo.list_available_symbols().await {
        Ok(symbols) => {
            info!("Found {} symbols in the database", symbols.len());
            for symbol in &symbols {
                info!("Available symbol: {}", symbol);
            }
        }
        Err(e) => {
            error!("Error listing symbols: {}", e);
        }
    }

    // Define the symbol
    let symbol = "CL"; // Crude oil

    // Get the date range for the symbol
    match repo.get_date_range_for_symbol(symbol).await {
        Ok((min_date, max_date)) => {
            info!(
                "Data available for {} from {} to {}",
                symbol,
                min_date.format("%Y-%m-%d %H:%M:%S"),
                max_date.format("%Y-%m-%d %H:%M:%S")
            );

            // Set our query start date to 30 days before max_date
            let start_date = max_date - Duration::days(30);

            // Example 1: Get daily prices (up to 30 points)
            info!("Fetching daily prices for the last 30 days...");
            match repo
                .get_historical_prices(symbol, &TimeFrame::Day, &start_date, 30)
                .await
            {
                Ok(prices) => {
                    info!("Retrieved {} daily price points", prices.len());

                    // Display a few prices
                    let display_count = std::cmp::min(5, prices.len());
                    info!("First {} daily prices:", display_count);
                    for (i, price) in prices.iter().take(display_count).enumerate() {
                        info!("Day {}: {}", i + 1, price);
                    }

                    // Calculate simple statistics
                    if !prices.is_empty() {
                        let sum: Positive = prices.iter().sum();
                        let avg = sum / prices.len() as f64;
                        let min = prices.iter().min().unwrap();
                        let max = prices.iter().max().unwrap();

                        info!("Statistics - Avg: {}, Min: {}, Max: {}", avg, min, max);
                    }
                }
                Err(e) => {
                    error!("Error fetching daily prices: {}", e);
                }
            }

            // Example 2: Get hourly prices (up to 168 points - 7 days * 24 hours)
            info!("Fetching hourly prices for the last 7 days...");
            let hourly_start = max_date - Duration::days(7);
            let hourly_limit = 168; // 7 days * 24 hours
            match repo
                .get_historical_prices(symbol, &TimeFrame::Hour, &hourly_start, hourly_limit)
                .await
            {
                Ok(prices) => {
                    info!("Retrieved {} hourly price points", prices.len());

                    // Display a few prices
                    let display_count = std::cmp::min(5, prices.len());
                    info!("First {} hourly prices:", display_count);
                    for (i, price) in prices.iter().take(display_count).enumerate() {
                        info!("Hour {}: {}", i + 1, price);
                    }
                }
                Err(e) => {
                    error!("Error fetching hourly prices: {}", e);
                }
            }

            // Example 3: Get OHLCV data and extract different price types
            info!("Fetching OHLCV data for the last 14 days...");
            let ohlcv_start = max_date - Duration::days(14);
            let ohlcv_limit = 14; // 14 daily data points
            match client
                .fetch_ohlcv_data(symbol, &TimeFrame::Day, &ohlcv_start, ohlcv_limit)
                .await
            {
                Ok(ohlcv_data) => {
                    info!("Retrieved {} OHLCV data points", ohlcv_data.len());

                    // Extract different price types
                    let open_prices = client.extract_prices(&ohlcv_data, PriceType::Open);
                    let high_prices = client.extract_prices(&ohlcv_data, PriceType::High);
                    let low_prices = client.extract_prices(&ohlcv_data, PriceType::Low);
                    let close_prices = client.extract_prices(&ohlcv_data, PriceType::Close);
                    let typical_prices = client.extract_prices(&ohlcv_data, PriceType::Typical);

                    info!(
                        "Price counts - Open: {}, High: {}, Low: {}, Close: {}, Typical: {}",
                        open_prices.len(),
                        high_prices.len(),
                        low_prices.len(),
                        close_prices.len(),
                        typical_prices.len()
                    );

                    // Display a sample OHLCV point
                    if !ohlcv_data.is_empty() {
                        let sample = &ohlcv_data[0];
                        info!("Sample OHLCV: {}", sample);

                        // Calculate high-low range for the sample
                        let range = sample.high - sample.low;
                        info!("High-Low Range: {}", range);

                        // Calculate percentage change from open to close
                        let pct_change = ((sample.close - sample.open) / sample.open) * pos!(100.0);
                        info!("Percent Change: {}%", pct_change);
                    }
                }
                Err(e) => {
                    error!("Error fetching OHLCV data: {}", e);
                }
            }
        }
        Err(e) => {
            error!("Error getting date range for {}: {}", symbol, e);
        }
    }

    info!("ClickHouse client example completed");
    Ok(())
}