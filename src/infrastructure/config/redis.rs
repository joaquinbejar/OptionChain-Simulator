use std::fmt;

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
    match super::read_var(var) {
        Some(raw) => match raw.parse::<u64>() {
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
        None => default,
    }
}

/// Percent-encodes one credential for the URL's userinfo section.
///
/// Everything outside RFC 3986's `unreserved` set is encoded, which is wider
/// than the minimum the grammar demands: `sub-delims` and `:` are legal in
/// userinfo unencoded, but encoding them costs nothing and removes every
/// question about where the section ends. `%` is escaped along with the rest,
/// so a password that legitimately contains `%40` becomes `%2540` and cannot
/// decode back to `@`.
///
/// A credential of letters, digits, `-`, `.`, `_` or `~` passes through
/// untouched, so a URL that works today is byte-identical after this.
#[must_use]
fn encode_userinfo(raw: &str) -> String {
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            // Uppercase hex, as RFC 3986 recommends for what a producer emits.
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

impl RedisConfig {
    pub(crate) fn url(&self) -> String {
        // Start building the URL
        let mut url = String::from("redis://");

        // Add credentials if either username or password is present
        //
        // PERCENT-ENCODED, both of them. Interpolated verbatim, a credential
        // carrying a URL delimiter builds a URL that is WRONG rather than one
        // that fails: a `/` moves what the driver reads as the database index,
        // a `#` truncates the rest, and a `@` moves where the credentials are
        // taken to end. The resolved fields stay unencoded, so a caller reading
        // `RedisConfig.password` still sees what the operator set.
        if self.username.is_some() || self.password.is_some() {
            // Add username if present, otherwise an empty string
            if let Some(username) = &self.username {
                url.push_str(&encode_userinfo(username));
            }

            // Add password with colon prefix if present
            if let Some(password) = &self.password {
                url.push(':');
                url.push_str(&encode_userinfo(password));
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
        // Trimmed at the call site: these are numbers, so surrounding
        // whitespace is noise. The credentials below are not trimmed at all.
        let port = super::read_var("REDIS_PORT")
            .and_then(|s| s.trim().parse::<u16>().ok())
            .unwrap_or(6379);

        let database = super::read_var("REDIS_DB")
            .and_then(|s| s.trim().parse::<u8>().ok())
            .unwrap_or(0);

        // Blank is unset HERE, unlike ClickHouse: `REDIS_USER=` and
        // `REDIS_PASSWORD=` used to become `Some("")` and build
        // `redis://:@host`, an AUTH attempt with an empty password against a
        // server that has none. An empty Redis credential asks for exactly what
        // an absent one does, so nothing is expressible only through the blank
        // form and the failure mode is real. Untrimmed, like every credential:
        // whatever a present, non-blank value says is what gets sent.
        let username = super::read_var("REDIS_USER");
        let password = super::read_var("REDIS_PASSWORD");

        let timeout = parse_timeout_secs("REDIS_TIMEOUT", 30);
        let connect_timeout = parse_timeout_secs("REDIS_CONNECT_TIMEOUT", 5);

        Self {
            host: super::read_var("REDIS_HOST")
                .map(|host| host.trim().to_string())
                .unwrap_or_else(|| "localhost".to_string()),
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
            std::env::set_var(name, value);
        }
    }

    fn remove_var(name: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(name);
        }
    }

    /// A blank credential is an unset credential, not an empty one.
    ///
    /// The failure this closes: `REDIS_PASSWORD=` used to become `Some("")`,
    /// which built `redis://:@host:6379` — an AUTH attempt with an empty
    /// password against a server that has none. The connection failed with an
    /// authentication error that pointed nowhere near the actual cause, and
    /// `.env.example` shipped exactly that line.
    #[test]
    fn test_a_blank_credential_reads_as_unset() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        remove_var("REDIS_HOST");
        remove_var("REDIS_PORT");
        remove_var("REDIS_DB");
        remove_var("REDIS_TIMEOUT");
        remove_var("REDIS_CONNECT_TIMEOUT");

        for blank in ["", "   "] {
            set_var("REDIS_USER", blank);
            set_var("REDIS_PASSWORD", blank);

            let config = RedisConfig::default();
            assert_eq!(config.username, None, "user {blank:?} must read as unset");
            assert_eq!(
                config.password, None,
                "password {blank:?} must read as unset"
            );
            assert!(
                !config.url().contains('@'),
                "a password-less server must get a URL with no userinfo, got {}",
                config.url()
            );
        }

        remove_var("REDIS_USER");
        remove_var("REDIS_PASSWORD");
    }

    /// A credential that is actually set still reaches the URL unchanged.
    ///
    /// The other half of the rule: treating blank as unset must not touch a
    /// real value, including one with punctuation a URL would otherwise eat.
    #[test]
    fn test_a_real_credential_reaches_the_url_unchanged() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        remove_var("REDIS_HOST");
        remove_var("REDIS_PORT");
        remove_var("REDIS_DB");
        remove_var("REDIS_TIMEOUT");
        remove_var("REDIS_CONNECT_TIMEOUT");
        remove_var("REDIS_USER");

        // Two separate claims. The RESOLVED value must survive verbatim,
        // punctuation included, which is what blank-is-unset must not disturb.
        set_var("REDIS_PASSWORD", "s3cr:t@pass");
        assert_eq!(
            RedisConfig::default().password,
            Some("s3cr:t@pass".to_string()),
            "a real password must reach the config untouched"
        );

        // And that it reaches the URL, now percent-encoded: the punctuated
        // password round-trips through a real parse in
        // `test_a_punctuated_credential_round_trips_through_the_url`.
        set_var("REDIS_PASSWORD", "s3cret");
        let config = RedisConfig::default();
        assert!(
            config.url().contains("s3cret"),
            "the password must reach the URL, got {}",
            config.url()
        );

        remove_var("REDIS_PASSWORD");
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

    /// The credentials a URL parses back to, or `None` when the driver
    /// refuses it.
    ///
    /// A real parse by the driver that will consume this URL, so the assertion
    /// is about what Redis receives rather than about the string this module
    /// produced. Both halves: the username is encoded too, and a driver that
    /// stopped decoding usernames would leave a password-only assertion green.
    fn credentials_of(url: &str) -> Option<(Option<String>, Option<String>)> {
        let client = redis::Client::open(url).ok()?;
        let settings = client.get_connection_info().redis_settings();
        Some((
            settings.username().map(ToString::to_string),
            settings.password().map(ToString::to_string),
        ))
    }

    /// A credential full of delimiters parses back to exactly itself.
    ///
    /// Every one of these characters means something in a URL: `/` moves the
    /// database index, `#` truncates the rest, `?` opens a query, `@` moves
    /// where the credentials end, `:` splits user from password, a space is not
    /// allowed at all, and `%` is what an encoder must escape first or a
    /// literal `%40` decodes to `@`.
    #[test]
    fn test_a_punctuated_credential_round_trips_through_the_url() {
        for password in [
            "s3cr:t@pass",
            "p/secret",
            "with#hash",
            "with?query",
            "with space",
            "100%pure",
            "everything: /#?@% and more",
        ] {
            // The same punctuation in BOTH slots: the username goes through
            // the same encoder and the same decode on the driver's side.
            let config = RedisConfig {
                host: "localhost".to_string(),
                port: 6379,
                username: Some(password.to_string()),
                password: Some(password.to_string()),
                database: 0,
                timeout: 30,
                connect_timeout: 5,
            };

            assert_eq!(
                credentials_of(&config.url()),
                Some((Some(password.to_string()), Some(password.to_string()))),
                "{password:?} did not survive the URL: {}",
                config.url()
            );
        }
    }

    /// A `/` in a credential does not move the database index.
    ///
    /// The failure that motivated the issue: unencoded, `p/secret` ends the
    /// authority early and the driver reads a database that nobody configured.
    #[test]
    fn test_a_slash_in_a_credential_leaves_the_database_alone() {
        let config = RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            username: None,
            password: Some("p/7/secret".to_string()),
            database: 3,
            timeout: 30,
            connect_timeout: 5,
        };

        let client = match redis::Client::open(config.url()) {
            Ok(client) => client,
            Err(error) => panic!("the URL must parse: {error}, url {}", config.url()),
        };
        let info = client.get_connection_info();

        assert_eq!(
            info.redis_settings().db(),
            3,
            "the configured database must survive"
        );
        assert_eq!(info.redis_settings().password(), Some("p/7/secret"));
    }

    /// An unreserved credential produces the URL it produced before.
    ///
    /// The encoding must be invisible to everything already working: letters,
    /// digits, `-`, `.`, `_` and `~` pass through untouched.
    #[test]
    fn test_an_unreserved_credential_is_untouched() {
        let config = RedisConfig {
            host: "redis.internal".to_string(),
            port: 6380,
            username: Some("admin".to_string()),
            password: Some("s3cret-pass.word_v2~1".to_string()),
            database: 2,
            timeout: 30,
            connect_timeout: 5,
        };

        assert_eq!(
            config.url(),
            "redis://admin:s3cret-pass.word_v2~1@redis.internal:6380/2"
        );
    }

    /// The encoder escapes what it must, and nothing else.
    #[test]
    fn test_the_encoder_escapes_only_the_reserved() {
        assert_eq!(encode_userinfo("aZ09-._~"), "aZ09-._~");
        assert_eq!(encode_userinfo("a/b"), "a%2Fb");
        assert_eq!(encode_userinfo("a@b"), "a%40b");
        assert_eq!(encode_userinfo("a:b"), "a%3Ab");
        assert_eq!(encode_userinfo("a b"), "a%20b");
        // `%` first: otherwise a literal %40 in a password would decode to @.
        assert_eq!(encode_userinfo("%40"), "%2540");
        // Multi-byte characters are encoded per UTF-8 byte.
        assert_eq!(encode_userinfo("ñ"), "%C3%B1");
    }

    /// A punctuated credential still cannot leak through the redaction.
    ///
    /// Encoding removes every `@` from the credential, so the last `@` in the
    /// URL is the real separator and `redact_userinfo` cannot be walked past it.
    #[test]
    fn test_the_redaction_covers_the_encoded_form() {
        let config = RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            username: Some("adm@in".to_string()),
            password: Some("p@ss/word".to_string()),
            database: 0,
            timeout: 30,
            connect_timeout: 5,
        };

        let redacted = crate::infrastructure::config::redact_userinfo(&config.url());

        assert_eq!(
            redacted, "redis://***@localhost:6379",
            "nothing of either credential may survive"
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
                connect_timeout: 5,
            };
            let display = format!("{}", config);
            let debug = format!("{:?}", config);
            assert!(!display.contains(pw), "Display leaked {pw:?}: {display}");
            assert!(!debug.contains(pw), "Debug leaked {pw:?}: {debug}");
            assert!(display.contains("***@"));
            // The URL still carries the credential, percent-encoded, which is
            // what makes the last `@` the real separator for the redaction.
            assert_eq!(
                credentials_of(&config.url()),
                Some((Some("user".to_string()), Some(pw.to_string()))),
                "the URL must parse back to the credential"
            );
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
        // connection path keeps working. Alphanumerics and `-` are unreserved,
        // so these two appear verbatim.
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
