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
        end_date: &DateTime<Utc>,
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
