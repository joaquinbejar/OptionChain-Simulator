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
//! it is written to behave like one. Every failure degrades to the behaviour
//! without it: a cache that cannot be reached is a cache miss, never a failed
//! request.
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
}

impl RedisTapeCache {
    /// Builds a cache over `client`.
    ///
    /// `ttl` should be the simulation retention window: a tape outliving the
    /// simulation it belongs to is memory nobody can use, and one expiring
    /// before it costs a rebuild.
    #[must_use]
    pub fn new(client: Arc<RedisClient>, prefix: impl Into<String>, ttl: Duration) -> Self {
        Self {
            client,
            prefix: prefix.into(),
            ttl,
        }
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
        if let Err(error) = self
            .client
            .set(&key, encoded.to_string(), Some(seconds))
            .await
        {
            warn!(%error, %key, "the shared tape cache could not be written");
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
