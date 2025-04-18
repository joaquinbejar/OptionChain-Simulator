use chrono::{DateTime,  Utc};

/// Converts a field in a `Row` object into a `DateTime<Utc>` value.
///
/// # Arguments
///
/// * `row` - A reference to a `clickhouse_rs::types::Row` object that contains the data.
/// * `field_name` - A string slice holding the name of the field in the row whose value will be converted.
///
/// # Returns
///
/// * `Ok(DateTime<Utc>)` - The `DateTime<Utc>` representation of the field value on success.
/// * `Err(String)` - An error message if the conversion fails, such as when the field is missing,
///   has an invalid timestamp, or when accessing the field results in an error.
///
/// # Type Parameters
///
/// * `K` - Represents the column type in the `Row` object. It must implement the `clickhouse_rs::types::column::ColumnType` trait.
///
/// # Errors
///
/// Returns an error in the following cases:
/// - If the field `field_name` does not exist or cannot be retrieved from the row.
/// - If the value in the field is not a valid timestamp in seconds.
///
///
/// Note: Ensure the `clickhouse_rs` and `chrono` crates are added to your `Cargo.toml`
/// when using this function.
pub fn row_to_datetime<'a, K>(
    row: &clickhouse_rs::types::Row<'a, K>,
    field_name: &str,
) -> Result<DateTime<Utc>, String>
where
    K: clickhouse_rs::types::column::ColumnType,
{
    let timestamp_seconds: i64 = row
        .get(field_name)
        .map_err(|e| format!("Failed to get '{}' from row: {}", field_name, e))?;

    DateTime::<Utc>::from_timestamp(timestamp_seconds, 0)
        .ok_or_else(|| format!("Invalid timestamp value: {}", timestamp_seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone, Utc};
    use mockall::predicate::*;
    use mockall::*;

    mock! {
        pub Row<'a> {
            fn get<T: 'static>(&self, field_name: &str) -> Result<T, String>;
        }
    }

    fn row_to_datetime_test(row: &MockRow, field_name: &str) -> Result<DateTime<Utc>, String> {
        let timestamp_seconds: i64 = row.get(field_name)?;

        DateTime::<Utc>::from_timestamp(timestamp_seconds, 0)
            .ok_or_else(|| format!("Invalid timestamp value: {}", timestamp_seconds))
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
            .returning(|_| Err("Column not found: timestamp".to_string()));

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