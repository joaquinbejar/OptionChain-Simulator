use crate::utils::ChainError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use optionstratlib::Positive;
use optionstratlib::utils::TimeFrame;

/// A trait that defines the interface for interacting with a repository of historical financial data.
/// Provides methods to fetch historical prices, get a list of available symbols, and determine the
/// date range for a specific symbol.
#[async_trait]
pub trait HistoricalDataRepository: Send + Sync {
    ///
    /// Retrieves historical price data for a given financial instrument within a specified time range.
    ///
    /// # Parameters
    ///
    /// * `symbol` - A string slice representing the ticker symbol or identifier of the financial instrument.
    /// * `timeframe` - A reference to a `TimeFrame` enum indicating the desired time intervals (e.g., daily, hourly).
    /// * `start_date` - A reference to a `DateTime<Utc>` object specifying the starting date and time of the historical data query (inclusive).
    /// * `end_date` - A reference to a `DateTime<Utc>` object specifying the ending date and time of the historical data query (inclusive).
    ///
    /// # Returns
    ///
    /// * `Ok(Vec<Positive>)` - A vector of `Positive` values representing the historical prices for the
    ///   specified `symbol` and `timeframe` within the given date range.
    /// * `Err(String)` - Returns an error message if the query fails, such as due to invalid input parameters,
    ///   network issues, or data unavailability.
    ///
    /// # Errors
    ///
    /// This function returns an `Err` if:
    /// - The `symbol` is invalid or not recognized.
    /// - The `start_date` is after the `end_date`.
    /// - The date range exceeds limitations imposed by the source of the data.
    /// - There is a network error or the data source is unavailable.
    ///
    /// # Notes
    ///
    /// Ensure the `symbol` and `timeframe` are supported by the underlying data provider. The
    /// function assumes that both `start_date` and `end_date` are in UTC.
    async fn get_historical_prices(
        &self,
        symbol: &str,
        timeframe: &TimeFrame,
        start_date: &DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<Positive>, ChainError>;

    /// Retrieves a list of all available symbols that can be accessed.
    ///
    /// This function returns a list of symbols as strings, which might
    /// represent financial instruments, assets, or other entities depending
    /// on the context of use. The specific semantics of the symbols are
    /// determined by the system where this function is implemented.
    ///
    /// # Returns
    /// - `Ok(Vec<String>)`: A vector of strings representing the available symbols.
    /// - `Err(String)`: An error message indicating why the operation failed.
    ///
    /// # Errors
    /// If the retrieval process encounters an issue (e.g., network issues,
    /// unavailable data, or internal errors), this method will return an
    /// appropriate error message encapsulated in a `Result::Err`.
    ///
    async fn list_available_symbols(&self) -> Result<Vec<String>, ChainError>;

    ///
    /// Retrieves the date range (start and end dates) associated with a given symbol.
    ///
    /// # Arguments
    ///
    /// * `symbol` - A string slice representing the symbol for which the date range is to be retrieved.
    ///
    /// # Returns
    ///
    /// * `Ok((DateTime<Utc>, DateTime<Utc>))` - A tuple containing the start and end dates
    ///   (both inclusive) in UTC format if the operation is successful.
    /// * `Err(String)` - A string containing an error message if the symbol is invalid
    ///   or if the date range cannot be retrieved.
    ///
    /// # Errors
    ///
    /// This method returns an error in the following cases:
    /// * If the provided symbol is not found or is invalid.
    /// * If there is an issue retrieving the date range for the symbol.
    ///
    async fn get_date_range_for_symbol(
        &self,
        symbol: &str,
    ) -> Result<(DateTime<Utc>, DateTime<Utc>), ChainError>;
}

#[cfg(test)]
mod tests {
    use crate::infrastructure::HistoricalDataRepository;
    use crate::utils::ChainError;
    use async_trait::async_trait;
    use chrono::{DateTime, TimeZone, Utc};
    use optionstratlib::Positive;
    use optionstratlib::pos;
    use optionstratlib::utils::TimeFrame;
    use std::collections::HashMap;
    use std::sync::RwLock;

    type HashMapStringDateTime = HashMap<String, (DateTime<Utc>, DateTime<Utc>)>;
    /// Mock implementation of HistoricalDataRepository for testing
    struct MockHistoricalRepository {
        /// Stored prices for each symbol, timeframe, and date
        prices: RwLock<HashMap<String, Vec<Positive>>>,
        /// Available symbols
        symbols: RwLock<Vec<String>>,
        /// Date ranges for each symbol
        date_ranges: RwLock<HashMapStringDateTime>,
        /// Controls if methods should return errors
        should_fail: bool,
    }

    impl MockHistoricalRepository {
        /// Creates a new mock repository
        fn new() -> Self {
            Self {
                prices: RwLock::new(HashMap::new()),
                symbols: RwLock::new(Vec::new()),
                date_ranges: RwLock::new(HashMap::new()),
                should_fail: false,
            }
        }

