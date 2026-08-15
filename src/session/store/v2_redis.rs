//! Redis-backed [`SimulationStore`] for v2 rolling simulations.
//!
//! Uses its own key prefix — `optionchain:simulation:v2:` by default — so a v2
//! document and a v1 session can never collide, and a v1 document is never read
//! back as rolling configuration (ADR 0001 §12.2).
//!
//! # Why every write is a script
//!
//! A simulation is three keys: the document, a companion revision key, and a
//! membership entry in an index. Writing them with sequential commands leaves
//! two silent torn states — a document with no revision key, and a document
//! absent from the index, which would be invisible to [`SimulationStore::cleanup`]
//! forever. Each operation is therefore one Lua script, which Redis runs as a
//! single uninterruptible unit.
//!
//! # Why there is an index at all
//!
//! Redis expires keys on its own, invisibly to this process. That is fine for
//! v1, whose `cleanup` honestly reports `0`. It is not fine for v2: the caller
//! owns heavyweight per-simulation domain caches and needs the **ids** that went
//! away in order to evict them, which is what [`SimulationStore::cleanup`]
//! promises. The index is a sorted set scored by each simulation's expiry
//! deadline, so cleanup is a range query over what has actually expired rather
//! than an existence probe per member — no `SMEMBERS` over an unbounded set, and
//! no window in which a recreated id could be reported as expired.

use crate::infrastructure::RedisClient;
use crate::session::model_v2::SessionV2;
use crate::session::store::v2_interface::SimulationStore;
use crate::utils::error::ChainError;
use async_trait::async_trait;
use redis::RedisError;
use std::sync::Arc;
use tracing::{debug, error, info, instrument};
use uuid::Uuid;

/// Default Redis key prefix for v2 simulations.
///
/// Distinct from v1's `session:` / `optionchain:session:` prefixes, which is
/// what keeps the two key spaces from ever meeting.
pub const DEFAULT_V2_KEY_PREFIX: &str = "optionchain:simulation:v2:";

/// Creates a simulation, or reports that the id is taken.
///
/// One unit, so a created document always has its revision key and its index
/// entry. The index score is the expiry deadline, read from Redis's own clock
/// (`TIME`) rather than the client's, so a skewed caller cannot make cleanup
/// reap early or late.
///
/// Keys: `KEYS[1]` document, `KEYS[2]` revision, `KEYS[3]` index.
/// Arguments: `ARGV[1]` document JSON, `ARGV[2]` revision string, `ARGV[3]` TTL
/// in seconds, `ARGV[4]` simulation id.
///
/// Returns `1` when created, `0` when the id already exists.
const CREATE_SCRIPT: &str = r#"
if redis.call('SET', KEYS[1], ARGV[1], 'EX', tonumber(ARGV[3]), 'NX') == false then
    return 0
end
redis.call('SET', KEYS[2], ARGV[2], 'EX', tonumber(ARGV[3]))
local now = tonumber(redis.call('TIME')[1])
redis.call('ZADD', KEYS[3], now + tonumber(ARGV[3]), ARGV[4])
return 1
"#;

/// Compare-and-swap: writes only if the stored revision still matches.
///
/// The revision compared is **not** read from the JSON document. Decoding it
/// with `cjson` would round the integer through an IEEE-754 double, so two
/// adjacent revisions above `2^53` collapse to the same value and a stale writer
/// could pass the check. It lives in a companion key as a plain string and is
/// compared byte for byte. A document written before that key existed has none;
/// `GET` returns `false`, so `or '0'` treats it as revision `0`.
///
/// The same unit refreshes the TTL on both keys and the deadline in the index,
/// so a simulation being actively walked cannot have its index entry go stale
/// and be reaped while its document is still live.
///
/// Keys: `KEYS[1]` document, `KEYS[2]` revision, `KEYS[3]` index.
/// Arguments: `ARGV[1]` new document JSON, `ARGV[2]` expected revision,
/// `ARGV[3]` new revision, `ARGV[4]` TTL in seconds, `ARGV[5]` simulation id.
///
/// Returns `-1` when the document is gone, `-2` on a revision mismatch, `1` when
/// written.
const SAVE_CAS_SCRIPT: &str = r#"
if not redis.call('GET', KEYS[1]) then
    return -1
