//! What the readiness endpoint asks, and of whom.
//!
//! The probes live here, beside the clients they call, rather than beside the
//! handler that serves them: asking Redis whether it is there is infrastructure
//! work, and putting it in `api` would have that layer reaching into this one's
//! internals — the credential redaction, the driver error types — to do it.
//! `api` keeps the routes, the response shape and the status codes, and this
//! module answers the only question they need: which dependencies answered.
//!
//! # Bounded, concurrent, and never cached
//!
//! Every probe runs under a two-second bound and all of them run at once, so a
//! single hung dependency can neither hold the probe open past that bound nor
//! hide the state of the others. Nothing is cached: an instance whose Redis
//! came back must report itself ready again WITHOUT a restart, which is exactly
//! what a cached answer would prevent. The only thing remembered between calls
//! is what was last LOGGED, so an outage costs one line rather than one line
//! per probe per ten seconds forever.
//!
//! # What reaches the response body
//!
//! A FIXED CATEGORY, and nothing else. `/ready` is unauthenticated, so a
//! driver's own words must not reach it: a server message can carry internal
//! host names, database paths, TLS file paths, query text and tokens in forms
//! no redaction routine knows to look for, and redacting URL userinfo covers
//! only the shape it was written for.
//!
//! A report therefore carries [`ProbeFailure`], which has exactly two values —
//! the dependency did not answer, or it did not answer in time — and both are
//! facts this module owns rather than text it parsed. The driver's explanation
//! stays inside the process, credential-redacted and length-bounded, in the
//! transition log.

use crate::infrastructure::config::redact_userinfo;
use crate::infrastructure::{MongoDBRepository, RedisClient, SimulationSnapshotRepository};
use async_trait::async_trait;
use futures::future::join_all;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{info, warn};

/// How long a single dependency has to answer.
///
/// Short on purpose. A probe is asked repeatedly and its answer is only useful
/// while it is current, so a dependency that has not answered in this long is
/// one this instance could not serve a request through either.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How much of a failure explanation reaches the response body.
///
/// A driver error can be an unbounded server exception, and this one goes into
/// an unauthenticated body. Enough to name the problem, not enough to be a
/// payload.
pub(crate) const MAX_DETAIL_CHARS: usize = 200;

/// Why a dependency did not answer, in terms safe to publish.
///
/// An allowlist, deliberately small: each value is decided by WHICH branch of
/// the probe fired, never by inspecting what a server said. Adding a value
/// means deciding, once and in the open, that the new fact is safe to tell an
/// unauthenticated caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailure {
    /// It refused, failed, or could not be reached at all.
    Unreachable,
    /// It did not answer inside the probe's bound.
    TimedOut,
}

impl ProbeFailure {
    /// The public wording, which is the whole of what a caller learns.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeFailure::Unreachable => "unreachable",
            ProbeFailure::TimedOut => "timed_out",
        }
    }
}

/// One dependency's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DependencyReport {
    /// What was probed: `redis`, `mongodb` or `clickhouse`.
    pub name: &'static str,
    /// Why it did not answer, absent when it did. A category, never a message.
    pub failure: Option<ProbeFailure>,
}

impl DependencyReport {
    /// Whether this dependency answered.
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.failure.is_none()
    }
}

/// One thing readiness asks before reporting an instance able to take work.
#[async_trait]
pub trait DependencyProbe: Send + Sync {
    /// The name this dependency is reported under.
    fn name(&self) -> &'static str;

    /// Asks it whether it is there.
    ///
    /// # Errors
    ///
    /// Returns a short explanation when it is not. The caller redacts and
    /// bounds it before it reaches a response body, so an implementation may
    /// return whatever the driver said.
    async fn check(&self) -> Result<(), String>;
}

/// Redis, which every session and every v2 simulation is stored in.
pub struct RedisProbe(Arc<RedisClient>);

impl RedisProbe {
    /// Probes the given client.
    #[must_use]
    pub fn new(client: Arc<RedisClient>) -> Self {
        Self(client)
    }
}

