use std::{env, fmt};

/// Configuration for a Redis connection
///
/// `Debug` is implemented manually (not derived) so that credentials are never
/// leaked through `{:?}` logging; both `Debug` and `Display` render the same
/// redacted form.
#[derive(Clone)]
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
    /// Response timeout in seconds, applied to every command sent over the
    /// connection manager (`REDIS_TIMEOUT`, default 30). Guards against a hung
    /// server holding an async worker indefinitely.
    pub timeout: u64,
    /// Timeout in seconds for establishing a new connection to the server
    /// (`REDIS_CONNECT_TIMEOUT`, default 5). Bounds how long a (re)connect
    /// attempt may block before the manager retries.
    pub connect_timeout: u64,
}
/// Parses a timeout environment variable in whole seconds.
///
/// The value must be a positive integer: zero would disable the bound entirely
/// (a hung server could then hold a worker forever), so `0`, non-numeric, and
/// unset values all fall back to `default` — invalid ones with a warning.
fn parse_timeout_secs(var: &str, default: u64) -> u64 {
    match env::var(var) {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(v) if v >= 1 => v,
            _ => {
                tracing::warn!(
                    "invalid {} value {:?}; must be an integer >= 1, using default {}s",
                    var,
                    raw,
                    default
                );
                default
            }
        },
        Err(_) => default,
    }
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

        let timeout = parse_timeout_secs("REDIS_TIMEOUT", 30);
        let connect_timeout = parse_timeout_secs("REDIS_CONNECT_TIMEOUT", 5);

        Self {
            host: env::var("REDIS_HOST").unwrap_or_else(|_| "localhost".to_string()),
            port,
            username,
            password,
            database,
            timeout,
            connect_timeout,
        }
    }
}

impl fmt::Display for RedisConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Build the redacted form directly from the fields: the password never
        // enters the formatted string, so no parser can be defeated by
        // delimiter-containing credentials (`/`, whitespace, `@`, ...).
        let creds = if self.username.is_some() || self.password.is_some() {
            "***@"
        } else {
            ""
        };
        write!(f, "redis://{}{}:{}", creds, self.host, self.port)?;
        if self.database > 0 {
            write!(f, "/{}", self.database)?;
        }
        write!(f, " (timeout: {}s)", self.timeout)
    }
}

