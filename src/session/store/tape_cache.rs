//! A factor tape shared between instances.
//!
//! # Why
//!
//! A tape is a pure function of a simulation's stored parameters and its seed,
//! and building one walks every step. Kept only in the process that built it,
//! a deployment behind a balancer rebuilds it on whichever instance takes the
//! next step: with N instances the hit rate falls towards 1/N, and the work
//! repeated is the expensive kind (issue #136).
//!
//! Correctness was never at risk — the same parameters produce the same tape
//! anywhere, which is the reproducibility contract — so this is a cost fix, and
//! it is written to behave like one. Every failure here degrades to the
//! behaviour without it: a cache command that fails is a miss, never a failed
//! request.
//!
//! That promise is about the CACHE and not about Redis. The simulation itself
//! is read from the same Redis before this is consulted, so an outage fails
//! there; what degrades is the saving, not the service's dependence on its
//! store.
//!
//! # What bounds it
//!
//! `OCS_MAX_CACHED_TAPES` — the same knob that bounds each instance's own map
//! — bounds this one for the whole deployment. A recency index orders the
//! entries and the write script evicts the oldest past the bound in the same
//! atomic step, so two instances cannot both see room and both insert. A
//! deleted or reaped simulation drops its tapes rather than waiting out the
//! TTL.
//!
//! # What the key carries
//!
//! Everything that decides the VALUES, not just the identity:
//!
//! - the snapshot generation, so a build that changes what a step means cannot
//!   read a tape written under the old meaning;
//! - a fingerprint of the parameters, so a document rewritten under the same id
//!   cannot be served a tape of the parameters it used to have;
//! - the simulation id.
//!
//! The fingerprint is a UUID v5 over the canonical JSON of the parameters,
//! which is deterministic across processes and across releases in a way a
//! `DefaultHasher` is not.

use crate::infrastructure::CURRENT_SNAPSHOT_GENERATION;
use crate::infrastructure::RedisClient;
use crate::session::SimulationParametersV2;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};
use uuid::Uuid;

/// The namespace the parameter fingerprints are drawn from.
///
/// A fixed v5 namespace, so the same parameters produce the same fingerprint in
/// every process and every release.
const FINGERPRINT_NAMESPACE: Uuid =
    Uuid::from_u128(0x6f_63_73_5f_74_61_70_65_5f_63_61_63_68_65_76_31);

/// The default key prefix, so one Redis can hold more than one deployment.
pub const DEFAULT_TAPE_KEY_PREFIX: &str = "optionchain:tape:";

/// Writes a tape, records its recency, and evicts down to the bound.
///
/// One script, so the write, the index update and the eviction cannot
/// interleave with another instance doing the same. Without it the bound would
/// be advisory: two instances could each see room and both insert.
///
/// `KEYS[1]` the recency index, `KEYS[2]` the tape's own key. `ARGV[1]` the
/// encoded tape, `ARGV[2]` the TTL in seconds, `ARGV[3]` now, `ARGV[4]` how
/// many tapes the deployment may hold.
///
/// Victims are deleted by the names the index holds, whose slots are not
/// declared as keys, so this wants a single Redis rather than a cluster. Every
/// script in this crate makes the same assumption.
const PUT_SCRIPT: &str = r"
local ttl = tonumber(ARGV[2])
redis.call('SET', KEYS[2], ARGV[1], 'EX', ttl)
redis.call('ZADD', KEYS[1], tonumber(ARGV[3]), KEYS[2])

local excess = redis.call('ZCARD', KEYS[1]) - tonumber(ARGV[4])
if excess > 0 then
  local victims = redis.call('ZRANGE', KEYS[1], 0, excess - 1)
  for _, victim in ipairs(victims) do
    if victim ~= KEYS[2] then
      redis.call('DEL', victim)
    end
    redis.call('ZREM', KEYS[1], victim)
  end
end

redis.call('EXPIRE', KEYS[1], ttl * 2)
return 1
";

