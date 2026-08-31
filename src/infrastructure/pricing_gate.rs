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
//! A sorted set of leases, scored by the instant each was taken, read from the
//! REDIS server rather than from the instance holding it: the whole point is
//! that several machines order each other's leases, and their clocks do not
//! agree well enough for one to decide when another's lease died. Acquiring is
//! one Lua script, so the expiry sweep, the count and the insert cannot
//! interleave with another instance doing the same:
//!
//! 1. drop every lease older than the lease window — that is what stops a
//!    killed instance from holding one forever;
//! 2. if fewer than the limit remain, add this one and say yes;
//! 3. otherwise say no, and let the caller wait and ask again.
//!
//! The window is a fallback, not a timer: a job that finishes releases its
//! lease, and a job still running renews it. Renewal is what makes the window
//! safe to keep short enough to be useful — without it a job outliving the
//! window would lose its lease to the sweep and let another in, whatever the
//! window was set to.
//!
//! # Full is not failure
//!
//! A gate that is up and full makes the caller WAIT, retrying until a lease
//! frees. Reporting "no lease" there would send every replica back to its own
//! semaphore exactly under the sustained load the deployment-wide bound exists
//! for.
//!
//! An error is different: it is reported as "no lease", and `admit_blocking`
//! then proceeds under the per-process bound, which is the behaviour the
//! service had before this existed. An unreachable Redis makes the deployment
//! price more concurrently than configured; refusing to price at all would be
//! a worse answer to the same outage.

use crate::infrastructure::RedisClient;
use crate::utils::admission::SharedPricingGate;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
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

/// How often a held lease is refreshed while its job runs.
///
/// A third of the window, so a renewal can be missed entirely — a slow round
/// trip, a paused thread — without the sweep reaping a live lease.
const RENEWAL_INTERVAL: Duration = Duration::from_secs(100);

/// How long to sleep between attempts when every lease is taken.
///
/// Short enough that a freed lease is picked up promptly, long enough that a
/// queue of waiters does not become a load test of its own.
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Acquires a lease if the deployment is under its bound.
///
/// The clock is the server's, read with `TIME` inside the script, so one
/// instance's skew cannot make it reap another instance's live lease.
///
/// `KEYS[1]` the sorted set; `ARGV[1]` the lease window in milliseconds,
/// `ARGV[2]` the limit, `ARGV[3]` this lease's token. Returns 1 when the lease
/// was taken and 0 when the bound is full.
const ACQUIRE_SCRIPT: &str = r"
local clock = redis.call('TIME')
local now = tonumber(clock[1]) * 1000 + math.floor(tonumber(clock[2]) / 1000)
local window = tonumber(ARGV[1])
local limit = tonumber(ARGV[2])

redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now - window)
if redis.call('ZCARD', KEYS[1]) < limit then
  redis.call('ZADD', KEYS[1], now, ARGV[3])
  redis.call('PEXPIRE', KEYS[1], window * 2)
  return 1
end
return 0
";

/// Refreshes a lease that is still held, on the same server clock.
///
/// Only refreshes a lease that is actually in the set: re-adding a reaped one
/// would let the deployment hold more leases than its bound.
///
/// `KEYS[1]` the sorted set; `ARGV[1]` the token, `ARGV[2]` the window in
/// milliseconds. Returns 1 when the lease was refreshed and 0 when it is gone.
const RENEW_SCRIPT: &str = r"
if redis.call('ZSCORE', KEYS[1], ARGV[1]) == false then
  return 0