impl fmt::Debug for RedisConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never leak credentials through `{:?}`; render the redacted form.
        write!(f, "{}", self)
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
        remove_var("REDIS_TIMEOUT");
        remove_var("REDIS_CONNECT_TIMEOUT");

        let config = RedisConfig::default();

        // Check default values
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 6379);
        assert_eq!(config.username, None);
        assert_eq!(config.password, None);
        assert_eq!(config.database, 0);
        assert_eq!(config.timeout, 30);
        assert_eq!(config.connect_timeout, 5);
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
        set_var("REDIS_TIMEOUT", "45");
        set_var("REDIS_CONNECT_TIMEOUT", "7");

        let config = RedisConfig::default();

        // Check values from environment variables
        assert_eq!(config.host, "redis.example.com");
        assert_eq!(config.port, 6380);
        assert_eq!(config.username, Some("testuser".to_string()));
        assert_eq!(config.password, Some("testpass".to_string()));
        assert_eq!(config.database, 2);
        // Both timeouts are now configurable via env vars.
        assert_eq!(config.timeout, 45);
        assert_eq!(config.connect_timeout, 7);

        // Clean up
        remove_var("REDIS_HOST");
        remove_var("REDIS_PORT");
        remove_var("REDIS_USER");
        remove_var("REDIS_PASSWORD");
        remove_var("REDIS_DB");
        remove_var("REDIS_TIMEOUT");
        remove_var("REDIS_CONNECT_TIMEOUT");
    }

    #[test]
    fn test_invalid_timeouts_fall_back_to_defaults() {
        let _guard = ENV_MUTEX.lock().unwrap();

        // Non-numeric timeouts must fall back to the documented defaults.
        set_var("REDIS_TIMEOUT", "not_a_number");
        set_var("REDIS_CONNECT_TIMEOUT", "also_bad");

        let config = RedisConfig::default();

        assert_eq!(config.timeout, 30);
        assert_eq!(config.connect_timeout, 5);

        remove_var("REDIS_TIMEOUT");
        remove_var("REDIS_CONNECT_TIMEOUT");
    }

    #[test]
    fn test_zero_timeouts_fall_back_to_defaults() {
        let _guard = ENV_MUTEX.lock().unwrap();

        // Zero would disable the bound entirely; it must be rejected like any
        // other invalid value.
        set_var("REDIS_TIMEOUT", "0");
        set_var("REDIS_CONNECT_TIMEOUT", "0");

        let config = RedisConfig::default();

        assert_eq!(config.timeout, 30);
        assert_eq!(config.connect_timeout, 5);

        remove_var("REDIS_TIMEOUT");
        remove_var("REDIS_CONNECT_TIMEOUT");
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
            connect_timeout: 5,
        };

        assert_eq!(
            format!("{}", config),
            "redis://localhost:6379 (timeout: 30s)"
        );
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
            connect_timeout: 5,
        };

        // Credentials must be redacted, never printed.
        assert_eq!(
            format!("{}", config),
            "redis://***@localhost:6379 (timeout: 30s)"
        );
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
            connect_timeout: 5,
        };

        // Credentials must be redacted, never printed.
        assert_eq!(
            format!("{}", config),
            "redis://***@localhost:6379 (timeout: 30s)"
        );
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
            connect_timeout: 5,
        };

        // Credentials must be redacted, never printed.
        assert_eq!(
            format!("{}", config),
            "redis://***@localhost:6379 (timeout: 30s)"
        );
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
            connect_timeout: 5,
        };

        assert_eq!(
            format!("{}", config),
            "redis://localhost:6379/3 (timeout: 30s)"
        );
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
            connect_timeout: 5,
        };

        // Credentials must be redacted, never printed.
        assert_eq!(
            format!("{}", config),
            "redis://***@redis.example.com:6380/5 (timeout: 45s)"
        );
    }

    #[test]
    fn test_display_and_debug_redact_delimiter_passwords() {
        // Display/Debug are built from fields, so passwords containing URL
        // delimiters ('/', whitespace, '@') can never leak through parsing.
        for pw in ["p/secret-pw", "p secret-pw", "p@secret-pw"] {
            let config = RedisConfig {
                host: "localhost".to_string(),
                port: 6379,
                username: Some("user".to_string()),
                password: Some(pw.to_string()),
                database: 0,
                timeout: 30,
            };
            let display = format!("{}", config);
            let debug = format!("{:?}", config);
            assert!(!display.contains(pw), "Display leaked {pw:?}: {display}");
            assert!(!debug.contains(pw), "Debug leaked {pw:?}: {debug}");
            assert!(display.contains("***@"));
            // The connection URL still carries the real credential.
            assert!(config.url().contains(pw));
        }
    }

    #[test]
    fn test_display_and_debug_redact_password() {
        let config = RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            username: Some("admin".to_string()),
            password: Some("s3ntinel-pw".to_string()),
            database: 0,
            timeout: 30,
            connect_timeout: 5,
        };

        let display = format!("{}", config);
        let debug = format!("{:?}", config);

        // Neither Display nor Debug may leak the password or the username.
        assert!(!display.contains("s3ntinel-pw"));
        assert!(!display.contains("admin"));
        assert!(display.contains("***"));
        assert!(!debug.contains("s3ntinel-pw"));
        assert!(!debug.contains("admin"));
        assert!(debug.contains("***"));

        // The connection URL must still carry the real credentials so the
        // connection path keeps working.
        assert!(config.url().contains("s3ntinel-pw"));
        assert!(config.url().contains("admin"));
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
            connect_timeout: 8,
        };

        let cloned = original.clone();

        assert_eq!(cloned.host, "redis.example.com");
        assert_eq!(cloned.port, 6380);
        assert_eq!(cloned.username, Some("testuser".to_string()));
        assert_eq!(cloned.password, Some("testpass".to_string()));
        assert_eq!(cloned.database, 2);
        assert_eq!(cloned.timeout, 45);
        assert_eq!(cloned.connect_timeout, 8);
    }
}
