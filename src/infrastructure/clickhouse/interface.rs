use optionstratlib::Positive;
use optionstratlib::utils::TimeFrame;

// Repository interface for historical data access
pub trait HistoricalDataRepository: Send + Sync {
    /// Fetches historical prices for a given symbol and timeframe
    fn get_historical_prices(
        &self,
        symbol: &str,
        timeframe: &TimeFrame,
        start_date: &chrono::DateTime<chrono::Utc>,
        end_date: &chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Positive>, String>;

    /// Lists all available symbols in the repository
    fn list_available_symbols(&self) -> Result<Vec<String>, String>;

    /// Gets the available date range for a given symbol
    fn get_date_range_for_symbol(&self, symbol: &str) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>), String>;
}
