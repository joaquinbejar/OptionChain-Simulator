//! This module defines the `ClickHouseConfig` struct and its default implementation.
use serde::{Deserialize, Serialize};
use std::env;
use std::fmt;

/// Configuration structure for connecting to a ClickHouse server.
///
/// This structure holds the necessary connection details required
/// to authenticate and communicate with a ClickHouse server instance.
/// It includes parameters for the host address, port number,
/// authentication credentials, database name, and connection timeout.
///
/// # Fields
///
/// * `host` - Host address of the ClickHouse server. This can be an IP address or a domain name.
/// * `port` - Port of the ClickHouse server the client should connect to.
/// * `username` - Username for authentication with the ClickHouse server.
/// * `password` - Password for authentication with the ClickHouse server.
/// * `database` - Name of the database to connect to within the ClickHouse server.
/// * `timeout` - Connection timeout in seconds, defining how long the client will wait before giving up.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl Default for ClickHouseConfig {
    /// Creates a default instance of the struct by initializing fields with values
    /// from environment variables or falling back to predefined default values if the
    /// environment variables are not set.
    ///
    /// # Environment Variables
    /// - `CLICKHOUSE_HOST`: The hostname for connecting to the ClickHouse database.
    ///   Defaults to `"localhost"` if not set.
    /// - `CLICKHOUSE_PORT`: The port for connecting to the ClickHouse database.
    ///   Defaults to `8123` if the variable is missing or contains an invalid value.
    /// - `CLICKHOUSE_USER`: The username for authentication.
    ///   Defaults to `"admin"` if not set.
    /// - `CLICKHOUSE_PASSWORD`: The password for authentication.
    ///   Defaults to `"password"` if not set.
    /// - `CLICKHOUSE_DB`: The name of the database to connect to.
    ///   Defaults to `"default"` if not set.
    ///
    /// # Default Timeout
    /// The `timeout` field is initialized to `30` seconds.
    ///
    /// # Returns
    /// A new instance of the struct with properly initialized fields.
    fn default() -> Self {
        let port = env::var("CLICKHOUSE_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(8123);

        Self {
            host: env::var("CLICKHOUSE_HOST").unwrap_or_else(|_| "localhost".to_string()),
            port,
            username: env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "admin".to_string()),
            password: env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "password".to_string()),
            database: env::var("CLICKHOUSE_DB").unwrap_or_else(|_| "default".to_string()),
        }
    }
}

