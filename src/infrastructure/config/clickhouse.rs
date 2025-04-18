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
        Self {
            host: "localhost".to_string(),
            port: 9000,
            username: "default".to_string(),
            password: "".to_string(),
            database: "default".to_string(),
            timeout: 30,
        }
    }
}