/// Drops every tape belonging to one simulation, index entries included.
///
/// A key ends with the simulation id, so a deleted or reaped simulation is
/// found by suffix. Bounded by the index, which is itself bounded by the
/// deployment cap, so this is a short scan rather than a `KEYS *`.
///
/// `KEYS[1]` the index, `ARGV[1]` the suffix to match.
const FORGET_SCRIPT: &str = r"
local members = redis.call('ZRANGE', KEYS[1], 0, -1)
local dropped = 0
for _, member in ipairs(members) do
  if string.sub(member, -string.len(ARGV[1])) == ARGV[1] then
    redis.call('DEL', member)
    redis.call('ZREM', KEYS[1], member)
    dropped = dropped + 1
  end
end
return dropped
";

/// A place to leave a built tape for the other instances.
///
/// It trades in ENCODED tapes rather than in the tape type: the tape is a
/// domain type and the domain is private to this crate, so a public trait
/// carrying it would leak one. The encoding and the key belong to the caller
/// that owns both, which is the manager.
///
/// Every method swallows its own failures. An implementation that cannot reach
/// its store reports a miss and lets the caller build, because a slower answer
/// is better than none.
#[async_trait]
pub trait SharedTapeCache: Send + Sync {
    /// What is stored under `key`, if anything.
    async fn get(&self, key: &str) -> Option<String>;

    /// Offers `encoded` to the other instances, under `key`.
    async fn put(&self, key: &str, encoded: &str);

    /// Drops whatever this simulation left behind, because it is gone.
    ///
    /// Without it a deleted or reaped simulation keeps its tape until the TTL,
    /// which is memory nobody can use and, on a busy deployment, most of what
    /// the cache holds. Called with the id rather than a key because the
    /// parameters that built the key may already have been deleted.
    async fn forget_simulation(&self, id: &str);
}

/// The key a tape is stored under.
///
/// Public within the crate so tests can assert what it carries rather than
/// inferring it from behaviour.
pub(crate) fn tape_key(id: Uuid, parameters: &SimulationParametersV2) -> String {
    let fingerprint = fingerprint(parameters);
    format!("{CURRENT_SNAPSHOT_GENERATION}:{fingerprint}:{id}")
}

/// A deterministic fingerprint of everything that decides a tape's values.
///
/// Derived from the serialised parameters rather than from a hand-picked list
/// of fields: a new field that changes the walk is then covered the day it is
/// added, instead of the day someone remembers to add it here.
fn fingerprint(parameters: &SimulationParametersV2) -> Uuid {
    match serde_json::to_vec(parameters) {
        Ok(canonical) => Uuid::new_v5(&FINGERPRINT_NAMESPACE, &canonical),
        // Parameters that will not serialise cannot be fingerprinted, and a
        // constant would collide across simulations. A random value simply
        // misses the cache, every time, which is the safe direction.
        Err(error) => {
            warn!(%error, "could not fingerprint the parameters; the shared tape cache will miss");
            Uuid::new_v4()
        }
    }
}

/// A shared tape cache backed by Redis.
pub struct RedisTapeCache {
    client: Arc<RedisClient>,
    prefix: String,
    ttl: Duration,
    /// How many tapes the DEPLOYMENT may hold, enforced on every write.
    ///
    /// The same knob that bounds each instance's own map, now meaning the
    /// shared cache too: an operator writes the number of tapes they are
    /// willing to keep, and gets that many rather than that many per replica.
    bound: usize,
}

impl RedisTapeCache {
    /// Builds a cache over `client`.
    ///
    /// `ttl` should be the simulation retention window: a tape outliving the
    /// simulation it belongs to is memory nobody can use, and one expiring
    /// before it costs a rebuild.
    #[must_use]
    pub fn new(
        client: Arc<RedisClient>,
        prefix: impl Into<String>,
        ttl: Duration,
        bound: usize,
    ) -> Self {
        Self {
            client,
            prefix: prefix.into(),
            ttl,
            bound: bound.max(1),
        }
    }

