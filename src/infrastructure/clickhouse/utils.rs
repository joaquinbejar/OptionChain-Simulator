use chrono::{DateTime,  Utc};

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
