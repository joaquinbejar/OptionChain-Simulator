use crate::infrastructure::config::redact_userinfo;
use crate::infrastructure::config::redis::RedisConfig;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{AsyncCommands, Client, FromRedisValue, RedisError, RedisResult, ToSingleRedisArg};
use std::time::Duration;
use tracing::{debug, error, info, instrument};

/// An async client for interacting with a Redis database.
///
/// Internally this holds a [`ConnectionManager`], which owns a single
/// *multiplexed* connection: many in-flight commands share one socket and are
/// demultiplexed by the driver, so concurrent callers do NOT serialize on a
/// lock (unlike the previous `Mutex<Connection>` design that head-of-line
/// blocked every async worker). The manager also reconnects automatically on
/// connection loss.
///
/// `ConnectionManager` is cheap to clone (an `Arc` internally), so every method
/// takes `&self` and clones the manager for the individual command. This is why
/// `RedisClient` derives `Clone` and can be shared across actix workers without
/// contention.
#[derive(Clone)]
pub struct RedisClient {
    /// Multiplexed, auto-reconnecting connection manager (no `Mutex`).
    manager: ConnectionManager,
    /// Redis configuration
    config: RedisConfig,
}

impl RedisClient {
    /// Creates a new Redis client with the provided configuration.
    ///
    /// Establishes the multiplexed connection eagerly so a misconfigured or
    /// unreachable server surfaces at startup. Response and connection timeouts
    /// come from [`RedisConfig::timeout`] and [`RedisConfig::connect_timeout`].
    #[instrument(skip(config), level = "debug")]
    pub async fn new(config: RedisConfig) -> Result<Self, RedisError> {
        // Build Redis connection URL (used only to connect, never logged raw).
        let url = config.url();
        // The config's Display impl is already credential-redacted.
        info!("Connecting to Redis at {}", config);

        // Create Redis client
        let client = Client::open(url).inspect_err(|e| {
            // Sanitize in case the driver echoes the URL back in its error.
            error!(
                "Failed to open Redis client: {}",
                redact_userinfo(&e.to_string())
            );
        })?;

        // Bound both command responses and connection attempts so a hung server
        // can never pin an async worker indefinitely.
        let manager_config = ConnectionManagerConfig::new()
            .set_response_timeout(Some(Duration::from_secs(config.timeout)))
            .set_connection_timeout(Some(Duration::from_secs(config.connect_timeout)));

        // Multiplexed, auto-reconnecting connection manager (no per-op lock).
        let manager = ConnectionManager::new_with_config(client, manager_config)
            .await
            .inspect_err(|e| {
                error!(
                    "Failed to connect to Redis: {}",
                    redact_userinfo(&e.to_string())
                );
            })?;

        info!("Successfully connected to Redis");

        Ok(Self { manager, config })
    }

    /// Gets a value from Redis by key.
    ///
    /// Returns `Ok(None)` when the key is absent (a nil reply deserializes into
    /// `None`), so a missing key is not an error.
    #[instrument(skip(self), level = "debug")]
    pub async fn get<T: FromRedisValue>(&self, key: &str) -> RedisResult<Option<T>> {
        let mut conn = self.manager.clone();
        debug!("Retrieving key '{}' from Redis", key);
        let value: Option<T> = conn.get(key).await?;
        Ok(value)
    }

    /// Sets a value in Redis with an optional expiration time in seconds
    #[instrument(skip(self, value), level = "debug")]
    pub async fn set<T: ToSingleRedisArg + Send + Sync>(
        &self,
        key: &str,
        value: T,
        expiry_secs: Option<u64>,
    ) -> RedisResult<()> {
        let mut conn = self.manager.clone();

        match expiry_secs {
            Some(secs) => {
                debug!(
                    "Setting key '{}' in Redis with expiry of {} seconds",
                    key, secs
                );
                conn.set_ex(key, value, secs).await
            }
            None => {
                debug!("Setting key '{}' in Redis without expiry", key);
                conn.set(key, value).await
            }
        }
    }

