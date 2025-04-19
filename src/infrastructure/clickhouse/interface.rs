use chrono::Utc;
use optionstratlib::Positive;
use optionstratlib::utils::TimeFrame;
use crate::utils::ChainError;

/// A trait that defines the interface for interacting with a repository of historical financial data.
/// Provides methods to fetch historical prices, get a list of available symbols, and determine the
/// date range for a specific symbol.
pub trait HistoricalDataRepository: Send + Sync {
    ///
    /// Retrieves historical price data for a given financial instrument within a specified time range.
    ///
    /// # Parameters
    ///
    /// * `symbol` - A string slice representing the ticker symbol or identifier of the financial instrument.
    /// * `timeframe` - A reference to a `TimeFrame` enum indicating the desired time intervals (e.g., daily, hourly).
    /// * `start_date` - A reference to a `chrono::DateTime<Utc>` object specifying the starting date and time of the historical data query (inclusive).
    /// * `end_date` - A reference to a `chrono::DateTime<Utc>` object specifying the ending date and time of the historical data query (inclusive).
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
    fn get_historical_prices(
        &self,
        symbol: &str,
        timeframe: &TimeFrame,
        start_date: &chrono::DateTime<Utc>,
        end_date: &chrono::DateTime<Utc>,
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
    fn list_available_symbols(&self) -> Result<Vec<String>, ChainError>;

    ///
    /// Retrieves the date range (start and end dates) associated with a given symbol.
    ///
    /// # Arguments
    ///
    /// * `symbol` - A string slice representing the symbol for which the date range is to be retrieved.
    ///
    /// # Returns
    ///
    /// * `Ok((chrono::DateTime<Utc>, chrono::DateTime<Utc>))` - A tuple containing the start and end dates
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
    fn get_date_range_for_symbol(&self, symbol: &str) -> Result<(chrono::DateTime<Utc>, chrono::DateTime<Utc>), ChainError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use mockall::predicate::*;
    use mockall::*;
    use optionstratlib::{pos, Positive};
    use std::sync::Arc;
    
    mock! {
        pub HistoricalDataRepo {}

        impl HistoricalDataRepository for HistoricalDataRepo {
            fn get_historical_prices(
                &self,
                symbol: &str,
                timeframe: &TimeFrame,
                start_date: &chrono::DateTime<Utc>,
                end_date: &chrono::DateTime<Utc>,
            ) -> Result<Vec<Positive>, ChainError>;

            fn list_available_symbols(&self) -> Result<Vec<String>, ChainError>;

            fn get_date_range_for_symbol(
                &self,
                symbol: &str,
            ) -> Result<(chrono::DateTime<Utc>, chrono::DateTime<Utc>), ChainError>;
        }
    }


    struct MarketDataService {
        repository: Arc<dyn HistoricalDataRepository>,
    }

    impl MarketDataService {
        fn new(repository: Arc<dyn HistoricalDataRepository>) -> Self {
            Self { repository }
        }
        
        fn get_closing_prices(
            &self,
            symbol: &str,
            start_date: &chrono::DateTime<Utc>,
            end_date: &chrono::DateTime<Utc>,
        ) -> Result<Vec<Positive>, ChainError> {
            self.repository.get_historical_prices(
                symbol,
                &TimeFrame::Day,
                start_date,
                end_date
            )
        }


        fn is_symbol_available(&self, symbol: &str) -> Result<bool, ChainError> {
            let symbols = self.repository.list_available_symbols()?;
            Ok(symbols.contains(&symbol.to_string()))
        }


        fn get_data_age_days(&self, symbol: &str) -> Result<i64, ChainError> {
            let (start_date, end_date) = self.repository.get_date_range_for_symbol(symbol)?;
            let duration = end_date.signed_duration_since(start_date);
            Ok(duration.num_days())
        }
    }

    #[test]
    fn test_get_closing_prices() {
        // Arrange
        let start_date = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end_date = Utc.with_ymd_and_hms(2023, 1, 5, 0, 0, 0).unwrap();
        let symbol = "AAPL";

        let expected_prices = vec![
            pos!(150.25),
            pos!(152.50),
            pos!(151.75)
        ];

        let mut mock_repo = MockHistoricalDataRepo::new();
        mock_repo
            .expect_get_historical_prices()
            .with(
                eq(symbol),
                eq(TimeFrame::Day),
                eq(start_date),
                eq(end_date)
            )
            .returning(move |_, _, _, _| {
                Ok(vec![
                    pos!(150.25),
                    pos!(152.50),
                    pos!(151.75)
                ])
            });

        let service = MarketDataService::new(Arc::new(mock_repo));

        // Act
        let result = service.get_closing_prices(symbol, &start_date, &end_date);

        // Assert
        assert!(result.is_ok());
        let prices = result.unwrap();
        assert_eq!(prices, expected_prices);
    }

    #[test]
    fn test_is_symbol_available_true() {
        // Arrange
        let symbol = "AAPL";
        let available_symbols = vec![
            "GOOG".to_string(),
            "AAPL".to_string(),
            "MSFT".to_string()
        ];

        let mut mock_repo = MockHistoricalDataRepo::new();
        mock_repo
            .expect_list_available_symbols()
            .returning(move || {
                Ok(available_symbols.clone())
            });

        let service = MarketDataService::new(Arc::new(mock_repo));

        // Act
        let result = service.is_symbol_available(symbol);

        // Assert
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_is_symbol_available_false() {
        // Arrange
        let symbol = "NONEXISTENT";
        let available_symbols = vec![
            "GOOG".to_string(),
            "AAPL".to_string(),
            "MSFT".to_string()
        ];

        let mut mock_repo = MockHistoricalDataRepo::new();
        mock_repo
            .expect_list_available_symbols()
            .returning(move || {
                Ok(available_symbols.clone())
            });

        let service = MarketDataService::new(Arc::new(mock_repo));

        // Act
        let result = service.is_symbol_available(symbol);

        // Assert
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn test_is_symbol_available_error() {
        // Arrange
        let symbol = "AAPL";
        let error_message = "Database connection failed".to_string();

        let mut mock_repo = MockHistoricalDataRepo::new();
        mock_repo
            .expect_list_available_symbols()
            .returning(move || {
                Err(ChainError::ClickHouseError( error_message.clone()))
            });

        let service = MarketDataService::new(Arc::new(mock_repo));

        // Act
        let result = service.is_symbol_available(symbol);

        // Assert
        assert!(result.is_err());
    }

    #[test]
    fn test_get_data_age_days() {
        // Arrange
        let symbol = "AAPL";
        let start_date = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let end_date = Utc.with_ymd_and_hms(2023, 1, 10, 0, 0, 0).unwrap();
        let expected_days = 9; // Jan 10 - Jan 1 = 9 days

        let mut mock_repo = MockHistoricalDataRepo::new();
        mock_repo
            .expect_get_date_range_for_symbol()
            .with(eq(symbol))
            .returning(move |_| {
                Ok((start_date, end_date))
            });

        let service = MarketDataService::new(Arc::new(mock_repo));

        // Act
        let result = service.get_data_age_days(symbol);

        // Assert
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), expected_days);
    }

    #[test]
    fn test_get_data_age_days_error() {
        // Arrange
        let symbol = "NONEXISTENT";
        let error_message = "Symbol not found".to_string();

        let mut mock_repo = MockHistoricalDataRepo::new();
        mock_repo
            .expect_get_date_range_for_symbol()
            .with(eq(symbol))
            .returning(move |_| {
                Err(ChainError::ClickHouseError(error_message.clone()))
            });

        let service = MarketDataService::new(Arc::new(mock_repo));

        // Act
        let result = service.get_data_age_days(symbol);

        // Assert
        assert!(result.is_err());
    }
}