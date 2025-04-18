use std::str::FromStr;
use std::time::Duration;
use chrono::{DateTime, NaiveDateTime, Utc};
use clickhouse_rs::{Options, Pool};
use optionstratlib::{pos, Positive};
use optionstratlib::utils::TimeFrame;
use rust_decimal::Decimal;
use tracing::{debug, info, instrument};
use crate::infrastructure::clickhouse::model::{OHLCVData, PriceType};
use crate::infrastructure::clickhouse::utils::row_to_datetime;
use crate::infrastructure::ClickHouseConfig;

/// Represents a client for interacting with ClickHouse historical data
pub struct ClickHouseClient {
    /// The connection pool for ClickHouse
    pub(crate) pool: Pool,
    /// The configuration for this client
    config: ClickHouseConfig,
}

impl ClickHouseClient {
    /// Creates a new ClickHouse client with the provided configuration
    #[instrument(name = "clickhouse_client_new", skip(config), level = "debug")]
    pub fn new(config: ClickHouseConfig) -> Result<Self, String> {
        let url = format!(
            "tcp://{}:{}@{}:{}/{}",
            config.username,
            config.password,
            config.host,
            config.port,
            config.database
        );

        let opts = Options::from_str(&url)
            .map_err(|e| format!("Failed to parse ClickHouse URL: {}", e))?
            .ping_timeout(Duration::from_secs(config.timeout))
            .query_timeout(Duration::from_secs(5))
            .connection_timeout(Duration::from_secs(1));

        let pool = Pool::new(opts);

        info!("Created new ClickHouse client for host: {}", config.host);
        Ok(Self { pool, config })
    }


    /// Creates a new ClickHouse client with default configuration
    pub fn default() -> Result<Self, String> {
        Self::new(ClickHouseConfig::default())
    }

