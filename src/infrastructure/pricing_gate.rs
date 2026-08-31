//! The deployment-wide pricing bound, held in Redis.
//!
//! `utils::admission` bounds the pricing jobs of ONE process, which is the
//! wrong unit once the service runs replicated: an operator asking for four and
//! running two replicas gets eight, and learns it when the host saturates
//! (issue #135). This is the adapter that makes the same number mean the
//! deployment.
//!
//! # How it holds
//!
//! A sorted set of leases, scored by the instant each was taken. Acquiring is
//! one Lua script, so the expiry sweep, the count and the insert cannot
//! interleave with another instance doing the same:
//!
//! 1. drop every lease older than the lease window — that is what stops a
//!    killed instance from holding one forever;
//! 2. if fewer than the limit remain, add this one and say yes;
//! 3. otherwise say no, and let the caller wait and ask again.
//!
//! The window is a fallback, not a timer: a job that finishes releases its
//! lease. It has to be longer than a pricing job or a running job would lose
//! its lease to the sweep and let another in.
//!
//! # It never fails a request
//!
//! Every error here is reported as "no lease", and `admit_blocking` then
//! proceeds under the per-process bound, which is the behaviour the service
//! had before this existed. An unreachable Redis makes the deployment price
//! more concurrently than configured; refusing to price at all would be a worse
//! answer to the same outage.

use crate::infrastructure::RedisClient;
use crate::utils::admission::SharedPricingGate;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};
use uuid::Uuid;

/// The key the leases live under.
pub const DEFAULT_PRICING_GATE_KEY: &str = "optionchain:pricing:leases";

/// How long a lease survives without being released.
///
/// Longer than any pricing job: a running job whose lease expired would let a
/// second one in, which is the bound leaking. A snapshot at the contract cap
/// is seconds, so minutes is the safe side of that.
const LEASE_WINDOW: Duration = Duration::from_secs(300);

/// How long to sleep between attempts when every lease is taken.
///
/// Short enough that a freed lease is picked up promptly, long enough that a
/// queue of waiters does not become a load test of its own.
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Acquires a lease if the deployment is under its bound.
///
/// `KEYS[1]` the sorted set; `ARGV[1]` now in milliseconds, `ARGV[2]` the lease
/// window in milliseconds, `ARGV[3]` the limit, `ARGV[4]` this lease's token.
/// Returns 1 when the lease was taken and 0 when the bound is full.
const ACQUIRE_SCRIPT: &str = r"
local now = tonumber(ARGV[1])
local window = tonumber(ARGV[2])
local limit = tonumber(ARGV[3])

redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now - window)
if redis.call('ZCARD', KEYS[1]) < limit then
  redis.call('ZADD', KEYS[1], now, ARGV[4])
  redis.call('PEXPIRE', KEYS[1], window * 2)
  return 1
end
return 0
";

/// A pricing bound shared by every instance through Redis.
pub struct RedisPricingGate {
    client: Arc<RedisClient>,
    key: String,
    limit: usize,
}

impl RedisPricingGate {
    /// Builds a gate allowing `limit` pricing jobs across the deployment.
    #[must_use]
    pub fn new(client: Arc<RedisClient>, key: impl Into<String>, limit: usize) -> Self {
        Self {
            client,
            key: key.into(),
            limit: limit.max(1),
        }
    }

    /// Milliseconds since the epoch, or `None` if the clock is before it.
    ///
    /// A clock that cannot be read is a gate that cannot decide, which is
    /// reported as "no lease" like every other failure here.
    fn now_millis() -> Option<u64> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|since| u64::try_from(since.as_millis()).ok())
    }

    /// One attempt. `Ok(true)` took a lease, `Ok(false)` the bound is full.
    async fn try_acquire(&self, token: &str) -> Result<bool, String> {
        let now = Self::now_millis().ok_or_else(|| "the clock is before the epoch".to_string())?;
        let mut conn = self.client.connection_manager();

        let taken: i64 = redis::Script::new(ACQUIRE_SCRIPT)
            .key(&self.key)
            .arg(now.to_string())
            .arg(LEASE_WINDOW.as_millis().to_string())
            .arg(self.limit.to_string())
            .arg(token)
            .invoke_async(&mut conn)
            .await
            .map_err(|error| error.to_string())?;

        Ok(taken == 1)
    }
}

