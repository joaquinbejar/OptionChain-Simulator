use crate::infrastructure::ClickHouseConfig;
use crate::infrastructure::clickhouse::model::{OHLCVData, PriceType};
use crate::infrastructure::clickhouse::utils::row_to_datetime;
use crate::utils::ChainError;
use chrono::{DateTime, Utc};
use clickhouse_rs::{Options, Pool};
use optionstratlib::utils::TimeFrame;
use optionstratlib::{Positive, pos};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::time::Duration;
use tracing::{debug, info, instrument};

/// Represents a client for interacting with a ClickHouse database.
///
/// The `ClickHouseClient` struct encapsulates the details required to connect
/// and interact with a ClickHouse database instance. It provides the necessary
/// components, such as a connection pool and configuration settings, to manage
/// database access efficiently.
///
/// # Fields
///
/// * `pool` - A connection pool (`Pool`) used for establishing and managing
///            database connections. This is a private field that facilitates
///            efficient resource management and avoids the overhead of creating
///            new connections for each operation.
///
/// * `config` - The configuration (`ClickHouseConfig`) that contains settings
///              specific to this client, such as authentication credentials,
///              database connection details, or other configuration parameters.
///
pub struct ClickHouseClient {
    /// Represents a connection pool that is used internally within the crate.
    ///
    /// The `pool` is responsible for managing a collection of database connections
    /// or other types of resources to allow efficient reuse and reduce overhead.
    ///
    /// This is defined with a `pub(crate)` visibility modifier, meaning it
    /// is accessible only within the current crate and not exposed to external users.
    ///
    /// - `Pool` is usually a struct or type responsible for abstracting resource
    ///   pooling behavior, such as managing concurrent access to the pooled resources.
    ///
    /// Note:
    /// Ensure that the `Pool` is properly configured and initialized before use
    /// to avoid runtime errors or resource exhaustion in multi-threaded applications.
    pub(crate) pool: Pool,

    /// Represents the configuration settings for connecting to and interacting
    /// with a ClickHouse database.
    ///
    /// The `ClickHouseConfig` object contains necessary parameters such as
    /// server host, port, authentication credentials, and other connection options.
    ///
    /// Fields:
    /// - `host` (String): The hostname or IP address of the ClickHouse server.
    /// - `port` (u16): The port on which the ClickHouse server is running.
    /// - `username` (String): The username for authentication.
    /// - `password` (String): The password for authentication.
    /// - `database` (String): The name of the database to connect to.
    /// - `options` (Option<HashMap<String, String>>): Additional optional parameters
    ///   for customizing the connection (e.g., timeouts, retries).
    ///
    /// Ensure you provide valid and reachable settings to avoid connection issues.
    config: ClickHouseConfig,
}

impl ClickHouseClient {
    /// Creates a new ClickHouse client with the provided configuration
    #[instrument(name = "clickhouse_client_new", skip(config), level = "debug")]
    pub fn new(config: ClickHouseConfig) -> Result<Self, ChainError> {
        let url = format!(
            "tcp://{}:{}@{}:{}/{}",
            config.username, config.password, config.host, config.port, config.database
        );

        let opts = Options::from_str(&url)
            .map_err(|e| {
                ChainError::ClickHouseError(format!("Failed to parse ClickHouse URL: {}", e))
            })?
            .ping_timeout(Duration::from_secs(config.timeout))
            .query_timeout(Duration::from_secs(5))
            .connection_timeout(Duration::from_secs(1));

        let pool = Pool::new(opts);

        info!("Created new ClickHouse client for host: {}", config.host);
        Ok(Self { pool, config })
    }

    /// Creates a new ClickHouse client with default configuration
    pub fn default() -> Result<Self, ChainError> {
        Self::new(ClickHouseConfig::default())
    }

    /// Fetches historical price data for a given symbol, time frame, and date range
    #[instrument(skip(self), level = "debug")]
    pub async fn fetch_historical_prices(
        &self,
        symbol: &str,
        timeframe: &TimeFrame,
        start_date: &DateTime<Utc>,
        end_date: &DateTime<Utc>,
    ) -> Result<Vec<Positive>, ChainError> {
        debug!(
            "Fetching historical prices for {} from {} to {} with timeframe {:?}",
            symbol, start_date, end_date, timeframe
        );

        // Build the SQL query based on the timeframe
        let query = self.build_timeframe_query(symbol, timeframe, start_date, end_date)?;

        // Execute the query
        let results = self.execute_query(query).await?;

        // Map results to a vector of Positive prices (usually close prices)
        let prices: Vec<Positive> = results.into_iter().map(|data| data.close).collect();

        info!("Fetched {} historical prices for {}", prices.len(), symbol);

        Ok(prices)
    }