    /// Fetches historical price data for a given symbol, time frame, and date range
    #[instrument(skip(self), level = "debug")]
    pub async fn fetch_historical_prices(
        &self,
        symbol: &str,
        timeframe: &TimeFrame,
        start_date: &chrono::DateTime<chrono::Utc>,
        end_date: &chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Positive>, String> {
        debug!(
            "Fetching historical prices for {} from {} to {} with timeframe {:?}",
            symbol, start_date, end_date, timeframe
        );

        // Build the SQL query based on the timeframe
        let query = self.build_timeframe_query(symbol, timeframe, start_date, end_date)?;

        // Execute the query
        let results = self.execute_query(query).await?;

        // Map results to a vector of Positive prices (usually close prices)
        let prices: Vec<Positive> = results
            .into_iter()
            .map(|data| data.close)
            .collect();

        info!(
            "Fetched {} historical prices for {}",
            prices.len(),
            symbol
        );

        Ok(prices)
    }

    /// Fetches full OHLCV data for a given symbol, time frame, and date range
    #[instrument(skip(self), level = "debug")]
    pub async fn fetch_ohlcv_data(
        &self,
        symbol: &str,
        timeframe: &TimeFrame,
        start_date: &chrono::DateTime<chrono::Utc>,
        end_date: &chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<OHLCVData>, String> {
        debug!(
            "Fetching OHLCV data for {} from {} to {} with timeframe {:?}",
            symbol, start_date, end_date, timeframe
        );

        // Build the SQL query based on the timeframe
        let query = self.build_timeframe_query(symbol, timeframe, start_date, end_date)?;

        // Execute the query directly
        let results = self.execute_query(query).await?;

        info!(
            "Fetched {} OHLCV data points for {}",
            results.len(),
            symbol
        );

        Ok(results)
    }

    /// Builds an appropriate SQL query for the given timeframe
    fn build_timeframe_query(
        &self,
        symbol: &str,
        timeframe: &TimeFrame,
        start_date: &chrono::DateTime<chrono::Utc>,
        end_date: &chrono::DateTime<chrono::Utc>,
    ) -> Result<String, String> {
        // Format dates for SQL
        let start_date_str = start_date.format("%Y-%m-%d %H:%M:%S").to_string();
        let end_date_str = end_date.format("%Y-%m-%d %H:%M:%S").to_string();

        // Base query for minute data (smallest timeframe supported)
        if *timeframe == TimeFrame::Minute {
            return Ok(format!(
                "SELECT symbol, timestamp, open, high, low, close, volume \
                FROM ohlcv \
                WHERE symbol = '{}' \
                AND timestamp BETWEEN '{}' AND '{}' \
                ORDER BY timestamp",
                symbol, start_date_str, end_date_str
            ));
        }

        // For larger timeframes, we need to aggregate the minute data
        let interval = match timeframe {
            TimeFrame::Minute => "1 MINUTE", // Already handled above, but included for completeness
            TimeFrame::Hour => "1 HOUR",
            TimeFrame::Day => "1 DAY",
            TimeFrame::Week => "1 WEEK",
            TimeFrame::Month => "1 MONTH",
            _ => return Err(format!("Unsupported timeframe: {:?}", timeframe)),
        };

        // Query with aggregation for larger timeframes
        Ok(format!(
            "SELECT 
                symbol,
                toStartOfInterval(timestamp, INTERVAL {}) as timestamp,
                any(open) as open,
                max(high) as high,
                min(low) as low,
                any(arrayElement(
                    groupArray(close), 
                    length(groupArray(close))
                )) as close,
                sum(volume) as volume
            FROM ohlcv
            WHERE symbol = '{}' 
            AND timestamp BETWEEN '{}' AND '{}'
            GROUP BY symbol, timestamp
            ORDER BY timestamp",
            interval, symbol, start_date_str, end_date_str
        ))
    }

    /// Executes a SQL query and returns OHLCV data
    async fn execute_query(&self, query: String) -> Result<Vec<OHLCVData>, String> {
        debug!("Executing ClickHouse query: {}", query);

        let mut conn = self.pool.get_handle()
            .await
            .map_err(|e| format!("Failed to get ClickHouse connection: {}", e))?;

        let block = conn.query(query)
            .fetch_all()
            .await
            .map_err(|e| format!("Failed to execute ClickHouse query: {}", e))?;

        let mut results = Vec::new();

        for row in block.rows() {
            let symbol: String = row.get("symbol")
                .map_err(|e| format!("Failed to get 'symbol' from row: {}", e))?;
            
            let timestamp = row_to_datetime(&row, "timestamp")?;
            
            let open: f32 = row.get("open")
                .map_err(|e| format!("Failed to get 'open' from row: {}", e))?;

            let high: f32 = row.get("high")
                .map_err(|e| format!("Failed to get 'high' from row: {}", e))?;

            let low: f32 = row.get("low")
                .map_err(|e| format!("Failed to get 'low' from row: {}", e))?;

            let close: f32 = row.get("close")
                .map_err(|e| format!("Failed to get 'close' from row: {}", e))?;

            let volume: u32 = row.get("volume")
                .map_err(|e| format!("Failed to get 'volume' from row: {}", e))?;

            // Convert to Positive, which doesn't allow negative values
            let open_pos = pos!(open as f64);
            let high_pos = pos!(high as f64);
            let low_pos = pos!(low as f64);
            let close_pos = pos!(close as f64);

            results.push(OHLCVData {
                symbol,
                timestamp,
                open: open_pos,
                high: high_pos,
                low: low_pos,
                close: close_pos,
                volume,
            });
        }

        Ok(results)
    }

    /// Converts a vector of OHLCV data to a vector of prices based on selection criteria
    pub fn extract_prices(&self, data: &[OHLCVData], price_type: PriceType) -> Vec<Positive> {
        data.iter()
            .map(|ohlcv| match price_type {
                PriceType::Open => ohlcv.open,
                PriceType::High => ohlcv.high,
                PriceType::Low => ohlcv.low,
                PriceType::Close => ohlcv.close,
                PriceType::Typical => {
                    // Typical price is (high + low + close) / 3
                    let sum = ohlcv.high + ohlcv.low + ohlcv.close;
                    let typical = sum / Decimal::from(3);
                    typical
                }
            })
            .collect()
    }
}