#[async_trait]
impl DependencyProbe for RedisProbe {
    fn name(&self) -> &'static str {
        "redis"
    }

    async fn check(&self) -> Result<(), String> {
        self.0.ping().await.map_err(|error| error.to_string())
    }
}

/// MongoDB, which the v1 event log is written to.
pub struct MongoDbProbe(Arc<MongoDBRepository>);

impl MongoDbProbe {
    /// Probes the given repository.
    #[must_use]
    pub fn new(repository: Arc<MongoDBRepository>) -> Self {
        Self(repository)
    }
}

#[async_trait]
impl DependencyProbe for MongoDbProbe {
    fn name(&self) -> &'static str {
        "mongodb"
    }

    async fn check(&self) -> Result<(), String> {
        self.0.ping().await.map_err(|error| error.to_string())
    }
}

/// The ClickHouse snapshot warehouse, registered only when persistence is on.
pub struct WarehouseProbe(Arc<dyn SimulationSnapshotRepository>);

impl WarehouseProbe {
    /// Probes the given warehouse.
    #[must_use]
    pub fn new(warehouse: Arc<dyn SimulationSnapshotRepository>) -> Self {
        Self(warehouse)
    }
}

#[async_trait]
impl DependencyProbe for WarehouseProbe {
    fn name(&self) -> &'static str {
        "clickhouse"
    }

    async fn check(&self) -> Result<(), String> {
        self.0.ping().await.map_err(|error| error.to_string())
    }
}

/// What readiness checks.
///
/// Assembled once at startup from the dependencies the deployment actually
/// configured, so an instance is never reported unready over a service it does
/// not use, and never reported ready without one it does.
#[derive(Clone, Default)]
pub struct Readiness {
    probes: Arc<Vec<Arc<dyn DependencyProbe>>>,
    /// The last readiness that was LOGGED, so a state that has not changed is
    /// not restated on every probe. Not a cached answer: it never decides what
    /// [`Readiness::evaluate`] returns.
    last_logged_ready: Arc<AtomicBool>,
}