end
local ver = redis.call('GET', KEYS[2]) or '0'
if ver ~= ARGV[2] then
    return -2
end
redis.call('SET', KEYS[1], ARGV[1], 'EX', tonumber(ARGV[4]))
redis.call('SET', KEYS[2], ARGV[3], 'EX', tonumber(ARGV[4]))
local now = tonumber(redis.call('TIME')[1])
redis.call('ZADD', KEYS[3], now + tonumber(ARGV[4]), ARGV[5])
return 1
"#;

/// Deletes a simulation and every trace of it.
///
/// Keys: `KEYS[1]` document, `KEYS[2]` revision, `KEYS[3]` index.
/// Arguments: `ARGV[1]` simulation id.
///
/// Returns `1` when a document was removed, `0` when there was nothing to
/// remove. The revision key and the index entry go either way, so a partially
/// expired simulation cannot leave debris behind.
const DELETE_SCRIPT: &str = r#"
local removed = redis.call('DEL', KEYS[1])
redis.call('DEL', KEYS[2])
redis.call('ZREM', KEYS[3], ARGV[1])
return removed
"#;

/// Reaps every simulation whose deadline has passed and reports their ids.
///
/// The range query and the removals are one unit, so an id cannot be reported as
/// expired and then immediately recreated by a concurrent `create` — which,
/// because ids are deterministic and repeat across process restarts, is a real
/// interleaving rather than a theoretical one. Reporting a live simulation would
/// make the caller evict a cache it still needs and drop the id from the index
/// permanently.
///
/// Keys: `KEYS[1]` index.
/// Arguments: `ARGV[1]` key prefix, `ARGV[2]` maximum ids to reap in one pass.
///
/// Returns the reaped ids.
const CLEANUP_SCRIPT: &str = r#"
local now = tonumber(redis.call('TIME')[1])
local expired = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', now, 'LIMIT', 0, tonumber(ARGV[2]))
for _, id in ipairs(expired) do
    redis.call('DEL', ARGV[1] .. id)
    redis.call('DEL', ARGV[1] .. id .. ':ver')
    redis.call('ZREM', KEYS[1], id)
end
return expired
"#;

/// Maximum ids one `cleanup` pass reaps.
///
/// Bounds both the script's run time — Redis is single-threaded, so an unbounded
/// loop would block every other client — and the size of the reply. A caller
/// with more than this to reap simply reaps the rest on its next pass.
const CLEANUP_BATCH: usize = 1_000;

/// The key holding a simulation document.
///
/// A free function so the layout can be asserted without a live connection —
/// changing it moves every stored simulation, which is exactly the kind of
/// change that should have to break a test first.
#[must_use]
#[inline]
fn simulation_key(prefix: &str, id: Uuid) -> String {
    format!("{prefix}{id}")
}

/// The companion key holding a simulation's revision as an integer string.
#[must_use]
#[inline]
fn version_key(prefix: &str, id: Uuid) -> String {
    format!("{}:ver", simulation_key(prefix, id))
}

/// The sorted set holding every live simulation id, scored by its deadline.
#[must_use]
#[inline]
fn index_key(prefix: &str) -> String {
    format!("{prefix}index")
}

/// Redis-backed store for v2 rolling simulations.
pub struct InRedisSimulationStore {
    client: Arc<RedisClient>,
    key_prefix: String,
    retention_secs: u64,
}

