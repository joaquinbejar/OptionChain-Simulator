use std::{env, fmt};


/// Configuration for a Redis connection
#[derive(Clone, Debug)]
pub struct RedisConfig {
    /// The hostname of the Redis server
    pub host: String,
    /// The port of the Redis server
    pub port: u16,
    /// Username for authentication (optional)
    pub username: Option<String>,
    /// Password for authentication (optional)
    pub password: Option<String>,
    /// Database number to use
    pub database: u8,
    /// Connection timeout in seconds
    pub timeout: u64,
}
impl RedisConfig {
    pub(crate) fn url(&self) -> String {
        // Start building the URL
        let mut url = String::from("redis://");

        // Add credentials if either username or password is present
        if self.username.is_some() || self.password.is_some() {
            // Add username if present, otherwise an empty string
            if let Some(username) = &self.username {
                url.push_str(username);
            }

            // Add password with colon prefix if present
            if let Some(password) = &self.password {
                url.push(':');
                url.push_str(password);
            }

            // Add the @ separator after credentials
            url.push('@');
        }

        // Add host and port
        url.push_str(&self.host);
        url.push(':');
        url.push_str(&self.port.to_string());

        // Add database if not 0
        if self.database > 0 {
            url.push('/');
            url.push_str(&self.database.to_string());
        };
        
        url
    }
}

impl Default for RedisConfig {
    fn default() -> Self {
        let port = env::var("REDIS_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(6379);

        let database = env::var("REDIS_DB")
            .ok()
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(0);
        
        let username = env::var("REDIS_USER").ok();
        let password = env::var("REDIS_PASSWORD").ok();

        Self {
            host: env::var("REDIS_HOST").unwrap_or_else(|_| "localhost".to_string()),
            port,
            username,
            password,
            database,
            timeout: 30,
        }
    }
}

impl fmt::Display for RedisConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // // Start building the URL
        // let mut url = String::from("redis://");
        // 
        // // Add credentials if either username or password is present
        // if self.username.is_some() || self.password.is_some() {
        //     // Add username if present, otherwise an empty string
        //     if let Some(username) = &self.username {
        //         url.push_str(username);
        //     }
        // 
        //     // Add password with colon prefix if present
        //     if let Some(password) = &self.password {
        //         url.push(':');
        //         url.push_str(password);
        //     }
        // 
        //     // Add the @ separator after credentials
        //     url.push('@');
        // }
        // 
        // // Add host and port
        // url.push_str(&self.host);
        // url.push(':');
        // url.push_str(&self.port.to_string());
        // 
        // // Add database if not 0
        // if self.database > 0 {
        //     url.push('/');
        //     url.push_str(&self.database.to_string());
        // }
        
        let url = self.url();

        // Write the complete URL and timeout
        write!(f, "{} (timeout: {}s)", url, self.timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    // We use a mutex to ensure environment variable tests don't conflict
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
        let _guard = ENV_MUTEX.lock().unwrap();

        // Clear all relevant environment variables to test defaults
        remove_var("REDIS_HOST");
        remove_var("REDIS_PORT");
        remove_var("REDIS_USER");
        remove_var("REDIS_PASSWORD");
        remove_var("REDIS_DB");

        let config = RedisConfig::default();

        // Check default values
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 6379);
        assert_eq!(config.username, None);
        assert_eq!(config.password, None);
        assert_eq!(config.database, 0);
        assert_eq!(config.timeout, 30);
    }

    #[test]
    fn test_environment_variable_overrides() {
        let _guard = ENV_MUTEX.lock().unwrap();

        // Set environment variables
        set_var("REDIS_HOST", "redis.example.com");
        set_var("REDIS_PORT", "6380");
        set_var("REDIS_USER", "testuser");
        set_var("REDIS_PASSWORD", "testpass");
        set_var("REDIS_DB", "2");

        let config = RedisConfig::default();

        // Check values from environment variables
        assert_eq!(config.host, "redis.example.com");
        assert_eq!(config.port, 6380);
        assert_eq!(config.username, Some("testuser".to_string()));
        assert_eq!(config.password, Some("testpass".to_string()));
        assert_eq!(config.database, 2);
        // Timeout is still default as it's not configurable via env vars
        assert_eq!(config.timeout, 30);

        // Clean up
        remove_var("REDIS_HOST");
        remove_var("REDIS_PORT");
        remove_var("REDIS_USER");
        remove_var("REDIS_PASSWORD");
        remove_var("REDIS_DB");
    }

    #[test]
    fn test_invalid_port_in_env() {
        let _guard = ENV_MUTEX.lock().unwrap();

        // Set invalid port
        set_var("REDIS_PORT", "not_a_number");

        let config = RedisConfig::default();

        // Should fall back to default
        assert_eq!(config.port, 6379);

        // Clean up
        remove_var("REDIS_PORT");
    }

    #[test]
    fn test_invalid_db_in_env() {
        let _guard = ENV_MUTEX.lock().unwrap();

        // Set invalid database
        set_var("REDIS_DB", "not_a_number");

        let config = RedisConfig::default();

        // Should fall back to default
        assert_eq!(config.database, 0);

        // Clean up
        remove_var("REDIS_DB");
    }

    #[test]
    fn test_display_without_credentials() {
        let config = RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            username: None,
            password: None,
            database: 0,
            timeout: 30,
        };

        assert_eq!(format!("{}", config), "redis://localhost:6379 (timeout: 30s)");
    }

    #[test]
    fn test_display_with_username_only() {
        let config = RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            username: Some("testuser".to_string()),
            password: None,
            database: 0,
            timeout: 30,
        };

        assert_eq!(format!("{}", config), "redis://testuser@localhost:6379 (timeout: 30s)");
    }

    #[test]
    fn test_display_with_password_only() {
        let config = RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            username: None,
            password: Some("testpass".to_string()),
            database: 0,
            timeout: 30,
        };

        assert_eq!(format!("{}", config), "redis://:testpass@localhost:6379 (timeout: 30s)");
    }

    #[test]
    fn test_display_with_full_credentials() {
        let config = RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            username: Some("testuser".to_string()),
            password: Some("testpass".to_string()),
            database: 0,
            timeout: 30,
        };

        assert_eq!(format!("{}", config), "redis://testuser:testpass@localhost:6379 (timeout: 30s)");
    }

    #[test]
    fn test_display_with_non_default_database() {
        let config = RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            username: None,
            password: None,
            database: 3,
            timeout: 30,
        };

        assert_eq!(format!("{}", config), "redis://localhost:6379/3 (timeout: 30s)");
    }

    #[test]
    fn test_display_full_configuration() {
        let config = RedisConfig {
            host: "redis.example.com".to_string(),
            port: 6380,
            username: Some("admin".to_string()),
            password: Some("s3cret".to_string()),
            database: 5,
            timeout: 45,
        };

        assert_eq!(format!("{}", config), "redis://admin:s3cret@redis.example.com:6380/5 (timeout: 45s)");
    }

    #[test]
    fn test_clone() {
        let original = RedisConfig {
            host: "redis.example.com".to_string(),
            port: 6380,
            username: Some("testuser".to_string()),
            password: Some("testpass".to_string()),
            database: 2,
            timeout: 45,
        };

        let cloned = original.clone();

        assert_eq!(cloned.host, "redis.example.com");
        assert_eq!(cloned.port, 6380);
        assert_eq!(cloned.username, Some("testuser".to_string()));
        assert_eq!(cloned.password, Some("testpass".to_string()));
        assert_eq!(cloned.database, 2);
        assert_eq!(cloned.timeout, 45);
    }
}