end
local clock = redis.call('TIME')
local now = tonumber(clock[1]) * 1000 + math.floor(tonumber(clock[2]) / 1000)
redis.call('ZADD', KEYS[1], now, ARGV[1])
redis.call('PEXPIRE', KEYS[1], tonumber(ARGV[2]) * 2)
return 1
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

    /// One attempt. `Ok(true)` took a lease, `Ok(false)` the bound is full.
    async fn try_acquire(&self, token: &str) -> Result<bool, String> {
        let mut conn = self.client.connection_manager();

        let taken: i64 = redis::Script::new(ACQUIRE_SCRIPT)
            .key(&self.key)
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
    async fn acquire(&self) -> Option<String> {
        let token = Uuid::new_v4().to_string();
        let mut waited = Duration::ZERO;

        loop {
            match self.try_acquire(&token).await {
                Ok(true) => {
                    debug!(limit = self.limit, "took a deployment-wide pricing lease");
                    return Some(token);
                }
                // Full, which is the bound working. Waiting is the answer, and
                // the wait lives in the caller's future, so a client that goes
                // away cancels it.
                Ok(false) => {}
                Err(error) => {
                    // Unreachable: the caller proceeds under the per-process
                    // bound rather than failing.
                    warn!(
                        %error,
                        "the deployment-wide pricing gate is unreachable; falling back to this \
                         instance's own bound"
                    );
                    return None;
                }
            }

            tokio::time::sleep(RETRY_INTERVAL).await;
            waited += RETRY_INTERVAL;
            // Loud but not a decision: a queue this long is worth an operator's
            // attention, and letting it past the gate would not shorten it.
            if waited.as_millis().is_multiple_of(LEASE_WINDOW.as_millis()) {
                warn!(
                    limit = self.limit,
                    waited_secs = waited.as_secs(),
                    "still waiting for a deployment-wide pricing lease"
                );
            }
        }
    }

    async fn renew(&self, token: &str) -> bool {
        let mut conn = self.client.connection_manager();
        let renewed: Result<i64, _> = redis::Script::new(RENEW_SCRIPT)
            .key(&self.key)
            .arg(token)
            .arg(LEASE_WINDOW.as_millis().to_string())
            .invoke_async(&mut conn)
            .await;

        match renewed {
            Ok(1) => true,
            Ok(_) => false,
            Err(error) => {
                // Unknown rather than lost: the lease may well still be held,
                // and the next renewal is one interval away.
                warn!(%error, "a pricing lease could not be renewed");
                false
            }
        }
    }

    fn renewal_interval(&self) -> Duration {
        RENEWAL_INTERVAL
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
    /// replicas run N times the configured jobs, and this must not. Written
    /// against `try_acquire` rather than `acquire`, because a full gate now
    /// waits instead of reporting failure, which is the other half of the
    /// contract this asserts.
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

        let one = Uuid::new_v4().to_string();
        let two = Uuid::new_v4().to_string();
        let three = Uuid::new_v4().to_string();

        assert_eq!(
            first.try_acquire(&one).await,
            Ok(true),
            "the first lease fits in a bound of two"
        );
        assert_eq!(
            second.try_acquire(&two).await,
            Ok(true),
            "the second lease fits in a bound of two"
        );
        assert_eq!(
            second.try_acquire(&three).await,
            Ok(false),
            "a third lease was granted against a bound of two, so replicas can exceed it"
        );

        // A held lease renews; one that was never taken does not, or the sweep
        // could be defeated by any instance that kept asking.
        assert!(first.renew(&one).await, "a held lease must renew");
        assert!(
            !first.renew(&three).await,
            "a lease that is not held must not be renewable into existence"
        );

        // And releasing frees a slot for the other instance.
        first.release(&one).await;
        assert_eq!(
            second.try_acquire(&three).await,
            Ok(true),
            "a released lease must free a slot"
        );

        second.release(&two).await;
        second.release(&three).await;
        let _: Result<i64, _> = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut client.connection_manager())
            .await;
    }

    /// A gate that is up and full waits rather than reporting failure.
    ///
    /// Reporting failure would send every replica back to its own semaphore
    /// exactly under the sustained load the deployment-wide bound exists for.
    #[tokio::test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_a_full_gate_waits_instead_of_falling_back() {
        let client = match RedisClient::new(RedisConfig::default()).await {
            Ok(client) => Arc::new(client),
            Err(error) => panic!("this test needs a live Redis: {error}"),
        };
        let key = format!("test:pricing:{}", Uuid::new_v4());
        let gate = RedisPricingGate::new(Arc::clone(&client), key.clone(), 1);

        let held = Uuid::new_v4().to_string();
        assert_eq!(gate.try_acquire(&held).await, Ok(true));

        let mut waiting = Box::pin(gate.acquire());
        match futures::future::select(
            &mut waiting,
            Box::pin(tokio::time::sleep(Duration::from_millis(300))),
        )
        .await
        {
            futures::future::Either::Left((outcome, _)) => {
                panic!("a full gate must wait, not answer with {outcome:?}")
            }
            futures::future::Either::Right(((), _)) => {}
        }

        // Freed, the waiter takes it.
        gate.release(&held).await;
        match tokio::time::timeout(Duration::from_secs(5), waiting).await {
            Ok(Some(token)) => gate.release(&token).await,
            other => panic!("the waiter must take the freed lease: {other:?}"),
        }

        let _: Result<i64, _> = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut client.connection_manager())
            .await;
    }
}