    /// The recency index, one per prefix.
    fn index_key(&self) -> String {
        format!("{}index", self.prefix)
    }

    /// Milliseconds since the epoch, or zero if the clock will not read.
    ///
    /// Only used to order entries against each other, so a clock problem costs
    /// eviction order rather than correctness. Milliseconds rather than
    /// seconds because several tapes are written within one second routinely,
    /// and tied scores make the eviction order lexicographic by key, which is
    /// to say arbitrary.
    fn now_millis() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|since| u64::try_from(since.as_millis()).ok())
            .unwrap_or(0)
    }
}

#[async_trait]
impl SharedTapeCache for RedisTapeCache {
    async fn get(&self, key: &str) -> Option<String> {
        let key = format!("{}{key}", self.prefix);
        match self.client.get::<String>(&key).await {
            Ok(stored) => {
                if stored.is_some() {
                    debug!(%key, "shared tape cache hit");
                    // Recency, or the eviction below would drop exactly the
                    // tapes the deployment is using.
                    let mut conn = self.client.connection_manager();
                    let touched: Result<i64, _> = redis::cmd("ZADD")
                        .arg(self.index_key())
                        .arg(Self::now_millis())
                        .arg(&key)
                        .query_async(&mut conn)
                        .await;
                    if let Err(error) = touched {
                        warn!(%error, %key, "a tape's recency could not be recorded");
                    }
                }
                stored
            }
            Err(error) => {
                // A miss, deliberately: the caller builds and the request is
                // served. Reporting the error would turn an unreachable cache
                // into an unreachable service.
                warn!(%error, %key, "the shared tape cache could not be read");
                None
            }
        }
    }

    async fn put(&self, key: &str, encoded: &str) {
        let key = format!("{}{key}", self.prefix);
        let seconds = self.ttl.as_secs().max(1);
        let now = Self::now_millis();
        let mut conn = self.client.connection_manager();

        let written: Result<i64, _> = redis::Script::new(PUT_SCRIPT)
            .key(self.index_key())
            .key(&key)
            .arg(encoded)
            .arg(seconds.to_string())
            .arg(now.to_string())
            .arg(self.bound.to_string())
            .invoke_async(&mut conn)
            .await;

        if let Err(error) = written {
            warn!(%error, %key, "the shared tape cache could not be written");
        }
    }

