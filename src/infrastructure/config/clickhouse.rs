use std::env;

/// Configuration for the ClickHouse client
#[derive(Debug, Clone)]
pub struct ClickHouseConfig {
    /// Host address of the ClickHouse server
    pub host: String,
    /// Port of the ClickHouse server
    pub port: u16,
    /// Username for authentication
    pub username: String,
    /// Password for authentication
    pub password: String,
    /// Database name
    pub database: String,
    /// Connection timeout in seconds
    pub timeout: u64,
}

impl Default for ClickHouseConfig {
    fn default() -> Self {
        let port = env::var("CLICKHOUSE_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(9000);

        Self {
            host: env::var("CLICKHOUSE_HOST").unwrap_or_else(|_| "localhost".to_string()),
            port,
            username: env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "admin".to_string()),
            password: env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "password".to_string()),
            database: env::var("CLICKHOUSE_DB").unwrap_or_else(|_| "default".to_string()),
            timeout: 30,
        }
    }
}