impl InRedisSimulationStore {
    /// Creates a Redis-backed simulation store.
    ///
    /// `key_prefix` defaults to [`DEFAULT_V2_KEY_PREFIX`] and `retention_secs`
    /// to [`super::v2_memory::DEFAULT_V2_RETENTION_SECS`], the same window the
    /// in-memory store applies — ADR 0001 §9.1 requires the two backends to
    /// agree.
    #[must_use]
    #[instrument(skip(client), level = "debug")]
    pub fn new(
        client: Arc<RedisClient>,
        key_prefix: Option<String>,
        retention_secs: Option<u64>,
    ) -> Self {
        let prefix = key_prefix.unwrap_or_else(|| DEFAULT_V2_KEY_PREFIX.to_string());
        let retention = retention_secs.unwrap_or(super::v2_memory::DEFAULT_V2_RETENTION_SECS);

        info!(
            key_prefix = %prefix,
            retention_secs = retention,
            "Created new Redis simulation store"
        );

        Self {
            client,
            key_prefix: prefix,
            retention_secs: retention,
        }
    }

    /// The key holding a simulation document.
    #[must_use]
    #[inline]
    fn simulation_key(&self, id: Uuid) -> String {
        simulation_key(&self.key_prefix, id)
    }

    /// The companion key holding a simulation's revision as an integer string.
    #[must_use]
    #[inline]
    fn version_key(&self, id: Uuid) -> String {
        version_key(&self.key_prefix, id)
    }

    /// The sorted set holding every live simulation id, scored by its deadline.
    #[must_use]
    #[inline]
    fn index_key(&self) -> String {
        index_key(&self.key_prefix)
    }

    /// The retention window applied to stored simulations, in seconds.
    #[must_use]
    pub fn retention_secs(&self) -> u64 {
        self.retention_secs
    }

    /// Maps a Redis error into the crate's error boundary.
    ///
    /// The message carries the driver's text but never the connection string,
    /// which `RedisClient` keeps to itself.
    #[cold]
    fn map_redis_error(err: RedisError) -> ChainError {
        ChainError::Internal(format!("Redis error: {err}"))
    }

    /// Translates the integer result code returned by [`SAVE_CAS_SCRIPT`].
    ///
    /// Pure and side-effect free so the mapping is unit-testable without a live
    /// Redis. `1` committed, `-1` means the document is gone, `-2` means another
    /// writer won the race.
    fn map_cas_result(code: i64, id: Uuid, expected_version: u64) -> Result<(), ChainError> {
        match code {
            1 => Ok(()),
            -1 => Err(ChainError::NotFound(format!(
                "Simulation with id {id} not found"
            ))),
            -2 => Err(ChainError::Conflict(format!(
                "Simulation {id} was modified concurrently (expected version {expected_version})"
            ))),
            other => Err(ChainError::Internal(format!(
                "Unexpected CAS result code {other} for simulation {id}"
            ))),
        }
    }

    /// Serializes a simulation, reporting a failure through the error boundary.
    fn serialize(simulation: &SessionV2) -> Result<String, ChainError> {
        serde_json::to_string(simulation).map_err(|e| {
            error!(simulation_id = %simulation.id, error = %e, "Failed to serialize simulation");
            ChainError::Internal(format!("Failed to serialize simulation: {e}"))
        })
    }
}

#[async_trait]
impl SimulationStore for InRedisSimulationStore {
    #[instrument(skip(self), level = "debug")]
    async fn get(&self, id: Uuid) -> Result<SessionV2, ChainError> {
        let key = self.simulation_key(id);
        debug!(simulation_id = %id, key = %key, "Getting simulation from Redis");

        match self.client.get::<String>(&key).await {
            // Deserialization runs the document through the same validation a
            // request goes through, so a corrupted or hand-edited document is a
            // typed error here rather than a silently degraded simulation later.
            Ok(Some(json)) => serde_json::from_str::<SessionV2>(&json).map_err(|e| {
                error!(simulation_id = %id, error = %e, "Failed to deserialize simulation");
                ChainError::Internal(format!("Failed to deserialize simulation: {e}"))
            }),
            Ok(None) => Err(ChainError::NotFound(format!(
                "Simulation with id {id} not found"
            ))),
            Err(e) => {
                error!(simulation_id = %id, error = %e, "Redis error while getting simulation");
                Err(Self::map_redis_error(e))
            }
        }
    }

