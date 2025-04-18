use chrono::{DateTime, Duration, TimeZone, Utc};
use optionstratlib::{Positive, pos};
use optionstratlib::utils::{setup_logger, TimeFrame};
use std::sync::Arc;
use tracing::{info, Level};
use optionchain_simulator::infrastructure::{ClickHouseClient, ClickHouseConfig, ClickHouseHistoricalRepository, HistoricalDataRepository, PriceType};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    setup_logger();
    info!("Starting ClickHouse client example");

    // Create a ClickHouse config for your local database
    let config = ClickHouseConfig {
        host: "localhost".to_string(),
        port: 9000,
        username: "default".to_string(),
        password: "".to_string(),  // Default ClickHouse has no password
        database: "default".to_string(),
        timeout: 30,
    };

    // Create the ClickHouse client
    info!("Connecting to ClickHouse at {}", config.host);
    let client = Arc::new(ClickHouseClient::new(config)?);

    // Create the repository
    let repo = ClickHouseHistoricalRepository::new(client.clone());

    // Check available symbols
    info!("Checking available symbols...");
    match repo.list_available_symbols() {
        Ok(symbols) => {
            info!("Found {} symbols in the database", symbols.len());
            for symbol in &symbols {
                info!("Available symbol: {}", symbol);
            }
        }
        Err(e) => {
            info!("Error listing symbols: {}", e);
        }
    }

    // Define the symbol and date range
    let symbol = "CL";  // Crude oil

    // Get the date range for the symbol
    match repo.get_date_range_for_symbol(symbol) {
        Ok((min_date, max_date)) => {
            info!("Data available for {} from {} to {}", 
                  symbol, 
                  min_date.format("%Y-%m-%d %H:%M:%S"),
                  max_date.format("%Y-%m-%d %H:%M:%S"));

            // Set our query date range to the last 30 days of available data
            let end_date = max_date;
            let start_date = end_date - Duration::days(30);

            // Example 1: Get daily prices
            info!("Fetching daily prices for the last 30 days...");
            match repo.get_historical_prices(symbol, &TimeFrame::Day, &start_date, &end_date) {
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
                    info!("Error fetching daily prices: {}", e);
                }
            }

            // Example 2: Get hourly prices
            info!("Fetching hourly prices for the last 7 days...");
            let hourly_start = end_date - Duration::days(7);
            match repo.get_historical_prices(symbol, &TimeFrame::Hour, &hourly_start, &end_date) {
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
                    info!("Error fetching hourly prices: {}", e);
                }
            }

            // Example 3: Get OHLCV data and extract different price types
            info!("Fetching OHLCV data for the last 14 days...");
            let ohlcv_start = end_date - Duration::days(14);
            match client.fetch_ohlcv_data(symbol, &TimeFrame::Day, &ohlcv_start, &end_date).await {
                Ok(ohlcv_data) => {
                    info!("Retrieved {} OHLCV data points", ohlcv_data.len());

                    // Extract different price types
                    let open_prices = client.extract_prices(&ohlcv_data, PriceType::Open);
                    let high_prices = client.extract_prices(&ohlcv_data, PriceType::High);
                    let low_prices = client.extract_prices(&ohlcv_data, PriceType::Low);
                    let close_prices = client.extract_prices(&ohlcv_data, PriceType::Close);
                    let typical_prices = client.extract_prices(&ohlcv_data, PriceType::Typical);

                    info!("Price counts - Open: {}, High: {}, Low: {}, Close: {}, Typical: {}", 
                          open_prices.len(), high_prices.len(), low_prices.len(), 
                          close_prices.len(), typical_prices.len());

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
                    info!("Error fetching OHLCV data: {}", e);
                }
            }
        }
        Err(e) => {
            info!("Error getting date range for {}: {}", symbol, e);
        }
    }

    info!("ClickHouse client example completed");
    Ok(())
}