        /// Creates a mock repository that returns errors
        fn with_errors() -> Self {
            let mut repo = Self::new();
            repo.should_fail = true;
            repo
        }

        /// Adds a symbol with prices and date range
        fn add_symbol(
            &self,
            symbol: &str,
            prices: Vec<Positive>,
            start_date: DateTime<Utc>,
            end_date: DateTime<Utc>,
        ) {
            self.symbols.write().unwrap().push(symbol.to_string());
            self.prices
                .write()
                .unwrap()
                .insert(symbol.to_string(), prices);
            self.date_ranges
                .write()
                .unwrap()
                .insert(symbol.to_string(), (start_date, end_date));
        }
    }

    #[async_trait]
    impl HistoricalDataRepository for MockHistoricalRepository {
        async fn get_historical_prices(
            &self,
            symbol: &str,
            _timeframe: &TimeFrame,
            _start_date: &DateTime<Utc>,
            limit: usize,
        ) -> Result<Vec<Positive>, ChainError> {
            if self.should_fail {
                return Err(ChainError::ClickHouseError(
                    "Forced error in mock".to_string(),
                ));
            }

            // Get prices for the symbol
            let prices = self.prices.read().unwrap();
            let symbol_prices = prices
                .get(symbol)
                .cloned()
                .ok_or_else(|| ChainError::NotFound(format!("No prices for symbol: {}", symbol)))?;

            // Return limited prices
            let limit = std::cmp::min(limit, symbol_prices.len());
            Ok(symbol_prices.into_iter().take(limit).collect())
        }

        async fn list_available_symbols(&self) -> Result<Vec<String>, ChainError> {
            if self.should_fail {
                return Err(ChainError::ClickHouseError(
                    "Forced error in mock".to_string(),
                ));
            }

            let symbols = self.symbols.read().unwrap();
            Ok(symbols.clone())
        }

        async fn get_date_range_for_symbol(
            &self,
            symbol: &str,
        ) -> Result<(DateTime<Utc>, DateTime<Utc>), ChainError> {
            if self.should_fail {
                return Err(ChainError::ClickHouseError(
                    "Forced error in mock".to_string(),
                ));
            }

            let date_ranges = self.date_ranges.read().unwrap();
            date_ranges.get(symbol).cloned().ok_or_else(|| {
                ChainError::NotFound(format!("No date range for symbol: {}", symbol))
            })
        }
    }

    // Helper function to create a test repository with sample data
    fn create_test_repository() -> MockHistoricalRepository {
        let repo = MockHistoricalRepository::new();

        // Add AAPL data
        let aapl_prices = vec![
            pos!(150.0),
            pos!(151.2),
            pos!(149.8),
            pos!(152.5),
            pos!(153.1),
        ];
        let aapl_start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let aapl_end = Utc.with_ymd_and_hms(2023, 1, 5, 0, 0, 0).unwrap();
        repo.add_symbol("AAPL", aapl_prices, aapl_start, aapl_end);

        // Add MSFT data
        let msft_prices = vec![
            pos!(250.0),
            pos!(252.3),
            pos!(251.8),
            pos!(253.5),
            pos!(254.2),
        ];
        let msft_start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let msft_end = Utc.with_ymd_and_hms(2023, 1, 5, 0, 0, 0).unwrap();
        repo.add_symbol("MSFT", msft_prices, msft_start, msft_end);

        repo
    }

    #[tokio::test]
    async fn test_get_historical_prices_success() {
        let repo = create_test_repository();
        let symbol = "AAPL";
        let timeframe = TimeFrame::Day;
        let start_date = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let limit = 3;

        let result = repo
            .get_historical_prices(symbol, &timeframe, &start_date, limit)
            .await;

        assert!(result.is_ok(), "Expected successful result");
        let prices = result.unwrap();
        assert_eq!(prices.len(), 3, "Expected 3 price points");
        assert_eq!(prices[0], pos!(150.0));
        assert_eq!(prices[1], pos!(151.2));
        assert_eq!(prices[2], pos!(149.8));
    }

    #[tokio::test]
    async fn test_get_historical_prices_unknown_symbol() {
        let repo = create_test_repository();
        let symbol = "UNKNOWN";
        let timeframe = TimeFrame::Day;
        let start_date = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let limit = 5;

        let result = repo
            .get_historical_prices(symbol, &timeframe, &start_date, limit)
            .await;

        assert!(result.is_err(), "Expected error for unknown symbol");
        match result {
            Err(ChainError::NotFound(msg)) => {
                assert!(
                    msg.contains("UNKNOWN"),
                    "Error message should mention the symbol"
                );
            }
            _ => panic!("Expected NotFound error"),
        }
    }

    #[tokio::test]
    async fn test_get_historical_prices_with_error() {
        let repo = MockHistoricalRepository::with_errors();
        let symbol = "AAPL";
        let timeframe = TimeFrame::Day;
        let start_date = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let limit = 5;

        let result = repo
            .get_historical_prices(symbol, &timeframe, &start_date, limit)
            .await;

        assert!(result.is_err(), "Expected error when repository fails");
        match result {
            Err(ChainError::ClickHouseError(_)) => {
                // This is the expected error type
            }
            _ => panic!("Expected ClickHouseError"),
        }
    }