    #[instrument(skip(self, simulation), level = "debug")]
    async fn create(&self, simulation: SessionV2) -> Result<(), ChainError> {
        let id = simulation.id;
        debug!(simulation_id = %id, "Creating simulation in Redis");

        // Redis only rechecks on the way out, so without this a document
        // written invalid stays readable-but-rejected until its TTL expires.
        simulation.validate()?;

        let json = Self::serialize(&simulation)?;
        let mut conn = self.client.connection_manager();
        let created: i64 = redis::Script::new(CREATE_SCRIPT)
            .key(self.simulation_key(id))
            .key(self.version_key(id))
            .key(self.index_key())
            .arg(json)
            .arg(simulation.version.to_string())
            .arg(self.retention_secs)
            .arg(id.to_string())
            .invoke_async(&mut conn)
            .await
            .map_err(Self::map_redis_error)?;

        if created == 1 {
            debug!(simulation_id = %id, "Simulation created successfully");
            Ok(())
        } else {
            Err(ChainError::AlreadyExists(format!(
                "Simulation with id {id} already exists"
            )))
        }
    }

    #[instrument(skip(self, simulation), level = "debug")]
    async fn save_cas(
        &self,
        simulation: SessionV2,
        expected_version: u64,
    ) -> Result<(), ChainError> {
        let id = simulation.id;
        debug!(simulation_id = %id, expected_version, "CAS-saving simulation to Redis");

        simulation.validate()?;

        let json = Self::serialize(&simulation)?;
        let mut conn = self.client.connection_manager();
        let code: i64 = redis::Script::new(SAVE_CAS_SCRIPT)
            .key(self.simulation_key(id))
            .key(self.version_key(id))
            .key(self.index_key())
            .arg(json)
            .arg(expected_version.to_string())
            .arg(simulation.version.to_string())
            .arg(self.retention_secs)
            .arg(id.to_string())
            .invoke_async(&mut conn)
            .await
            .map_err(Self::map_redis_error)?;

        Self::map_cas_result(code, id, expected_version).inspect_err(|e| {
            debug!(simulation_id = %id, error = %e, "CAS save rejected");
        })
    }

    #[instrument(skip(self), level = "debug")]
    async fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
        debug!(simulation_id = %id, "Deleting simulation from Redis");

        let mut conn = self.client.connection_manager();
        let removed: i64 = redis::Script::new(DELETE_SCRIPT)
            .key(self.simulation_key(id))
            .key(self.version_key(id))
            .key(self.index_key())
            .arg(id.to_string())
            .invoke_async(&mut conn)
            .await
            .map_err(Self::map_redis_error)?;