    /// Atomically sets a value in Redis only if the key does not already exist
    /// (`SET key value NX [EX secs]`), with an optional expiration in seconds.
    ///
    /// # Returns
    /// - `Ok(true)` if the key was created (it did not exist before).
    /// - `Ok(false)` if the key already existed and was left untouched.
    /// - `Err(RedisError)` if the command failed.
    #[instrument(skip(self, value), level = "debug")]
    pub async fn set_nx<T: ToSingleRedisArg + Send + Sync>(
        &self,
        key: &str,
        value: T,
        expiry_secs: Option<u64>,
    ) -> RedisResult<bool> {
        let mut conn = self.manager.clone();

        // Redis replies with "OK" when the key was set and nil when NX prevented
        // the write; deserializing into `Option<String>` distinguishes the two.
        let outcome: Option<String> = match expiry_secs {
            Some(secs) => {
                debug!(
                    "Setting key '{}' in Redis with NX and expiry of {} seconds",
                    key, secs
                );
                redis::cmd("SET")
                    .arg(key)
                    .arg(value)
                    .arg("NX")
                    .arg("EX")
                    .arg(secs)
                    .query_async(&mut conn)
                    .await?
            }
            None => {
                debug!("Setting key '{}' in Redis with NX and no expiry", key);
                redis::cmd("SET")
                    .arg(key)
                    .arg(value)
                    .arg("NX")
                    .query_async(&mut conn)
                    .await?
            }
        };

        Ok(outcome.is_some())
    }

    /// Deletes a key from Redis
    #[instrument(skip(self), level = "debug")]
    pub async fn delete(&self, key: &str) -> RedisResult<bool> {
        let mut conn = self.manager.clone();
        debug!("Deleting key '{}' from Redis", key);
        let deleted: usize = conn.del(key).await?;
        Ok(deleted > 0)
    }

    /// Gets all keys matching a pattern
    #[instrument(skip(self), level = "debug")]
    pub async fn keys(&self, pattern: &str) -> RedisResult<Vec<String>> {
        let mut conn = self.manager.clone();
        debug!("Getting keys matching pattern '{}' from Redis", pattern);
        conn.keys(pattern).await
    }

    /// Checks if a key exists in Redis
    #[instrument(skip(self), level = "debug")]
    pub async fn exists(&self, key: &str) -> RedisResult<bool> {
        let mut conn = self.manager.clone();
        debug!("Checking if key '{}' exists in Redis", key);
        conn.exists(key).await
    }

    /// Returns the Redis configuration
    pub fn get_config(&self) -> &RedisConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of issue #19 is that a Redis operation no longer
    /// serializes on a shared lock. `ConnectionManager` multiplexes a single
    /// socket and is cheap to clone, so `RedisClient` must be `Clone` and every
    /// command method must take `&self`. These are the compile-level guarantees
    /// that let the client be shared across actix workers and driven
    /// concurrently without contention (a live-server concurrency test would
    /// require a Redis instance the unit suite deliberately does not depend on).
    #[test]
    fn test_redis_client_is_clone_send_sync() {
        fn assert_clone<T: Clone>() {}
        fn assert_send_sync<T: Send + Sync>() {}

        assert_clone::<RedisClient>();
        assert_send_sync::<RedisClient>();
    }

    /// Command methods take `&self`, so a single shared `&RedisClient` can drive
    /// several operations on DIFFERENT keys concurrently through one borrow —
    /// exactly the head-of-line-free behavior issue #19 requires. This function
    /// is a compile-time proof: it is never executed (it would need a live
    /// server) but it only type-checks if every method takes `&self` and the
    /// futures can be joined concurrently over the same borrow.
    #[allow(dead_code)]
    async fn concurrent_ops_compile_check(client: &RedisClient) {
        // Two GETs on different keys issued concurrently over one shared borrow.
        let a = client.get::<String>("session:a");
        let b = client.get::<String>("session:b");
        let _ = tokio::join!(a, b);

        // The remaining commands also accept the same shared &self.
        let _ = client.exists("session:a").await;
        let _ = client.keys("session:*").await;
        let _ = client.delete("session:a").await;
        let _ = client.set("session:a", "v".to_string(), None).await;
        let _ = client.set_nx("session:b", "v".to_string(), Some(1)).await;
    }
}