    #[tokio::test]
    async fn test_get_historical_prices_limit_handling() {
        let repo = create_test_repository();
        let symbol = "AAPL";
        let timeframe = TimeFrame::Day;
        let start_date = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

        // Test with limit greater than available data
        let large_limit = 10;
        let result = repo
            .get_historical_prices(symbol, &timeframe, &start_date, large_limit)
            .await;

        assert!(result.is_ok(), "Expected successful result");
        let prices = result.unwrap();
        assert_eq!(
            prices.len(),
            5,
            "Expected all 5 price points when limit exceeds available data"
        );

        // Test with zero limit
        let zero_limit = 0;
        let result = repo
            .get_historical_prices(symbol, &timeframe, &start_date, zero_limit)
            .await;

        assert!(result.is_ok(), "Expected successful result");
        let prices = result.unwrap();
        assert_eq!(prices.len(), 0, "Expected empty result with zero limit");
    }

    #[tokio::test]
    async fn test_list_available_symbols_success() {
        let repo = create_test_repository();

        let result = repo.list_available_symbols().await;

        assert!(result.is_ok(), "Expected successful result");
        let symbols = result.unwrap();
        assert_eq!(symbols.len(), 2, "Expected 2 symbols");
        assert!(
            symbols.contains(&"AAPL".to_string()),
            "Expected AAPL symbol"
        );
        assert!(
            symbols.contains(&"MSFT".to_string()),
            "Expected MSFT symbol"
        );
    }

    #[tokio::test]
    async fn test_list_available_symbols_with_error() {
        let repo = MockHistoricalRepository::with_errors();

        let result = repo.list_available_symbols().await;

        assert!(result.is_err(), "Expected error when repository fails");
        match result {
            Err(ChainError::ClickHouseError(_)) => {
                // This is the expected error type
            }
            _ => panic!("Expected ClickHouseError"),
        }
    }

    #[tokio::test]
    async fn test_list_available_symbols_empty() {
        let repo = MockHistoricalRepository::new(); // No symbols added

        let result = repo.list_available_symbols().await;

        assert!(result.is_ok(), "Expected successful result");
        let symbols = result.unwrap();
        assert_eq!(symbols.len(), 0, "Expected no symbols");
    }

    #[tokio::test]
    async fn test_get_date_range_for_symbol_success() {
        let repo = create_test_repository();
        let symbol = "AAPL";

        let result = repo.get_date_range_for_symbol(symbol).await;

        assert!(result.is_ok(), "Expected successful result");
        let (start_date, end_date) = result.unwrap();

        let expected_start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let expected_end = Utc.with_ymd_and_hms(2023, 1, 5, 0, 0, 0).unwrap();

        assert_eq!(start_date, expected_start, "Start date doesn't match");
        assert_eq!(end_date, expected_end, "End date doesn't match");
    }

    #[tokio::test]
    async fn test_get_date_range_for_symbol_unknown_symbol() {
        let repo = create_test_repository();
        let symbol = "UNKNOWN";

        let result = repo.get_date_range_for_symbol(symbol).await;

        assert!(result.is_err(), "Expected error for unknown symbol");
        match result {
            Err(ChainError::NotFound(msg)) => {
                assert!(
                    msg.contains("UNKNOWN"),
                    "Error message should mention the symbol"
                );
            }
            _ => panic!("Expected NotFound error"),
        }
    }

    #[tokio::test]
    async fn test_get_date_range_for_symbol_with_error() {
        let repo = MockHistoricalRepository::with_errors();
        let symbol = "AAPL";

        let result = repo.get_date_range_for_symbol(symbol).await;

        assert!(result.is_err(), "Expected error when repository fails");
        match result {
            Err(ChainError::ClickHouseError(_)) => {
                // This is the expected error type
            }
            _ => panic!("Expected ClickHouseError"),
        }
    }

    #[tokio::test]
    async fn test_integration_flow() {
        // This test verifies a typical flow of using the repository
        let repo = create_test_repository();

        // 1. List available symbols
        let symbols_result = repo.list_available_symbols().await;
        assert!(symbols_result.is_ok(), "Failed to list symbols");
        let symbols = symbols_result.unwrap();

        // 2. For the first symbol, get date range
        let first_symbol = &symbols[0];
        let date_range_result = repo.get_date_range_for_symbol(first_symbol).await;
        assert!(date_range_result.is_ok(), "Failed to get date range");
        let (start_date, _) = date_range_result.unwrap();

        // 3. Get historical prices for that symbol and date
        let timeframe = TimeFrame::Day;
        let prices_result = repo
            .get_historical_prices(first_symbol, &timeframe, &start_date, 5)
            .await;
        assert!(prices_result.is_ok(), "Failed to get historical prices");
        let prices = prices_result.unwrap();
        assert!(!prices.is_empty(), "Should have retrieved prices");
    }
}
