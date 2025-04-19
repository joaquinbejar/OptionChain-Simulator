//! A repository that interacts with ClickHouse to provide historical financial data.
use crate::infrastructure::clickhouse::{ClickHouseClient, HistoricalDataRepository};
use crate::infrastructure::row_to_datetime;
use crate::utils::ChainError;
use async_trait::async_trait;
use chrono::Utc;
use optionstratlib::Positive;
use optionstratlib::utils::TimeFrame;
use std::sync::Arc;

/// Represents a repository for accessing historical data stored in a ClickHouse database.
///
/// The `ClickHouseHistoricalRepository` provides an abstraction over the ClickHouse client
/// to interact with historical data, such as querying or managing stored records.
///
/// # Fields
///
/// * `client` - An `Arc`-wrapped instance of the `ClickHouseClient`, enabling shared ownership and
///              thread-safe access to the ClickHouse database.
///
/// # Notes
///
/// The `ClickHouseHistoricalRepository` assumes that the provided `ClickHouseClient` is properly
/// configured to interact with the database (e.g., connection details and authentication).
///
pub struct ClickHouseHistoricalRepository {
    /// The underlying ClickHouse client
    client: Arc<ClickHouseClient>,
}

impl ClickHouseHistoricalRepository {
    /// Creates a new instance of the struct.
    ///
    /// # Arguments
    ///
    /// * `client` - An `Arc<ClickHouseClient>` that represents a shared reference-counted pointer
    ///   to a ClickHouse client instance. This allows multiple parts of the application to share
    ///   the same client without duplicating resources.
    ///
    /// # Returns
    ///
    /// Returns a new instance of the struct, initialized with the provided `client`.
    ///
    /// This function ensures that the created struct can leverage the shared
    /// ClickHouse client for performing database operations efficiently.
    pub fn new(client: Arc<ClickHouseClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl HistoricalDataRepository for ClickHouseHistoricalRepository {
    ///
    /// Retrieves historical price data for a given symbol, timeframe, and date range.
    ///
    /// This function utilizes a Tokio runtime to block and execute an asynchronous operation
    /// for fetching the requested historical price data. The operation is delegated to the
    /// `fetch_historical_prices` method of an async client.
    ///
    /// # Parameters
    /// - `symbol`: A string slice representing the symbol for which to fetch historical prices (e.g., stock symbol, currency pair, etc.).
    /// - `timeframe`: A reference to a `TimeFrame` object representing the interval (e.g., daily, hourly, etc.) of the historical data.
    /// - `start_date`: A reference to a `chrono::DateTime<Utc>` representing the start of the date range for the data (inclusive).
    /// - `end_date`: A reference to a `chrono::DateTime<Utc>` representing the end of the date range for the data (exclusive).
    ///
    /// # Returns
    /// - `Ok(Vec<Positive>)`: A vector of `Positive` objects, each representing historical price data within the specified timeframe and date range.
    /// - `Err(String)`: An error message string if the runtime creation fails or if the asynchronous operation encounters an error.
    ///
    /// # Errors
    /// - Returns an `Err` if:
    ///   - The Tokio runtime fails to initialize.
    ///   - The asynchronous `fetch_historical_prices` call fails to retrieve the data.
    ///
    /// # Note
    /// - Ensure that the `fetch_historical_prices` method being called on `self.client` is correctly implemented and capable of handling
    ///   the provided parameters.
    ///
    async fn get_historical_prices(
        &self,
        symbol: &str,
        timeframe: &TimeFrame,
        start_date: &chrono::DateTime<Utc>,
        end_date: &chrono::DateTime<Utc>,
    ) -> Result<Vec<Positive>, ChainError> {
        self.client
            .fetch_historical_prices(symbol, timeframe, start_date, end_date)
            .await
    }

    /// Retrieves a list of distinct symbols from an `ohlcv` table in the ClickHouse database.
    ///
    /// This function creates a new Tokio runtime, establishes a connection to the ClickHouse database via a connection pool,
    /// executes an SQL query to collect unique symbols, and returns them in a vector sorted by the symbol.
    ///
    /// # Returns
    /// - `Ok(Vec<String>)`: If the operation is successful, a vector of distinct symbols is returned.
    /// - `Err(String)`: If an error occurs during any step (runtime creation, database connection, query execution, or row extraction),
    ///   a descriptive error message is returned.
    ///
    /// # Errors
    /// The function could fail for several reasons:
    /// - Failure to create a Tokio runtime.
    /// - Failure to obtain a connection from the ClickHouse connection pool.
    /// - Failure to execute the query against the database.
    /// - Failure to extract the `symbol` field from the retrieved rows.
    ///
    /// # Query Details
    /// The SQL query used is:
    /// ```sql
    /// SELECT DISTINCT symbol FROM ohlcv ORDER BY symbol;
    /// ```
    /// It fetches all unique `symbol` values from the `ohlcv` table and orders them alphabetically.
    ///
    /// # Important Notes
    /// - This function uses a synchronous executor (`runtime.block_on`) to run the asynchronous logic,
    ///   which may have performance implications if the function is called in an already asynchronous context.
    /// - Ensure that the `client.pool` has been properly configured to connect to a ClickHouse database.
    /// - The caller should handle the errors returned to identify or log the specific root cause.
    ///
    async fn list_available_symbols(&self) -> Result<Vec<String>, ChainError> {
        let mut conn = self
            .client
            .pool
            .get_handle()
            .await
            .map_err(|e| format!("Failed to get ClickHouse connection: {}", e))?;

        let query = "SELECT DISTINCT symbol FROM ohlcv ORDER BY symbol";

        let block = conn
            .query(query)
            .fetch_all()
            .await
            .map_err(|e| format!("Failed to execute ClickHouse query: {}", e))?;

        let mut symbols = Vec::new();
        for row in block.rows() {
            let symbol: String = row
                .get("symbol")
                .map_err(|e| format!("Failed to get 'symbol' from row: {}", e))?;
            symbols.push(symbol);
        }

        Ok(symbols)
    }

    /// Retrieves the date range (minimum and maximum timestamp) for a given financial symbol
    /// from the `ohlcv` table in the ClickHouse database.
    ///
    /// # Arguments
    ///
    /// * `symbol` - A string slice that holds the financial symbol for which the date range
    ///              is to be queried.
    ///
    /// # Returns
    ///
    /// * `Result<(chrono::DateTime<Utc>, chrono::DateTime<Utc>), ChainError>`:
    ///    - On success, returns a tuple containing the minimum and maximum timestamps
    ///      (`chrono::DateTime` objects in UTC) for the given symbol.
    ///    - On failure, returns a descriptive `String` error message.
    ///
    /// # Errors
    ///
    /// This function can return an error in the following cases:
    /// * If the Tokio runtime fails to initialize.
    /// * If a connection to the ClickHouse database cannot be obtained.
    /// * If the ClickHouse query execution fails.
    /// * If the result set returned by ClickHouse is empty or does not contain valid data
    ///   for the provided symbol.
    /// * If there are issues converting the database result to a `chrono::DateTime`.
    ///
    /// # Implementation Details
    ///
    /// * The function utilizes a Tokio runtime to perform asynchronous operations.
    /// * The ClickHouse database is queried for the minimum and maximum timestamps of a
    ///   specified symbol from the `ohlcv` table.
    /// * The query result is processed and converted to `chrono::DateTime` objects.
    /// * If no data is found for the symbol, an error indicating the absence of a date
    ///   range is returned.
    ///
    /// # Dependencies
    ///
    /// * `tokio`: Used for asynchronous execution.
    /// * `chrono`: Provides date and time handling.
    /// * `clickhouse-rs`: A client library for interacting with ClickHouse databases.
    ///
    /// # Note
    ///
    /// Ensure the `symbol` passed as an argument exists in the `ohlcv` table of the
    /// ClickHouse database to avoid unexpected errors.
    async fn get_date_range_for_symbol(
        &self,
        symbol: &str,
    ) -> Result<(chrono::DateTime<Utc>, chrono::DateTime<Utc>), ChainError> {
        let mut conn = self.client.pool.get_handle().await.map_err(|e| {
            ChainError::ClickHouseError(format!("Failed to get ClickHouse connection: {}", e))
        })?;

        let query = format!(
            "SELECT 
                toInt64(toUnixTimestamp(min(timestamp))) as min_date, 
                toInt64(toUnixTimestamp(max(timestamp))) as max_date 
            FROM ohlcv 
            WHERE symbol = '{}'",
            symbol
        );

        let block = conn.query(query).fetch_all().await.map_err(|e| {
            ChainError::ClickHouseError(format!("Failed to execute ClickHouse query: {}", e))
        })?;

        if let Some(row) = block.rows().next() {
            let min_date = row_to_datetime(&row, "min_date")?;
            let max_date = row_to_datetime(&row, "max_date")?;

            Ok((min_date, max_date))
        } else {
            Err(ChainError::ClickHouseError(format!(
                "No date range found for symbol: {}",
                symbol
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use optionstratlib::utils::TimeFrame;
    use optionstratlib::{Positive, pos};
    use std::cell::RefCell;
    use std::sync::Arc;

    #[derive(Clone)]
    struct TestClickHouseClient {
        prices: RefCell<Vec<Positive>>,
        symbols: RefCell<Vec<String>>,
        min_date: chrono::DateTime<Utc>,
        max_date: chrono::DateTime<Utc>,
    }

    impl TestClickHouseClient {
        fn new() -> Self {
            Self {
                prices: RefCell::new(Vec::new()),
                symbols: RefCell::new(Vec::new()),
                min_date: Utc.timestamp_opt(1609459200, 0).unwrap(), // 2021-01-01
                max_date: Utc.timestamp_opt(1640995200, 0).unwrap(), // 2022-01-01
            }
        }

        fn with_prices(mut self, prices: Vec<Positive>) -> Self {
            *self.prices.borrow_mut() = prices;
            self
        }

        fn with_symbols(mut self, symbols: Vec<String>) -> Self {
            *self.symbols.borrow_mut() = symbols;
            self
        }

        fn with_date_range(
            mut self,
            min_date: chrono::DateTime<Utc>,
            max_date: chrono::DateTime<Utc>,
        ) -> Self {
            self.min_date = min_date;
            self.max_date = max_date;
            self
        }

        async fn fetch_historical_prices(
            &self,
            _symbol: &str,
            _timeframe: &TimeFrame,
            _start_date: &chrono::DateTime<Utc>,
            _end_date: &chrono::DateTime<Utc>,
        ) -> Result<Vec<Positive>, ChainError> {
            Ok(self.prices.borrow().clone())
        }

        // Métodos para simular comportamiento del pool y consultas
        fn list_symbols(&self) -> Result<Vec<String>, ChainError> {
            Ok(self.symbols.borrow().clone())
        }

        fn get_date_range(
            &self,
        ) -> Result<(chrono::DateTime<Utc>, chrono::DateTime<Utc>), ChainError> {
            Ok((self.min_date, self.max_date))
        }
    }

    // ClickHouseHistoricalRepository que se adapta a nuestro TestClickHouseClient
    struct TestHistoricalRepository {
        client: Arc<TestClickHouseClient>,
    }

    impl TestHistoricalRepository {
        fn new(client: Arc<TestClickHouseClient>) -> Self {
            Self { client }
        }

        // Implementación directa de métodos relevantes para pruebas
        fn get_historical_prices(
            &self,
            symbol: &str,
            timeframe: &TimeFrame,
            start_date: &chrono::DateTime<Utc>,
            end_date: &chrono::DateTime<Utc>,
        ) -> Result<Vec<Positive>, ChainError> {
            // Crea un runtime de tokio para tests
            let runtime = tokio::runtime::Runtime::new()
                .map_err(|e| format!("Failed to create Tokio runtime: {}", e))?;

            // Ejecuta la operación asíncrona
            runtime.block_on(async {
                self.client
                    .fetch_historical_prices(symbol, timeframe, start_date, end_date)
                    .await
            })
        }

        fn list_available_symbols(&self) -> Result<Vec<String>, ChainError> {
            self.client.list_symbols()
        }

        fn get_date_range_for_symbol(
            &self,
            _symbol: &str,
        ) -> Result<(chrono::DateTime<Utc>, chrono::DateTime<Utc>), ChainError> {
            self.client.get_date_range()
        }
    }

    #[test]
    fn test_get_historical_prices() {
        // Arrange
        let start_date = Utc.timestamp_opt(1609459200, 0).unwrap(); // 2021-01-01
        let end_date = Utc.timestamp_opt(1640995200, 0).unwrap(); // 2022-01-01
        let symbol = "AAPL";
        let timeframe = TimeFrame::Day;

        let expected_prices = vec![pos!(150.25), pos!(152.50), pos!(151.75)];

        let test_client = TestClickHouseClient::new().with_prices(expected_prices.clone());

        let repo = TestHistoricalRepository::new(Arc::new(test_client));

        // Act
        let result = repo.get_historical_prices(symbol, &timeframe, &start_date, &end_date);

        // Assert
        assert!(result.is_ok(), "get_historical_prices debe retornar Ok");
        let prices = result.unwrap();
        assert_eq!(prices, expected_prices);
        assert_eq!(prices.len(), 3);
        assert_eq!(prices[0], pos!(150.25));
    }

    #[test]
    fn test_list_available_symbols() {
        // Arrange
        let expected_symbols = vec!["AAPL".to_string(), "GOOG".to_string(), "MSFT".to_string()];

        let test_client = TestClickHouseClient::new().with_symbols(expected_symbols.clone());

        let repo = TestHistoricalRepository::new(Arc::new(test_client));

        // Act
        let result = repo.list_available_symbols();

        // Assert
        assert!(result.is_ok(), "list_available_symbols debe retornar Ok");
        let symbols = result.unwrap();
        assert_eq!(symbols, expected_symbols);
        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[0], "AAPL");
    }

    #[test]
    fn test_get_date_range_for_symbol() {
        // Arrange
        let symbol = "AAPL";
        let expected_min_date = Utc.timestamp_opt(1609459200, 0).unwrap(); // 2021-01-01
        let expected_max_date = Utc.timestamp_opt(1640995200, 0).unwrap(); // 2022-01-01

        let test_client =
            TestClickHouseClient::new().with_date_range(expected_min_date, expected_max_date);

        let repo = TestHistoricalRepository::new(Arc::new(test_client));

        // Act
        let result = repo.get_date_range_for_symbol(symbol);

        // Assert
        assert!(result.is_ok(), "get_date_range_for_symbol debe retornar Ok");
        let (min_date, max_date) = result.unwrap();
        assert_eq!(min_date, expected_min_date);
        assert_eq!(max_date, expected_max_date);
    }
}