        debug!(simulation_id = %id, removed, "Simulation delete result");
        Ok(removed > 0)
    }

    #[instrument(skip(self), level = "debug")]
    async fn cleanup(&self) -> Result<Vec<Uuid>, ChainError> {
        let mut conn = self.client.connection_manager();
        let reaped: Vec<String> = redis::Script::new(CLEANUP_SCRIPT)
            .key(self.index_key())
            .arg(&self.key_prefix)
            .arg(CLEANUP_BATCH)
            .invoke_async(&mut conn)
            .await
            .map_err(Self::map_redis_error)?;

        // A member that is not a uuid cannot correspond to a document. The
        // script has already removed it, so simply do not report it.
        let expired: Vec<Uuid> = reaped
            .iter()
            .filter_map(|member| Uuid::parse_str(member).ok())
            .collect();

        if !expired.is_empty() {
            info!(count = expired.len(), "Reaped expired simulations");
        }
        Ok(expired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any_id() -> Uuid {
        Uuid::nil()
    }

    /// A committed compare-and-swap maps to `Ok`.
    #[test]
    fn test_map_cas_result_committed_is_ok() {
        assert!(InRedisSimulationStore::map_cas_result(1, any_id(), 3).is_ok());
    }

    /// A missing key maps to `NotFound`, naming the simulation.
    #[test]
    fn test_map_cas_result_missing_key_is_not_found() {
        match InRedisSimulationStore::map_cas_result(-1, any_id(), 3) {
            Err(ChainError::NotFound(message)) => assert!(message.contains("Simulation")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// A revision mismatch maps to `Conflict`, carrying the expected revision.
    #[test]
    fn test_map_cas_result_version_mismatch_is_conflict() {
        match InRedisSimulationStore::map_cas_result(-2, any_id(), 7) {
            Err(ChainError::Conflict(message)) => assert!(message.contains('7')),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    /// An unexpected code is an internal error rather than a silent success.
    #[test]
    fn test_map_cas_result_unknown_code_is_internal() {
        match InRedisSimulationStore::map_cas_result(42, any_id(), 0) {
            Err(ChainError::Internal(message)) => assert!(message.contains("42")),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    /// The key layout is stable: a change here moves every stored simulation.
    #[test]
    fn test_key_layout_is_stable() {
        let id = any_id();

        assert_eq!(
            simulation_key(DEFAULT_V2_KEY_PREFIX, id),
            "optionchain:simulation:v2:00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(
            version_key(DEFAULT_V2_KEY_PREFIX, id),
            "optionchain:simulation:v2:00000000-0000-0000-0000-000000000000:ver"
        );
        assert_eq!(
            index_key(DEFAULT_V2_KEY_PREFIX),
            "optionchain:simulation:v2:index"
        );
    }

    /// The v2 prefix cannot collide with v1's key space.
    #[test]
    fn test_v2_prefix_is_disjoint_from_v1() {
        for v1_prefix in ["session:", "optionchain:session:"] {
            assert!(
                !DEFAULT_V2_KEY_PREFIX.starts_with(v1_prefix),
                "the v2 prefix must not live inside the v1 key space"
            );
            assert!(
                !v1_prefix.starts_with(DEFAULT_V2_KEY_PREFIX),
                "the v1 prefix must not live inside the v2 key space"
            );
        }
    }
}

/// Live integration tests for the Redis store.
///
/// The scripts are the whole point of this module — atomic create, the CAS,
/// and the deadline-scored index that makes `cleanup` report real ids — and
/// none of that can be exercised against a mock without testing the mock. They
/// are `#[ignore]`d so the default suite stays hermetic, and CI runs them in
/// the dedicated Integration job, which provisions a `redis:7.4` service.
///
/// Every test uses a unique key prefix so parallel runs cannot collide, and
/// cleans up after itself.
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
    use crate::api::rest::requests_v2::CreateSimulationRequest;
    use crate::infrastructure::RedisConfig;
    use crate::session::model::SessionState;
    use crate::session::model_v2::SimulationParametersV2;
    use crate::session::{ExpiryRule, ExpiryRuleKind};
    use chrono::{TimeZone, Utc, Weekday};
    use tokio::test;

    /// Builds a store against the provisioned Redis, under its own key prefix.
    async fn store(prefix: &str, retention_secs: u64) -> InRedisSimulationStore {
        let client = RedisClient::new(RedisConfig::default())
            .await
            .expect("the provisioned Redis must accept a connection");
        InRedisSimulationStore::new(
            Arc::new(client),
            Some(format!("ocs:test:{prefix}:")),
            Some(retention_secs),
        )
    }

    fn simulation(seed: u64) -> SessionV2 {
        let rule = ExpiryRule::new("zero_dte", ExpiryRuleKind::Daily, 1)
            .expect("the test rule must be valid");
        let weeklies = ExpiryRule::new(
            "weeklies",
            ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Fri]),
            2,
        )
        .expect("the test rule must be valid");
        let start_at = Utc
            .with_ymd_and_hms(2026, 1, 5, 14, 30, 0)
            .single()
            .expect("the test instant must be valid");

        let request = CreateSimulationRequest {
            symbol: "SPX".to_string(),
            steps: 10,
            start_at: Some(start_at),
            step_interval_seconds: Some(86_400),
            timezone: "America/New_York".to_string(),
            calendar: None,
            expiration_time: "17:00".to_string(),
            schedules: vec![rule, weeklies],
            initial_price: 5000.0,
            volatility: 0.18,
            risk_free_rate: 0.04,
            dividend_yield: 0.0,
            method: ApiWalkType::Brownian {
                dt: 0.004,
                drift: 0.0,
                volatility: 0.18,
            },
            time_frame: ApiTimeFrame::Day,
            chain_size: Some(15),
            strike_interval: Some(25.0),
            skew_slope: None,
            smile_curve: None,
            spread: Some(0.02),
            seed: Some(seed),
        };

        let parameters =
            SimulationParametersV2::try_from(request).expect("the reference request must convert");
        // Ids are random now, so each test already gets its own.
        SessionV2::new(parameters)
    }

    /// A created simulation round-trips through Redis unchanged, and its
    /// document survives the validating deserialization on the way back.
    #[test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_create_then_get_round_trips_through_redis() {
        let store = store("roundtrip", 60).await;
        let original = simulation(1);

        store.create(original.clone()).await.expect("must create");
        let loaded = store.get(original.id).await.expect("must load");
        assert_eq!(loaded, original);

        store.delete(original.id).await.expect("must delete");
    }

    /// `create` is atomic: the document, its revision key and its index entry
    /// all exist afterwards, so `cleanup` can never lose track of it.
    #[test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_create_writes_document_revision_and_index_together() {
        let store = store("atomic", 60).await;
        let sim = simulation(2);

        store.create(sim.clone()).await.expect("must create");

        assert!(
            store
                .client
                .exists(&store.simulation_key(sim.id))
                .await
                .expect("must query"),
            "the document must exist"
        );
        assert!(
            store
                .client
                .exists(&store.version_key(sim.id))
                .await
                .expect("must query"),
            "the revision key must exist"
        );

        let mut conn = store.client.connection_manager();
        let score: Option<f64> = redis::cmd("ZSCORE")
            .arg(store.index_key())
            .arg(sim.id.to_string())
            .query_async(&mut conn)
            .await
            .expect("must query");
        assert!(score.is_some(), "the index entry must exist");

        store.delete(sim.id).await.expect("must delete");
    }

    /// A duplicate id is rejected without touching the stored document.
    #[test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_duplicate_id_is_rejected() {
        let store = store("duplicate", 60).await;
        let sim = simulation(3);

        store.create(sim.clone()).await.expect("must create");
        match store.create(sim.clone()).await {
            Err(ChainError::AlreadyExists(_)) => {}
            other => panic!("expected AlreadyExists, got {other:?}"),
        }

        let loaded = store.get(sim.id).await.expect("must load");
        assert_eq!(loaded.version, sim.version);

        store.delete(sim.id).await.expect("must delete");
    }

    /// The compare-and-swap commits at the stored revision and rejects a stale
    /// one, so two concurrent advances produce one winner.
    #[test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_save_cas_commits_once_and_rejects_the_loser() {
        let store = store("cas", 60).await;
        let sim = simulation(4);
        store.create(sim.clone()).await.expect("must create");

        let mut winner = sim.clone();
        winner.current_step = 1;
        winner.state = SessionState::InProgress;
        let winner_expected = winner.bump_version().expect("must bump");

        let mut loser = sim.clone();
        loser.current_step = 1;
        loser.state = SessionState::InProgress;
        let loser_expected = loser.bump_version().expect("must bump");

        store
            .save_cas(winner, winner_expected)
            .await
            .expect("the first writer must commit");
        match store.save_cas(loser, loser_expected).await {
            Err(ChainError::Conflict(_)) => {}
            other => panic!("expected Conflict, got {other:?}"),
        }

        let loaded = store.get(sim.id).await.expect("must load");
        assert_eq!(loaded.current_step, 1);
        assert_eq!(loaded.version, 1);

        store.delete(sim.id).await.expect("must delete");
    }

    /// A compare-and-swap against a missing id is `NotFound`, not a silent
    /// insert.
    #[test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_save_cas_on_a_missing_id_is_not_found() {
        let store = store("cas-missing", 60).await;

        match store.save_cas(simulation(5), 0).await {
            Err(ChainError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// Delete removes the document, its revision key and its index entry, and a
    /// second delete is not an error.
    #[test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_delete_removes_every_trace() {
        let store = store("delete", 60).await;
        let sim = simulation(6);
        store.create(sim.clone()).await.expect("must create");

        assert!(store.delete(sim.id).await.expect("must delete"));
        assert!(!store.delete(sim.id).await.expect("a second delete is fine"));

        assert!(
            !store
                .client
                .exists(&store.version_key(sim.id))
                .await
                .expect("must query"),
            "the revision key must be gone"
        );
        let mut conn = store.client.connection_manager();
        let score: Option<f64> = redis::cmd("ZSCORE")
            .arg(store.index_key())
            .arg(sim.id.to_string())
            .query_async(&mut conn)
            .await
            .expect("must query");
        assert!(score.is_none(), "the index entry must be gone");
    }

    /// Cleanup reports the ids whose deadline has passed, and leaves a live
    /// simulation alone — the property #48's cache eviction depends on.
    #[test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_cleanup_reports_expired_ids_and_spares_live_ones() {
        // A one-second retention so the deadline passes within the test.
        let short_lived = store("cleanup", 1).await;
        let stale = simulation(7);
        short_lived
            .create(stale.clone())
            .await
            .expect("must create");

        // Redis scores in whole seconds, so wait past the deadline boundary.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let long_lived = store("cleanup", 3_600).await;
        let fresh = simulation(8);
        long_lived.create(fresh.clone()).await.expect("must create");

        let expired = short_lived.cleanup().await.expect("must clean up");
        assert!(
            expired.contains(&stale.id),
            "the expired simulation must be reported"
        );
        assert!(
            !expired.contains(&fresh.id),
            "a live simulation must not be reported"
        );

        assert!(long_lived.get(fresh.id).await.is_ok());
        long_lived.delete(fresh.id).await.expect("must delete");
    }

    /// Cleanup is idempotent: a second pass reports nothing, because the index
    /// entry went with the first.
    #[test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_cleanup_is_idempotent() {
        let store = store("cleanup-twice", 1).await;
        store.create(simulation(9)).await.expect("must create");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let first = store.cleanup().await.expect("must clean up");
        assert_eq!(first.len(), 1);
        let second = store.cleanup().await.expect("must clean up");
        assert!(second.is_empty());
    }

    /// A stored document that fails validation is a typed error on load, not a
    /// silently degraded simulation. This is the stored-input half of the
    /// boundary rule: Redis is an outer layer and is not trusted.
    #[test]
    #[ignore = "requires a live Redis matching REDIS_*; run with -- --ignored"]
    async fn test_corrupted_document_is_rejected_on_load() {
        let store = store("corrupt", 60).await;
        let sim = simulation(10);
        store.create(sim.clone()).await.expect("must create");

        // Freeze the simulated clock behind the store's back.
        let json = serde_json::to_string(&sim).expect("must serialize");
        let tampered = json.replace(
            "\"step_interval_seconds\":86400",
            "\"step_interval_seconds\":0",
        );
        assert_ne!(tampered, json, "the tamper must have applied");
        store
            .client
            .set(&store.simulation_key(sim.id), tampered, Some(60))
            .await
            .expect("must write");

        match store.get(sim.id).await {
            Err(ChainError::Internal(message)) => {
                assert!(message.contains("step_interval_seconds"), "got {message}");
            }
            other => panic!("expected the load to be rejected, got {other:?}"),
        }

        store.delete(sim.id).await.expect("must delete");
    }
}