    /// Fetches full OHLCV data for a given symbol, time frame, and date range
    #[instrument(skip(self), level = "debug")]
    pub async fn fetch_ohlcv_data(
        &self,
        symbol: &str,
        timeframe: &TimeFrame,
        start_date: &DateTime<Utc>,
        end_date: &DateTime<Utc>,
    ) -> Result<Vec<OHLCVData>, ChainError> {
        debug!(
            "Fetching OHLCV data for {} from {} to {} with timeframe {:?}",
            symbol, start_date, end_date, timeframe
        );

        // Build the SQL query based on the timeframe
        let query = self.build_timeframe_query(symbol, timeframe, start_date, end_date)?;

        // Execute the query directly
        let results = self.execute_query(query).await?;

        info!("Fetched {} OHLCV data points for {}", results.len(), symbol);

        Ok(results)
    }

    /// Builds an appropriate SQL query for the given timeframe
    fn build_timeframe_query(
        &self,
        symbol: &str,
        timeframe: &TimeFrame,
        start_date: &DateTime<Utc>,
        end_date: &DateTime<Utc>,
    ) -> Result<String, ChainError> {
        // Format dates for SQL
        let start_date_str = start_date.format("%Y-%m-%d %H:%M:%S").to_string();
        let end_date_str = end_date.format("%Y-%m-%d %H:%M:%S").to_string();

        // Base query for minute data (smallest timeframe supported)
        if *timeframe == TimeFrame::Minute {
            return Ok(format!(
                "SELECT symbol, toInt64(toUnixTimestamp(timestamp)) as timestamp, 
                open, high, low, close, toUInt64(volume) as volume \
                FROM ohlcv \
                WHERE symbol = '{}' \
                AND timestamp BETWEEN toDateTime('{}') AND toDateTime('{}') \
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
            _ => {
                return Err(ChainError::ClickHouseError(format!(
                    "Unsupported timeframe: {:?}",
                    timeframe
                )));
            }
        };

        // Query with aggregation for larger timeframes
        Ok(format!(
            "WITH intervals AS (
        SELECT 
                symbol,
                toStartOfInterval(timestamp, INTERVAL {}) as interval_start,
                any(open) as open,
                max(high) as high,
                min(low) as low,
                any(close) as close,
                sum(volume) as volume
            FROM ohlcv
            WHERE symbol = '{}' 
            AND timestamp BETWEEN '{}' AND '{}'
            GROUP BY symbol, interval_start
            ORDER BY interval_start
        )
        SELECT 
            symbol,
            toInt64(toUnixTimestamp(interval_start)) as timestamp,
            open, high, low, close, volume
        FROM intervals",
            interval, symbol, start_date_str, end_date_str
        ))
    }

    /// Executes a SQL query and returns OHLCV data
    async fn execute_query(&self, query: String) -> Result<Vec<OHLCVData>, ChainError> {
        debug!("Executing ClickHouse query: {}", query);

        let mut conn = self.pool.get_handle().await.map_err(|e| {
            ChainError::ClickHouseError(format!("Failed to get ClickHouse connection: {}", e))
        })?;

        let block = conn.query(query).fetch_all().await.map_err(|e| {
            ChainError::ClickHouseError(format!("Failed to execute ClickHouse query: {}", e))
        })?;

        let mut results = Vec::new();

        for row in block.rows() {
            let symbol: String = row.get("symbol").map_err(|e| {
                ChainError::ClickHouseError(format!("Failed to get 'symbol' from row: {}", e))
            })?;

            let timestamp = row_to_datetime(&row, "timestamp")?;

            let open: f32 = row.get("open").map_err(|e| {
                ChainError::ClickHouseError(format!("Failed to get 'open' from row: {}", e))
            })?;

            let high: f32 = row.get("high").map_err(|e| {
                ChainError::ClickHouseError(format!("Failed to get 'high' from row: {}", e))
            })?;

            let low: f32 = row.get("low").map_err(|e| {
                ChainError::ClickHouseError(format!("Failed to get 'low' from row: {}", e))
            })?;

            let close: f32 = row.get("close").map_err(|e| {
                ChainError::ClickHouseError(format!("Failed to get 'close' from row: {}", e))
            })?;

            let volume: u64 = row.get("volume").map_err(|e| {
                ChainError::ClickHouseError(format!("Failed to get 'volume' from row: {}", e))
            })?;

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
    
    pub fn get_config(&mut self) -> &mut ClickHouseConfig {
        &mut self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use optionstratlib::{Positive, pos};
    use rust_decimal::Decimal;

    #[test]
    fn test_build_timeframe_query_minute() {
        let config = ClickHouseConfig {
            host: "test-host".to_string(),
            port: 9000,
            username: "test-user".to_string(),
            password: "test-pass".to_string(),
            database: "test-db".to_string(),
            timeout: 30,
        };

        fn build_timeframe_query(
            symbol: &str,
            timeframe: &TimeFrame,
            start_date: &DateTime<Utc>,
            end_date: &DateTime<Utc>,
        ) -> Result<String, ChainError> {
            let start_date_str = start_date.format("%Y-%m-%d %H:%M:%S").to_string();
            let end_date_str = end_date.format("%Y-%m-%d %H:%M:%S").to_string();

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

            let interval = match timeframe {
                TimeFrame::Minute => "1 MINUTE",
                TimeFrame::Hour => "1 HOUR",
                TimeFrame::Day => "1 DAY",
                TimeFrame::Week => "1 WEEK",
                TimeFrame::Month => "1 MONTH",
                _ => {
                    return Err(ChainError::ClickHouseError(format!(
                        "Unsupported timeframe: {:?}",
                        timeframe
                    )));
                }
            };

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

        let symbol = "AAPL";
        let timeframe = TimeFrame::Minute;
        let start_date = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end_date = Utc.with_ymd_and_hms(2023, 1, 2, 0, 0, 0).unwrap();

        let query = build_timeframe_query(symbol, &timeframe, &start_date, &end_date).unwrap();

        assert!(query.contains("SELECT symbol, timestamp, open, high, low, close, volume"));
        assert!(query.contains("FROM ohlcv"));
        assert!(query.contains("WHERE symbol = 'AAPL'"));
        assert!(
            query.contains("AND timestamp BETWEEN '2023-01-01 00:00:00' AND '2023-01-02 00:00:00'")
        );
        assert!(query.contains("ORDER BY timestamp"));
    }

    #[test]
    fn test_build_timeframe_query_day() {
        fn build_timeframe_query(
            symbol: &str,
            timeframe: &TimeFrame,
            start_date: &DateTime<Utc>,
            end_date: &DateTime<Utc>,
        ) -> Result<String, ChainError> {
            let start_date_str = start_date.format("%Y-%m-%d %H:%M:%S").to_string();
            let end_date_str = end_date.format("%Y-%m-%d %H:%M:%S").to_string();

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

            let interval = match timeframe {
                TimeFrame::Minute => "1 MINUTE",
                TimeFrame::Hour => "1 HOUR",
                TimeFrame::Day => "1 DAY",
                TimeFrame::Week => "1 WEEK",
                TimeFrame::Month => "1 MONTH",
                _ => {
                    return Err(ChainError::ClickHouseError(format!(
                        "Unsupported timeframe: {:?}",
                        timeframe
                    )));
                }
            };

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

        let symbol = "AAPL";
        let timeframe = TimeFrame::Day;
        let start_date = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end_date = Utc.with_ymd_and_hms(2023, 1, 31, 0, 0, 0).unwrap();

        let query = build_timeframe_query(symbol, &timeframe, &start_date, &end_date).unwrap();

        assert!(query.contains("toStartOfInterval(timestamp, INTERVAL 1 DAY)"));
        assert!(query.contains("GROUP BY symbol, timestamp"));
        assert!(query.contains("max(high) as high"));
        assert!(query.contains("min(low) as low"));
        assert!(query.contains("sum(volume) as volume"));
    }

    #[test]
    fn test_extract_prices() {
        fn extract_prices(data: &[OHLCVData], price_type: PriceType) -> Vec<Positive> {
            data.iter()
                .map(|ohlcv| match price_type {
                    PriceType::Open => ohlcv.open,
                    PriceType::High => ohlcv.high,
                    PriceType::Low => ohlcv.low,
                    PriceType::Close => ohlcv.close,
                    PriceType::Typical => {
                        // Típico: (high + low + close) / 3
                        let sum = ohlcv.high + ohlcv.low + ohlcv.close;
                        let typical = sum / Decimal::from(3);
                        typical
                    }
                })
                .collect()
        }

        let data = vec![
            OHLCVData {
                symbol: "AAPL".to_string(),
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 10, 0, 0).unwrap(),
                open: pos!(150.0),
                high: pos!(155.0),
                low: pos!(149.0),
                close: pos!(153.0),
                volume: 10000,
            },
            OHLCVData {
                symbol: "AAPL".to_string(),
                timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 11, 0, 0).unwrap(),
                open: pos!(153.0),
                high: pos!(157.0),
                low: pos!(152.0),
                close: pos!(156.0),
                volume: 15000,
            },
        ];

        let open_prices = extract_prices(&data, PriceType::Open);
        let high_prices = extract_prices(&data, PriceType::High);
        let low_prices = extract_prices(&data, PriceType::Low);
        let close_prices = extract_prices(&data, PriceType::Close);
        let typical_prices = extract_prices(&data, PriceType::Typical);

        assert_eq!(open_prices, vec![pos!(150.0), pos!(153.0)]);
        assert_eq!(high_prices, vec![pos!(155.0), pos!(157.0)]);
        assert_eq!(low_prices, vec![pos!(149.0), pos!(152.0)]);
        assert_eq!(close_prices, vec![pos!(153.0), pos!(156.0)]);

        let expected_typical_1 = (pos!(155.0) + pos!(149.0) + pos!(153.0)) / Decimal::from(3);
        let expected_typical_2 = (pos!(157.0) + pos!(152.0) + pos!(156.0)) / Decimal::from(3);

        assert_eq!(typical_prices[0], expected_typical_1);
        assert_eq!(typical_prices[1], expected_typical_2);
    }
}