#[async_trait]
impl SharedPricingGate for RedisPricingGate {
    async fn acquire(&self, deadline: Duration) -> Option<String> {
        let token = Uuid::new_v4().to_string();
        let started = SystemTime::now();

        loop {
            match self.try_acquire(&token).await {
                Ok(true) => {
                    debug!(limit = self.limit, "took a deployment-wide pricing lease");
                    return Some(token);
                }
                Ok(false) => {}
                Err(error) => {
                    // Unreachable: the caller proceeds under the per-process
                    // bound rather than failing, and says so once per attempt
                    // rather than per retry.
                    warn!(
                        %error,
                        "the deployment-wide pricing gate is unreachable; falling back to this \
                         instance's own bound"
                    );
                    return None;
                }
            }

            let waited = started.elapsed().unwrap_or(Duration::ZERO);
            if waited >= deadline {
                warn!(
                    limit = self.limit,
                    waited_secs = waited.as_secs(),
                    "waited for a deployment-wide pricing lease without getting one; proceeding \
                     under this instance's own bound"
                );
                return None;
            }
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    }

    async fn release(&self, token: &str) {
        let mut conn = self.client.connection_manager();
        let removed: Result<i64, _> = redis::cmd("ZREM")
            .arg(&self.key)
            .arg(token)
            .query_async(&mut conn)
            .await;

        if let Err(error) = removed {
            // The lease expires on its own, so this costs a slot for the
            // window rather than forever.
            warn!(%error, "a pricing lease could not be released; it will expire");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::RedisConfig;

    /// A limit of zero would stop the deployment pricing anything at all, so
    /// it is read as one.
    #[tokio::test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_a_limit_of_zero_becomes_one() {
        let client = match RedisClient::new(RedisConfig::default()).await {
            Ok(client) => Arc::new(client),
            Err(error) => panic!("this test needs a live Redis: {error}"),
        };
        let gate = RedisPricingGate::new(client, "test:limit", 0);
        assert_eq!(gate.limit, 1, "a limit of zero would price nothing");
    }

    /// Two gates over one Redis cannot exceed the bound between them.
    ///
    /// This is the whole point of the issue: the per-process semaphore lets N
    /// replicas run N times the configured jobs, and this must not.
    #[tokio::test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_two_gates_cannot_exceed_the_bound_together() {
        let client = match RedisClient::new(RedisConfig::default()).await {
            Ok(client) => Arc::new(client),
            Err(error) => panic!("this test needs a live Redis: {error}"),
        };
        let key = format!("test:pricing:{}", Uuid::new_v4());

        let first = RedisPricingGate::new(Arc::clone(&client), key.clone(), 2);
        let second = RedisPricingGate::new(Arc::clone(&client), key.clone(), 2);

        let one = first.acquire(Duration::from_millis(200)).await;
        let two = second.acquire(Duration::from_millis(200)).await;
        assert!(
            one.is_some() && two.is_some(),
            "two leases fit in a bound of two"
        );

        // The third must not fit, whichever instance asks.
        let three = second.acquire(Duration::from_millis(200)).await;
        assert!(
            three.is_none(),
            "a third lease was granted against a bound of two, so replicas can exceed it"
        );

        // And releasing frees a slot for the other instance.
        if let Some(token) = one {
            first.release(&token).await;
        }
        let four = second.acquire(Duration::from_millis(500)).await;
        assert!(four.is_some(), "a released lease must free a slot");

        if let Some(token) = two {
            first.release(&token).await;
        }
        if let Some(token) = four {
            second.release(&token).await;
        }
        let _: Result<i64, _> = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut client.connection_manager())
            .await;
    }
}
