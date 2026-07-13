use crate::utils::ChainError;
use chrono::{DateTime, Duration, Utc};
use optionstratlib::utils::TimeFrame;
use rand::{Rng, RngExt};

/// Maximum number of characters allowed in a symbol.
const MAX_SYMBOL_LEN: usize = 32;

/// Validates an externally supplied symbol as a defense-in-depth measure on top
/// of query parameter binding.
///
/// The allowed pattern is `^[A-Za-z0-9._-]{1,32}$`, i.e. between 1 and 32 ASCII
/// alphanumeric characters, dots, underscores or hyphens. The check is
/// implemented with plain character inspection to avoid pulling in a regex
/// dependency, and it rejects any non-ASCII input (e.g. look-alike Unicode
/// characters) as well as empty or overly long values.
///
/// # Errors
///
/// Returns [`ChainError::ClickHouseError`] when the symbol is empty, longer than
/// 32 characters, or contains a character outside the allowed set.
pub fn validate_symbol(symbol: &str) -> Result<(), ChainError> {
    let len = symbol.chars().count();
    let is_valid = (1..=MAX_SYMBOL_LEN).contains(&len)
        && symbol
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));

    if is_valid {
        Ok(())
    } else {
        Err(ChainError::ClickHouseError(format!(
            "invalid symbol: {symbol}"
        )))
    }
}

/// Calculates the required duration based on timeframe and steps
pub fn calculate_required_duration(timeframe: &TimeFrame, steps: usize) -> Duration {
    match timeframe {
        TimeFrame::Microsecond => Duration::microseconds(steps as i64),
        TimeFrame::Millisecond => Duration::milliseconds(steps as i64),
        TimeFrame::Second => Duration::seconds(steps as i64),
        TimeFrame::Minute => Duration::minutes(steps as i64),
        TimeFrame::Hour => Duration::hours(steps as i64),
        TimeFrame::Day => Duration::days(steps as i64),
        TimeFrame::Week => Duration::weeks(steps as i64),
        TimeFrame::Month => Duration::days(steps as i64 * 30), // Approximation
        TimeFrame::Quarter => Duration::days(steps as i64 * 90), // Approximation
        TimeFrame::Year => Duration::days(steps as i64 * 365),
        TimeFrame::Custom(p) => Duration::days(p.to_i64()),
    }
}

/// Selects a random date between min_date and max_date ensuring enough data for steps
pub fn select_random_date<R: Rng>(
    rng: &mut R,
    min_date: DateTime<Utc>,
    max_date: DateTime<Utc>,
    timeframe: &TimeFrame,
    steps: usize,
) -> Result<DateTime<Utc>, ChainError> {
    // Calculate the minimum duration required
    let required_duration = calculate_required_duration(timeframe, steps);

    // Check if the range is sufficient
    let available_range = max_date - min_date;
    if available_range < required_duration {
        return Err(ChainError::NotEnoughData(format!(
            "Date range too small. Required: {} days, Available: {} days",
            required_duration.num_days(),
            available_range.num_days()
        )));
    }

    // Calculate the latest possible start date
    let latest_possible_start = max_date - required_duration;

    // Select a random date between min_date and latest_possible_start
    if latest_possible_start <= min_date {
        // If they're equal, we can only start at min_date
        Ok(min_date)
    } else {
        let possible_range = latest_possible_start - min_date;
        let random_days = rng.random_range(0..=possible_range.num_days());
        Ok(min_date + Duration::days(random_days))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone, Utc};
    use mockall::predicate::*;
    use mockall::*;

    mock! {
        pub Row<'a> {
            fn get<T: 'static>(&self, field_name: &str) -> Result<T, ChainError>;
        }
    }

    #[test]
    fn test_validate_symbol_accepts_valid_symbols() {
        for symbol in ["AAPL", "BRK.B", "BTC-USD", "spx_w", "A", &"X".repeat(32)] {
            assert!(
                validate_symbol(symbol).is_ok(),
                "expected {symbol} to be accepted"
            );
        }
    }

    #[test]
    fn test_validate_symbol_rejects_injection_and_unicode() {
        let cases = [
            "A'\u{2014}",                // A'— (single quote + em dash)
            "A' OR '1'='1",              // classic SQL injection payload
            "A\"; DROP TABLE ohlcv; --", // stacked statement attempt
            "A\u{0410}",                 // trailing Cyrillic 'А' look-alike
            "\u{0410}",                  // lone Cyrillic 'А'
            &"X".repeat(33),             // 33 chars, one over the limit
            "",                          // empty string
            "AA PL",                     // embedded space
        ];

        for symbol in cases {
            let result = validate_symbol(symbol);
            assert!(
                matches!(result, Err(ChainError::ClickHouseError(_))),
                "expected {symbol:?} to be rejected"
            );
        }
    }

    fn row_to_datetime_test(row: &MockRow, field_name: &str) -> Result<DateTime<Utc>, ChainError> {
        let timestamp_seconds: i64 = row.get(field_name)?;

        DateTime::<Utc>::from_timestamp(timestamp_seconds, 0).ok_or_else(|| {
            ChainError::ClickHouseError(format!("Invalid timestamp value: {}", timestamp_seconds))
        })
    }

    #[test]
    fn test_row_to_datetime_valid_timestamp() {
        // Unix timestamp for 2023-05-15 14:10:00 UTC
        let timestamp_secs = 1684159800;
        let expected = Utc.timestamp_opt(timestamp_secs, 0).unwrap();

        let mut mock_row = MockRow::new();
        mock_row
            .expect_get::<i64>()
            .with(eq("timestamp"))
            .returning(|_| Ok(1684159800));

        let result = row_to_datetime_test(&mock_row, "timestamp");
        assert!(result.is_ok(), "Should convert valid timestamp");

        let datetime = result.unwrap();
        assert_eq!(datetime, expected);
        assert_eq!(datetime.to_rfc3339(), "2023-05-15T14:10:00+00:00");
    }

    #[test]
    fn test_row_to_datetime_field_not_found() {
        let mut mock_row = MockRow::new();
        mock_row
            .expect_get::<i64>()
            .with(eq("timestamp"))
            .returning(|_| {
                Err(ChainError::ClickHouseError(
                    "Column not found: timestamp".to_string(),
                ))
            });

        let result = row_to_datetime_test(&mock_row, "timestamp");
        assert!(result.is_err(), "Should fail when field not found");
    }

    #[test]
    fn test_row_to_datetime_zero_timestamp() {
        let mut mock_row = MockRow::new();
        mock_row
            .expect_get::<i64>()
            .with(eq("timestamp"))
            .returning(|_| Ok(0));

        let result = row_to_datetime_test(&mock_row, "timestamp");
        assert!(result.is_ok(), "Should convert zero timestamp (Unix epoch)");

        let datetime = result.unwrap();
        assert_eq!(datetime.to_rfc3339(), "1970-01-01T00:00:00+00:00");
    }

    #[test]
    fn test_row_to_datetime_future_timestamp() {
        let timestamp_secs = 4102444800; // 2100-01-01 00:00:00 UTC

        let mut mock_row = MockRow::new();
        mock_row
            .expect_get::<i64>()
            .with(eq("timestamp"))
            .returning(move |_| Ok(timestamp_secs));

        let result = row_to_datetime_test(&mock_row, "timestamp");
        assert!(result.is_ok(), "Should convert future timestamp");

        let datetime = result.unwrap();
        assert_eq!(datetime.year(), 2100);
        assert_eq!(datetime.month(), 1);
        assert_eq!(datetime.day(), 1);
    }
}
