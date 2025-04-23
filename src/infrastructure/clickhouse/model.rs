use crate::utils::ChainError;
use chrono::{DateTime, Utc};
use clickhouse::Row;
use optionstratlib::{Positive, pos};
use serde::{Deserialize, Serialize};
use std::fmt;

/// OHLCV data structure representing historical financial data
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct OHLCVData {
    /// Symbol or ticker
    pub symbol: String,
    /// Timestamp of the data point
    pub timestamp: DateTime<Utc>,
    /// Opening price
    pub open: Positive,
    /// Highest price during the period
    pub high: Positive,
    /// Lowest price during the period
    pub low: Positive,
    /// Closing price
    pub close: Positive,
    /// Volume traded
    pub volume: u64,
}

impl From<ClickHouseRow> for OHLCVData {
    fn from(value: ClickHouseRow) -> Self {
        let timestamp = match DateTime::<Utc>::from_timestamp(value.timestamp, 0).ok_or_else(|| {
            ChainError::ClickHouseError(format!("Invalid timestamp value: {}", value.timestamp))
        }) {
            Ok(timestamp) => timestamp,
            Err(err) => panic!("{}", err),
        };

        let open_pos = pos!(value.open as f64);
        let high_pos = pos!(value.high as f64);
        let low_pos = pos!(value.low as f64);
        let close_pos = pos!(value.close as f64);

        OHLCVData {
            symbol: value.symbol,
            timestamp,
            open: open_pos,
            high: high_pos,
            low: low_pos,
            close: close_pos,
            volume: value.volume,
        }
    }
}

impl fmt::Display for OHLCVData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} @ {} [O: {}, H: {}, L: {}, C: {}, V: {}]",
            self.symbol,
            self.timestamp.format("%Y-%m-%d %H:%M:%S"),
            self.open,
            self.high,
            self.low,
            self.close,
            self.volume
        )
    }
}

/// Price types that can be extracted from OHLCV data
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
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

impl fmt::Display for PriceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PriceType::Open => write!(f, "Open"),
            PriceType::High => write!(f, "High"),
            PriceType::Low => write!(f, "Low"),
            PriceType::Close => write!(f, "Close"),
            PriceType::Typical => write!(f, "Typical"),
        }
    }
}

/// A row structure to match the expected query results
#[derive(Debug, Clone, Row, Deserialize)]
pub struct ClickHouseRow {
    pub(crate) symbol: String,
    pub(crate) timestamp: i64,
    pub(crate) open: f32,
    pub(crate) high: f32,
    pub(crate) low: f32,
    pub(crate) close: f32,
    pub(crate) volume: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use optionstratlib::pos;
    use serde_json::{from_str, to_string};

    #[test]
    fn test_ohlcv_data_serialization() {
        let sample_data = OHLCVData {
            symbol: "AAPL".to_string(),
            timestamp: Utc.with_ymd_and_hms(2023, 5, 15, 14, 30, 0).unwrap(),
            open: pos!(150.25),
            high: pos!(152.75),
            low: pos!(149.50),
            close: pos!(151.80),
            volume: 1_000_000,
        };

        // Serialization
        let serialized = to_string(&sample_data).expect("Failed to serialize OHLCVData");

        // Ensure serialized contains expected fields
        assert!(serialized.contains("AAPL"));
        assert!(serialized.contains("2023-05-15T14:30:00Z"));
        assert!(serialized.contains("150.25"));
        assert!(serialized.contains("152.75"));
        assert!(serialized.contains("149.5"));
        assert!(serialized.contains("151.8"));
        assert!(serialized.contains("1000000"));

        // Deserialization
        let deserialized: OHLCVData =
            from_str(&serialized).expect("Failed to deserialize OHLCVData");

        // Verify equality
        assert_eq!(deserialized, sample_data);
    }

    #[test]
    fn test_price_type_serialization() {
        // Test all price types
        let price_types = vec![
            PriceType::Open,
            PriceType::High,
            PriceType::Low,
            PriceType::Close,
            PriceType::Typical,
        ];

        for price_type in price_types {
            // Serialization
            let serialized = to_string(&price_type).expect("Failed to serialize PriceType");

            // Deserialization
            let deserialized: PriceType =
                from_str(&serialized).expect("Failed to deserialize PriceType");

            // Verify equality
            assert_eq!(deserialized, price_type);
        }
    }

    #[test]
    fn test_ohlcv_data_display_format() {
        let sample_data = OHLCVData {
            symbol: "AAPL".to_string(),
            timestamp: Utc.with_ymd_and_hms(2023, 5, 15, 14, 30, 0).unwrap(),
            open: pos!(150.25),
            high: pos!(152.75),
            low: pos!(149.50),
            close: pos!(151.80),
            volume: 1_000_000,
        };

        let displayed = format!("{}", sample_data);
        let expected =
            "AAPL @ 2023-05-15 14:30:00 [O: 150.25, H: 152.75, L: 149.5, C: 151.8, V: 1000000]";

        assert_eq!(displayed, expected);
    }

    #[test]
    fn test_price_type_display_format() {
        assert_eq!(format!("{}", PriceType::Open), "Open");
        assert_eq!(format!("{}", PriceType::High), "High");
        assert_eq!(format!("{}", PriceType::Low), "Low");
        assert_eq!(format!("{}", PriceType::Close), "Close");
        assert_eq!(format!("{}", PriceType::Typical), "Typical");
    }

    #[test]
    fn test_invalid_json_deserialization() {
        let invalid_json = r#"{"symbol": "AAPL", "timestamp": "invalid-date", "open": 150.25, "high": 152.75, "low": 149.50, "close": 151.80, "volume": 1000000}"#;

        let result: Result<OHLCVData, _> = from_str(invalid_json);
        assert!(
            result.is_err(),
            "Expected error when deserializing invalid JSON"
        );
    }

    #[test]
    fn test_missing_fields_deserialization() {
        let incomplete_json = r#"{"symbol": "AAPL", "timestamp": "2023-05-15T14:30:00Z"}"#;

        let result: Result<OHLCVData, _> = from_str(incomplete_json);
        assert!(
            result.is_err(),
            "Expected error when deserializing JSON with missing fields"
        );
    }
}