impl Readiness {
    /// Collects the probes to run, in the order they will be reported.
    #[must_use]
    pub fn new(probes: Vec<Arc<dyn DependencyProbe>>) -> Self {
        Self {
            probes: Arc::new(probes),
            // Starts ready, so the first FAILURE is a transition worth a line
            // and a healthy start is silent.
            last_logged_ready: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Runs every probe at once and collects the answers.
    ///
    /// Bounded: a hung dependency costs one timeout for the whole call, not one
    /// per dependency, and does not stop the others from being reported.
    pub async fn evaluate(&self) -> Vec<DependencyReport> {
        let answers = join_all(self.probes.iter().map(|probe| async move {
            let name = probe.name();
            // The category comes from which arm fired, and the driver's own
            // words go no further than the internal log beside it.
            match tokio::time::timeout(PROBE_TIMEOUT, probe.check()).await {
                Ok(Ok(())) => (name, None, None),
                Ok(Err(detail)) => (
                    name,
                    Some(ProbeFailure::Unreachable),
                    Some(bound_detail(&redact_userinfo(&detail))),
                ),
                Err(_) => (name, Some(ProbeFailure::TimedOut), None),
            }
        }))
        .await;

        let reports: Vec<DependencyReport> = answers
            .iter()
            .map(|(name, failure, _)| DependencyReport {
                name,
                failure: *failure,
            })
            .collect();

        self.log_transition(&answers);
        reports
    }

    /// Says something only when the answer CHANGED.
    ///
    /// The endpoint is polled forever by design, so logging every failure would
    /// emit a line per dependency every few seconds for as long as an outage
    /// lasts, which is what teaches an operator to filter the log.
    fn log_transition(&self, answers: &[(&'static str, Option<ProbeFailure>, Option<String>)]) {
        let ready = answers.iter().all(|(_, failure, _)| failure.is_none());
        if self.last_logged_ready.swap(ready, Ordering::SeqCst) == ready {
            return;
        }

        if ready {
            info!("every dependency answered again; this instance is ready");
            return;
        }

        // The one place a driver's explanation is kept, and it never leaves the
        // process. Redacted and bounded even here: logs get shipped.
        for (name, failure, detail) in answers.iter().filter(|(_, failure, _)| failure.is_some()) {
            warn!(
                dependency = name,
                reason = failure.map_or("", ProbeFailure::as_str),
                detail = detail.as_deref().unwrap_or(""),
                "a dependency stopped answering; this instance is not ready"
            );
        }
    }
}

/// Trims a failure explanation to what a response body should carry.
fn bound_detail(detail: &str) -> String {
    if detail.chars().count() <= MAX_DETAIL_CHARS {
        return detail.to_string();
    }

    let kept: String = detail.chars().take(MAX_DETAIL_CHARS).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::clickhouse::snapshots::interface::ContractSeriesQuery;
    use crate::infrastructure::{ContractQuote, SnapshotRecord};
    use crate::utils::ChainError;
    use uuid::Uuid;

    /// A probe whose answer the test controls, and can change mid-flight.
    struct Switch {
        name: &'static str,
        up: AtomicBool,
        detail: String,
    }

    impl Switch {
        fn new(name: &'static str, up: bool) -> Self {
            Self {
                name,
                up: AtomicBool::new(up),
                detail: "connection refused".to_string(),
            }
        }

        fn failing_with(name: &'static str, detail: &str) -> Self {
            Self {
                name,
                up: AtomicBool::new(false),
                detail: detail.to_string(),
            }
        }
    }

    #[async_trait]
    impl DependencyProbe for Switch {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn check(&self) -> Result<(), String> {
            if self.up.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(self.detail.clone())
            }
        }
    }

    /// A probe that never answers, standing in for a hung dependency.
    struct Hangs;

    #[async_trait]
    impl DependencyProbe for Hangs {
        fn name(&self) -> &'static str {
            "stalled"
        }

        async fn check(&self) -> Result<(), String> {
            tokio::time::sleep(PROBE_TIMEOUT * 10).await;
            Ok(())
        }
    }

    /// A warehouse that is unreachable, for the shipped [`WarehouseProbe`].
    struct UnreachableWarehouse;

    #[async_trait]
    impl SimulationSnapshotRepository for UnreachableWarehouse {
        async fn ping(&self) -> Result<(), ChainError> {
            Err(ChainError::ClickHouseError(
                "connection refused by clickhouse:8123".to_string(),
            ))
        }

        async fn persist(&self, _record: SnapshotRecord) -> Result<(), ChainError> {
            Err(ChainError::ClickHouseError("unreachable".to_string()))
        }

        async fn get(
            &self,
            _simulation: Uuid,
            _generation: u64,
            _step: usize,
        ) -> Result<Option<SnapshotRecord>, ChainError> {
            Err(ChainError::ClickHouseError("unreachable".to_string()))
        }

        async fn read_range(
            &self,
            _simulation: Uuid,
            _generation: u64,
            _from_step: usize,
            _to_step: usize,
        ) -> Result<Vec<SnapshotRecord>, ChainError> {
            Err(ChainError::ClickHouseError("unreachable".to_string()))
        }

        async fn contract_series(
            &self,
            _query: ContractSeriesQuery,
        ) -> Result<Vec<ContractQuote>, ChainError> {
            Err(ChainError::ClickHouseError("unreachable".to_string()))
        }
    }

    /// A warehouse with nothing behind it, which is reachable by construction.
    #[derive(Default)]
    struct LocalWarehouse;

    #[async_trait]
    impl SimulationSnapshotRepository for LocalWarehouse {
        async fn ping(&self) -> Result<(), ChainError> {
            Ok(())
        }

        async fn persist(&self, _record: SnapshotRecord) -> Result<(), ChainError> {
            Ok(())
        }

        async fn get(
            &self,
            _simulation: Uuid,
            _generation: u64,
            _step: usize,
        ) -> Result<Option<SnapshotRecord>, ChainError> {
            Ok(None)
        }

        async fn read_range(
            &self,
            _simulation: Uuid,
            _generation: u64,
            _from_step: usize,
            _to_step: usize,
        ) -> Result<Vec<SnapshotRecord>, ChainError> {
            Ok(Vec::new())
        }

        async fn contract_series(
            &self,
            _query: ContractSeriesQuery,
        ) -> Result<Vec<ContractQuote>, ChainError> {
            Ok(Vec::new())
        }
    }

    /// Every probe is reported, in the order it was registered.
    #[tokio::test]
    async fn test_every_probe_is_reported_in_order() {
        let readiness = Readiness::new(vec![
            Arc::new(Switch::new("redis", true)),
            Arc::new(Switch::new("mongodb", true)),
            Arc::new(Switch::new("clickhouse", true)),
        ]);

        let reports = readiness.evaluate().await;

        assert_eq!(
            reports.iter().map(|report| report.name).collect::<Vec<_>>(),
            vec!["redis", "mongodb", "clickhouse"]
        );
        assert!(reports.iter().all(DependencyReport::is_up));
    }

    /// A failing dependency carries its reason, and the healthy ones survive.
    #[tokio::test]
    async fn test_a_failure_is_named_beside_the_healthy_dependencies() {
        let readiness = Readiness::new(vec![
            Arc::new(Switch::new("redis", false)),
            Arc::new(Switch::new("mongodb", true)),
        ]);

        let reports = readiness.evaluate().await;

        match reports.iter().find(|report| report.name == "redis") {
            Some(redis) => assert_eq!(redis.failure, Some(ProbeFailure::Unreachable)),
            None => panic!("the failing dependency must be reported: {reports:?}"),
        }
        match reports.iter().find(|report| report.name == "mongodb") {
            Some(mongodb) => assert!(mongodb.is_up()),
            None => panic!("every dependency must be reported: {reports:?}"),
        }
    }

    /// A dependency that comes back flips the answer, with no restart.
    #[tokio::test]
    async fn test_a_recovered_dependency_reports_up_again() {
        let switch = Arc::new(Switch::new("redis", false));
        let readiness = Readiness::new(vec![switch.clone()]);

        assert!(!readiness.evaluate().await[0].is_up());
        switch.up.store(true, Ordering::SeqCst);
        assert!(
            readiness.evaluate().await[0].is_up(),
            "nothing may be cached across evaluations"
        );
    }

    /// A hung dependency times out rather than hanging the probe, and does not
    /// hold up the ones that answered.
    #[tokio::test(start_paused = true)]
    async fn test_a_hung_dependency_times_out() {
        let readiness = Readiness::new(vec![Arc::new(Hangs), Arc::new(Switch::new("redis", true))]);

        let reports = readiness.evaluate().await;

        match reports.iter().find(|report| report.name == "stalled") {
            Some(stalled) => assert_eq!(
                stalled.failure,
                Some(ProbeFailure::TimedOut),
                "the report says it timed out, not that it refused"
            ),
            None => panic!("the hung dependency must be reported: {reports:?}"),
        }
        match reports.iter().find(|report| report.name == "redis") {
            Some(redis) => assert!(redis.is_up(), "the probes run at once"),
            None => panic!("every dependency must be reported: {reports:?}"),
        }
    }

    /// A driver's own words never reach a report, whatever a probe returns.
    ///
    /// The report is what an unauthenticated 503 is rendered from, so the only
    /// thing it may carry is the category. A server message can name internal
    /// hosts, paths, queries and tokens that no redaction routine reliably
    /// recognises, which is why none of it is published rather than published
    /// after a scrub.
    #[tokio::test]
    async fn test_a_report_carries_no_driver_text() {
        let leaky = "IO error: redis://admin:hunter2@10.0.0.7:6379/prod?tls_cert=/etc/ssl/k.pem";
        let readiness = Readiness::new(vec![Arc::new(Switch::failing_with("redis", leaky))]);

        let reports = readiness.evaluate().await;

        assert_eq!(reports[0].failure, Some(ProbeFailure::Unreachable));
        let rendered = format!("{reports:?}");
        for secret in ["hunter2", "10.0.0.7", "/etc/ssl/k.pem", "prod"] {
            assert!(
                !rendered.contains(secret),
                "the report leaked {secret:?}: {rendered}"
            );
        }
    }

    /// The two public categories are the whole vocabulary.
    #[tokio::test]
    async fn test_the_public_vocabulary_is_two_words() {
        assert_eq!(ProbeFailure::Unreachable.as_str(), "unreachable");
        assert_eq!(ProbeFailure::TimedOut.as_str(), "timed_out");
    }

    /// The internal detail is redacted and bounded before it is logged.
    ///
    /// It never reaches a response, but logs get shipped, so the sanitiser
    /// still runs over it.
    #[tokio::test]
    async fn test_the_internal_detail_is_sanitised() {
        let redacted = bound_detail(&crate::infrastructure::config::redact_userinfo(
            "IO error: redis://admin:hunter2@redis:6379 refused",
        ));
        assert!(!redacted.contains("hunter2"), "the password survived");
        assert!(redacted.contains("refused"), "the reason was lost");

        let long = bound_detail(&"x".repeat(MAX_DETAIL_CHARS * 5));
        assert_eq!(long.chars().count(), MAX_DETAIL_CHARS + 3);
        assert!(long.ends_with("..."));

        assert_eq!(bound_detail("connection refused"), "connection refused");
    }

    /// With nothing configured to check, an instance is ready.
    ///
    /// Not degenerate: a deployment without snapshot persistence registers no
    /// warehouse probe, and the same reasoning scales down.
    #[tokio::test]
    async fn test_no_probes_is_ready() {
        assert!(Readiness::default().evaluate().await.is_empty());
    }

    /// The shipped warehouse probe reports the warehouse's own failure.
    #[tokio::test]
    async fn test_the_warehouse_probe_reports_an_unreachable_warehouse() {
        let probe = WarehouseProbe::new(Arc::new(UnreachableWarehouse));

        assert_eq!(probe.name(), "clickhouse");
        match probe.check().await {
            Ok(()) => panic!("an unreachable warehouse must not report up"),
            Err(detail) => assert!(
                detail.contains("connection refused"),
                "the warehouse's reason must survive: {detail}"
            ),
        }
    }

    /// A warehouse with no server behind it answers, which is the truth.
    #[tokio::test]
    async fn test_the_warehouse_probe_accepts_a_local_warehouse() {
        let probe = WarehouseProbe::new(Arc::new(LocalWarehouse));

        match probe.check().await {
            Ok(()) => {}
            Err(detail) => panic!("a local warehouse is reachable: {detail}"),
        }
    }

    /// The Redis probe answers against a live server.
    ///
    /// Opt-in (`cargo test -- --ignored`), like every other live-service test:
    /// `docker run -d --rm -p 6379:6379 redis:8`.
    #[tokio::test]
    #[ignore = "requires a live Redis on localhost:6379"]
    async fn test_the_redis_probe_answers_against_a_live_server() {
        let client = match RedisClient::new(crate::infrastructure::RedisConfig::default()).await {
            Ok(client) => Arc::new(client),
            Err(error) => panic!("Redis must be reachable for this test: {error}"),
        };
        let probe = RedisProbe::new(client);

        assert_eq!(probe.name(), "redis");
        match probe.check().await {
            Ok(()) => {}
            Err(detail) => panic!("a live server must answer: {detail}"),
        }
    }

    /// The MongoDB probe answers against a live server.
    ///
    /// Opt-in: `docker run -d --rm -p 27017:27017 mongo:7`.
    #[tokio::test]
    #[ignore = "requires a live MongoDB on localhost:27017"]
    async fn test_the_mongodb_probe_answers_against_a_live_server() {
        let repository = match crate::infrastructure::init_mongodb().await {
            Ok(repository) => repository,
            Err(error) => panic!("MongoDB must be reachable for this test: {error}"),
        };
        let probe = MongoDbProbe::new(repository);

        assert_eq!(probe.name(), "mongodb");
        match probe.check().await {
            Ok(()) => {}
            Err(detail) => panic!("a live server must answer: {detail}"),
        }
    }
}
