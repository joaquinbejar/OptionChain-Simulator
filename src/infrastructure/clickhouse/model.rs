use optionstratlib::Positive;

/// OHLCV data structure representing historical financial data
#[derive(Debug, Clone)]
pub struct OHLCVData {
    /// Symbol or ticker
    pub symbol: String,
    /// Timestamp of the data point
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Opening price
    pub open: Positive,
    /// Highest price during the period
    pub high: Positive,
    /// Lowest price during the period
    pub low: Positive,
    /// Closing price
    pub close: Positive,
    /// Volume traded
    pub volume: u32,
}

/// Price types that can be extracted from OHLCV data
#[derive(Debug, Clone, Copy)]
pub enum PriceType {
    /// Opening price
    Open,
    /// Highest price
    High,
    /// Lowest price
    Low,
    /// Closing price
    Close,
    /// Typical price (high + low + close) / 3
    Typical,
}