    async fn forget_simulation(&self, id: &str) {
        let mut conn = self.client.connection_manager();
        let dropped: Result<i64, _> = redis::Script::new(FORGET_SCRIPT)
            .key(self.index_key())
            .arg(format!(":{id}"))
            .invoke_async(&mut conn)
            .await;

        match dropped {
            Ok(dropped) if dropped > 0 => {
                debug!(simulation_id = %id, dropped, "dropped the shared tapes of a gone simulation");
            }
            Ok(_) => {}
            // They expire on their own, so this costs the bound a slot until
            // then rather than leaking.
            Err(error) => warn!(
                %error,
                simulation_id = %id,
                "the shared tapes of a gone simulation could not be dropped; they will expire"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
    use crate::api::rest::requests_v2::CreateSimulationRequest;
    use crate::session::{ExpiryRule, ExpiryRuleKind};

    /// Reference parameters, so the key tests vary exactly one thing.
    fn parameters(seed: u64) -> SimulationParametersV2 {
        let rule = match ExpiryRule::new("zero_dte", ExpiryRuleKind::Daily, 1) {
            Ok(rule) => rule,
            Err(error) => panic!("the test rule must be valid: {error}"),
        };
        let request = CreateSimulationRequest {
            symbol: "SPX".to_string(),
            steps: 4,
            start_at: None,
            step_interval_seconds: Some(86_400),
            timezone: "America/New_York".to_string(),
            calendar: None,
            expiration_time: "17:00".to_string(),
            schedules: vec![rule],
            initial_price: 5000.0,
            volatility: 0.2,
            risk_free_rate: 0.05,
            dividend_yield: 0.0,
            method: ApiWalkType::GeometricBrownian {
                dt: 0.004,
                drift: 0.05,
                volatility: 0.2,
            },
            time_frame: ApiTimeFrame::Day,
            chain_size: Some(2),
            strike_interval: Some(25.0),
            skew_slope: None,
            smile_curve: None,
            spread: None,
            spread_proportional: None,
            spread_moneyness_widening: None,
            spread_tenor_widening: None,
            spread_tick: None,
            strike_ladder: None,
            seed: Some(seed),
        };
        match SimulationParametersV2::try_from(request) {
            Ok(parameters) => parameters,
            Err(error) => panic!("the reference request must convert: {error}"),
        }
    }

    /// The same parameters key the same way, in this process and any other.
    #[test]
    fn test_the_key_is_stable_for_the_same_parameters() {
        let id = Uuid::from_u128(1);
        let left = tape_key(id, &parameters(42));
        let right = tape_key(id, &parameters(42));

        assert_eq!(left, right);
        assert!(
            left.starts_with(&format!("{CURRENT_SNAPSHOT_GENERATION}:")),
            "the key must carry the generation first: {left}"
        );
        assert!(
            left.ends_with(&id.to_string()),
            "the key must name the simulation: {left}"
        );
    }

    /// A different seed is a different tape, so it must be a different key.
    #[test]
    fn test_a_different_seed_is_a_different_key() {
        let id = Uuid::from_u128(1);
        assert_ne!(
            tape_key(id, &parameters(42)),
            tape_key(id, &parameters(43)),
            "two seeds produce two tapes and must not share a key"
        );
    }

    /// The bound is the DEPLOYMENT's: a third tape evicts the oldest.
    ///
    /// Against a live Redis, because the eviction is a Lua script and testing
    /// it anywhere else would be testing a different thing.
    #[tokio::test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_the_bound_evicts_the_least_recently_used() {
        use crate::infrastructure::RedisConfig;

        let client = match RedisClient::new(RedisConfig::default()).await {
            Ok(client) => Arc::new(client),
            Err(error) => panic!("this test needs a live Redis: {error}"),
        };
        let prefix = format!("test:tape:{}:", Uuid::new_v4());
        let cache = RedisTapeCache::new(
            Arc::clone(&client),
            prefix.clone(),
            Duration::from_secs(60),
            2,
        );

        cache.put("a:1", "first").await;
        cache.put("b:2", "second").await;
        // Touching the first makes the second the least recently used.
        assert_eq!(cache.get("a:1").await.as_deref(), Some("first"));
        cache.put("c:3", "third").await;

        assert_eq!(
            cache.get("a:1").await.as_deref(),
            Some("first"),
            "the recently used tape must survive"
        );
        assert_eq!(
            cache.get("c:3").await.as_deref(),
            Some("third"),
            "the tape just written must be there"
        );
        assert_eq!(
            cache.get("b:2").await,
            None,
            "a bound of two must have evicted the least recently used"
        );

        // And a simulation that is gone takes its tapes with it.
        cache.forget_simulation("3").await;
        assert_eq!(
            cache.get("c:3").await,
            None,
            "a forgotten simulation must leave nothing behind"
        );

        let _: Result<i64, _> = redis::cmd("DEL")
            .arg(format!("{prefix}index"))
            .arg(format!("{prefix}a:1"))
            .query_async(&mut client.connection_manager())
            .await;
    }

    /// Two simulations never share a key, however alike their parameters are.
    #[test]
    fn test_two_simulations_do_not_share_a_key() {
        let parameters = parameters(42);
        assert_ne!(
            tape_key(Uuid::from_u128(1), &parameters),
            tape_key(Uuid::from_u128(2), &parameters)
        );
    }
}