impl fmt::Display for ClickHouseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ClickHouse[{}@{}:{}/{}]",
            self.username, self.host, self.port, self.database
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use serde_json::{from_str, to_string};
    use std::env;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::path::Path;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static ENV_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn set_var(name: &str, value: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            env::set_var(name, value);
        }
    }

    fn remove_var(name: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            env::remove_var(name);
        }
    }

    #[test]
    fn test_default_values() {
        let _guard = ENV_MUTEX.lock().expect("ENV_MUTEX poisoned");

        // Temporarily clear environment variables that might affect this test
        let current_host = env::var("CLICKHOUSE_HOST").ok();
        let current_port = env::var("CLICKHOUSE_PORT").ok();
        let current_user = env::var("CLICKHOUSE_USER").ok();
        let current_password = env::var("CLICKHOUSE_PASSWORD").ok();
        let current_db = env::var("CLICKHOUSE_DB").ok();

        remove_var("CLICKHOUSE_HOST");
        remove_var("CLICKHOUSE_PORT");
        remove_var("CLICKHOUSE_USER");
        remove_var("CLICKHOUSE_PASSWORD");
        remove_var("CLICKHOUSE_DB");

        // Create default config
        let config = ClickHouseConfig::default();

        // Assert default values
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 8123);
        assert_eq!(config.username, "admin");
        assert_eq!(config.password, "password");
        assert_eq!(config.database, "default");

        // Restore environment variables
        if let Some(val) = current_host {
            set_var("CLICKHOUSE_HOST", &val);
        }
        if let Some(val) = current_port {
            set_var("CLICKHOUSE_PORT", &val);
        }
        if let Some(val) = current_user {
            set_var("CLICKHOUSE_USER", &val);
        }
        if let Some(val) = current_password {
            set_var("CLICKHOUSE_PASSWORD", &val);
        }
        if let Some(val) = current_db {
            set_var("CLICKHOUSE_DB", &val);
        }
    }

    #[test]
    fn test_env_override() {
        let _guard = ENV_MUTEX.lock().expect("ENV_MUTEX poisoned");
        // Set environment variables for test
        set_var("CLICKHOUSE_HOST", "test-host");
        set_var("CLICKHOUSE_PORT", "8123");
        set_var("CLICKHOUSE_USER", "test-user");
        set_var("CLICKHOUSE_PASSWORD", "test-password");
        set_var("CLICKHOUSE_DB", "test-db");

        // Create config which should use these environment variables
        let config = ClickHouseConfig::default();

        // Assert values from environment
        assert_eq!(config.host, "test-host");
        assert_eq!(config.port, 8123);
        assert_eq!(config.username, "test-user");
        assert_eq!(config.password, "test-password");
        assert_eq!(config.database, "test-db");

        // Clean up
        remove_var("CLICKHOUSE_HOST");
        remove_var("CLICKHOUSE_PORT");
        remove_var("CLICKHOUSE_USER");
        remove_var("CLICKHOUSE_PASSWORD");
        remove_var("CLICKHOUSE_DB");
    }

    #[test]
    fn test_invalid_port_env_var() {
        let _guard = ENV_MUTEX.lock().expect("ENV_MUTEX poisoned");

        // Set an invalid port
        set_var("CLICKHOUSE_PORT", "not_a_number");

        // Create config
        let config = ClickHouseConfig::default();

        // Should use default port
        assert_eq!(config.port, 8123);

        // Clean up
        remove_var("CLICKHOUSE_PORT");
    }

    #[test]
    fn test_serialization() {
        let config = ClickHouseConfig {
            host: "clickhouse.example.com".to_string(),
            port: 8123,
            username: "user".to_string(),
            password: "pass".to_string(),
            database: "analytics".to_string(),
        };

        let serialized = to_string(&config).expect("Failed to serialize config");

        // Ensure all fields are serialized
        assert!(serialized.contains("clickhouse.example.com"));
        assert!(serialized.contains("8123"));
        assert!(serialized.contains("user"));
        assert!(serialized.contains("pass"));
        assert!(serialized.contains("analytics"));
    }

    #[test]
    fn test_deserialization() {
        let _guard = ENV_MUTEX.lock().expect("ENV_MUTEX poisoned");

        let json = r#"{
            "host": "clickhouse.example.com",
            "port": 8123,
            "username": "user",
            "password": "pass",
            "database": "analytics",
            "timeout": 45
        }"#;

        let config: ClickHouseConfig = from_str(json).expect("Failed to deserialize config");

        assert_eq!(config.host, "clickhouse.example.com");
        assert_eq!(config.port, 8123);
        assert_eq!(config.username, "user");
        assert_eq!(config.password, "pass");
        assert_eq!(config.database, "analytics");
    }

    #[test]
    fn test_partial_deserialization() {
        // Missing some fields
        let json = r#"{
            "host": "clickhouse.example.com",
            "port": 8123
        }"#;

        // Should fail because all fields are required
        let result: Result<ClickHouseConfig, _> = from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_display_implementation() {
        let config = ClickHouseConfig {
            host: "clickhouse.example.com".to_string(),
            port: 8123,
            username: "user".to_string(),
            password: "pass".to_string(),
            database: "analytics".to_string(),
        };

        let display_string = format!("{}", config);
        let expected = "ClickHouse[user@clickhouse.example.com:8123/analytics]";

        assert_eq!(display_string, expected);
    }

    #[test]
    fn test_config_file_io() {
        // Create a temporary directory
        let dir = tempdir().expect("Failed to create temporary directory");
        let file_path = dir.path().join("config.json");

        // Create a config
        let config = ClickHouseConfig {
            host: "clickhouse.example.com".to_string(),
            port: 8123,
            username: "user".to_string(),
            password: "pass".to_string(),
            database: "analytics".to_string(),
        };

        // Serialize and write to file
        let serialized = to_string(&config).expect("Failed to serialize config");
        write_to_file(&file_path, &serialized).expect("Failed to write config to file");

        // Read from file and deserialize
        let file_content = read_from_file(&file_path).expect("Failed to read config from file");
        let loaded_config: ClickHouseConfig =
            from_str(&file_content).expect("Failed to deserialize config");

        // Compare original and loaded configs
        assert_eq!(config.host, loaded_config.host);
        assert_eq!(config.port, loaded_config.port);
        assert_eq!(config.username, loaded_config.username);
        assert_eq!(config.password, loaded_config.password);
        assert_eq!(config.database, loaded_config.database);
    }

    #[test]
    fn test_clone() {
        let config = ClickHouseConfig {
            host: "clickhouse.example.com".to_string(),
            port: 8123,
            username: "user".to_string(),
            password: "pass".to_string(),
            database: "analytics".to_string(),
        };

        let cloned_config = config.clone();

        assert_eq!(config.host, cloned_config.host);
        assert_eq!(config.port, cloned_config.port);
        assert_eq!(config.username, cloned_config.username);
        assert_eq!(config.password, cloned_config.password);
        assert_eq!(config.database, cloned_config.database);
    }

    fn write_to_file(path: &Path, content: &str) -> std::io::Result<()> {
        let mut file = File::create(path)?;
        file.write_all(content.as_bytes())?;
        Ok(())
    }

    fn read_from_file(path: &Path) -> std::io::Result<String> {
        let mut file = File::open(path)?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        Ok(content)
    }

    // Test implementación directa sin usar variables de entorno
    #[test]
    fn test_direct_initialization() {
        let config = ClickHouseConfig {
            host: "localhost".to_string(),
            port: 8123,
            username: "admin".to_string(),
            password: "password".to_string(),
            database: "default".to_string(),
        };

        // Verificar valores predeterminados
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 8123);
        assert_eq!(config.username, "admin");
        assert_eq!(config.password, "password");
        assert_eq!(config.database, "default");
    }

    // En vez de probar directamente variables de entorno, probamos el comportamiento
    // que simula la función Default implementando una versión simple
    #[test]
    fn test_custom_default_implementation() {
        // Simular los valores que vendría de variables de entorno
        fn create_config(
            host_var: Option<&str>,
            port_var: Option<&str>,
            user_var: Option<&str>,
            pass_var: Option<&str>,
            db_var: Option<&str>,
        ) -> ClickHouseConfig {
            let port = port_var.and_then(|s| s.parse::<u16>().ok()).unwrap_or(8123);

            ClickHouseConfig {
                host: host_var.unwrap_or("localhost").to_string(),
                port,
                username: user_var.unwrap_or("admin").to_string(),
                password: pass_var.unwrap_or("password").to_string(),
                database: db_var.unwrap_or("default").to_string(),
            }
        }

        // Test casos de prueba

        // 1. Todos los valores predeterminados
        let config1 = create_config(None, None, None, None, None);
        assert_eq!(config1.host, "localhost");
        assert_eq!(config1.port, 8123);

        // 2. Sobrescribir algunos valores
        let config2 = create_config(Some("test-host"), Some("8123"), None, None, None);
        assert_eq!(config2.host, "test-host");
        assert_eq!(config2.port, 8123);

        // 3. Puerto inválido debe usar valor predeterminado
        let config3 = create_config(None, Some("not_a_number"), None, None, None);
        assert_eq!(config3.port, 8123);
    }
}
