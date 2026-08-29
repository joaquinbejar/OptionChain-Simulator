//! Liveness and readiness probes.
//!
//! Before these existed the only endpoint an orchestrator could point at was
//! `GET /metrics`, which answers 200 as soon as the process is up. That works
//! as a probe by accident: it is a Prometheus surface, its availability says
//! nothing about the service being able to serve a simulation, and anything
//! written against it breaks the moment the metrics surface moves or is gated.
//!
//! Two endpoints, because they answer two different questions:
//!
//! - **`GET /health`** — is the process alive? Always 200, always cheap, no
//!   dependency touched. A failing dependency must NOT make this fail, or an
//!   orchestrator restarts a healthy instance every time Redis hiccups, which
//!   turns one outage into two.
//! - **`GET /ready`** — can this instance take work? 200 only when every
//!   dependency it needs answers, 503 naming the ones that did not. This is the
//!   signal a batch consumer sharding tape materialisation across instances
//!   reads to decide where to send the next simulation.
//!
//! What `/ready` actually asks lives in [`crate::infrastructure::Readiness`],
//! beside the clients it calls. This module owns the routes, the response
//! shapes and the status codes, and renders one into the other.

use crate::infrastructure::{DependencyReport, Readiness};
use actix_web::{HttpResponse, Responder, web};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The liveness route.
pub(crate) const HEALTH_PATH: &str = "/health";

/// The readiness route.
pub(crate) const READY_PATH: &str = "/ready";

/// The two probe routes, excluded from the request metrics.
///
/// An orchestrator probes every few seconds forever, so counting those requests
/// would bury the traffic the metrics exist to describe under a constant that
/// says nothing, and pour 503s into the error series for as long as a
/// dependency is down. Held here, beside the routes themselves, and handed to
/// the metrics middleware at startup rather than hardcoded inside it: the paths
/// belong to the API surface that defines them.
pub(crate) const PROBE_PATHS: [&str; 2] = [HEALTH_PATH, READY_PATH];

/// Whether a dependency answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DependencyState {
    /// It answered.
    Up,
    /// It did not, and `detail` says what it said instead.
    Down,
}

/// Whether this instance can take work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessState {
    /// Every configured dependency answered.
    Ready,
    /// At least one did not.
    NotReady,
}

/// The body of a liveness probe.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HealthResponse {
    /// Always `alive`. The status code is the signal; this makes a curl
    /// readable.
    pub status: String,
}

impl HealthResponse {
    /// The only value this response ever takes.
    #[must_use]
    pub fn alive() -> Self {
        Self {
            status: "alive".to_string(),
        }
    }
}

/// One dependency's answer.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DependencyStatus {
    /// What was probed: `redis`, `mongodb` or `clickhouse`.
    pub name: String,
    /// Whether it answered.
    pub status: DependencyState,
    /// Why it did not, absent when it did.
    ///
    /// A FIXED category — `unreachable` or `timed_out` — never a driver's own
    /// words. This body is unauthenticated, and a server message can carry
    /// internal host names, file paths, query text and tokens that no redaction
    /// routine reliably recognises. The full explanation stays in the service's
    /// log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl DependencyStatus {
    /// Whether this dependency answered.
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.status == DependencyState::Up
    }
}

impl From<&DependencyReport> for DependencyStatus {
    fn from(report: &DependencyReport) -> Self {
        Self {
            name: report.name.to_string(),
            status: if report.is_up() {
                DependencyState::Up
            } else {
                DependencyState::Down
            },
            reason: report.failure.map(|failure| failure.as_str().to_string()),
        }
    }
}

/// The body of a readiness probe.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReadinessResponse {
    /// Whether this instance can take work.
    pub status: ReadinessState,
    /// Every dependency probed, in a fixed order, whether it answered or not.
    /// Reported in full even when one is down, so one probe says which of them
    /// are healthy rather than only naming the first failure.
    pub dependencies: Vec<DependencyStatus>,
}

impl ReadinessResponse {
    /// Renders what the probes reported.
    ///
    /// The status is DERIVED here and nowhere else: a body whose `status` and
    /// `dependencies` could disagree would be two answers to one question.
    #[must_use]
    pub fn new(reports: &[DependencyReport]) -> Self {
        let dependencies: Vec<DependencyStatus> =
            reports.iter().map(DependencyStatus::from).collect();
        let ready = dependencies.iter().all(DependencyStatus::is_up);

        Self {
            status: if ready {
                ReadinessState::Ready
            } else {
                ReadinessState::NotReady
            },
            dependencies,
        }
    }

    /// Whether every dependency answered.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status == ReadinessState::Ready
    }
}

/// Liveness probe.
#[utoipa::path(
    get,
    path = "/health",
    tag = "Operations",
    responses(
        (status = 200, description = "The process is alive", body = HealthResponse)
    )
)]
pub(crate) async fn health() -> impl Responder {
    HttpResponse::Ok().json(HealthResponse::alive())
}

/// Readiness probe.
#[utoipa::path(
    get,
    path = "/ready",
    tag = "Operations",
    responses(
        (status = 200, description = "Every dependency answered", body = ReadinessResponse),
        (
            status = 503,
            description = "At least one dependency did not answer; the body names it",
            body = ReadinessResponse
        )
    )
)]
pub(crate) async fn ready(readiness: web::Data<Readiness>) -> impl Responder {
    let body = ReadinessResponse::new(&readiness.evaluate().await);

    if body.is_ready() {
        HttpResponse::Ok().json(body)
    } else {
        HttpResponse::ServiceUnavailable().json(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::DependencyProbe;
    use actix_web::{App, http::StatusCode, test, web};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A probe whose answer the test controls, and can change mid-flight.
    struct Switch {
        name: &'static str,
        up: AtomicBool,
    }

    impl Switch {
        fn new(name: &'static str, up: bool) -> Self {
            Self {
                name,
                up: AtomicBool::new(up),
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
                Err("connection refused".to_string())
            }
        }
    }

    /// Liveness answers 200 while a dependency is down.
    ///
    /// The whole distinction between the two endpoints: a dependency outage
    /// must not get a healthy process restarted.
    #[actix_web::test]
    async fn test_health_answers_while_a_dependency_is_down() {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Readiness::new(vec![Arc::new(Switch::new(
                    "redis", false,
                ))])))
                .route(HEALTH_PATH, web::get().to(health))
                .route(READY_PATH, web::get().to(ready)),
        )
        .await;

        let alive =
            test::call_service(&app, test::TestRequest::get().uri(HEALTH_PATH).to_request()).await;
        assert_eq!(alive.status(), StatusCode::OK);

        let not_ready =
            test::call_service(&app, test::TestRequest::get().uri(READY_PATH).to_request()).await;
        assert_eq!(
            not_ready.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "the same instance must be alive and not ready at once"
        );
    }

    /// The liveness body says what it is.
    #[actix_web::test]
    async fn test_health_reports_alive() {
        let app = test::init_service(App::new().route(HEALTH_PATH, web::get().to(health))).await;

        let body: HealthResponse = test::call_and_read_body_json(
            &app,
            test::TestRequest::get().uri(HEALTH_PATH).to_request(),
        )
        .await;

        assert_eq!(body.status, "alive");
    }

    /// With every dependency answering, readiness is 200 and names them all.
    #[actix_web::test]
    async fn test_ready_reports_every_dependency_when_all_answer() {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Readiness::new(vec![
                    Arc::new(Switch::new("redis", true)),
                    Arc::new(Switch::new("mongodb", true)),
                    Arc::new(Switch::new("clickhouse", true)),
                ])))
                .route(READY_PATH, web::get().to(ready)),
        )
        .await;

        let response =
            test::call_service(&app, test::TestRequest::get().uri(READY_PATH).to_request()).await;
        assert_eq!(response.status(), StatusCode::OK);

        let body: ReadinessResponse = test::read_body_json(response).await;
        assert_eq!(body.status, ReadinessState::Ready);
        assert_eq!(
            body.dependencies
                .iter()
                .map(|dependency| dependency.name.as_str())
                .collect::<Vec<_>>(),
            vec!["redis", "mongodb", "clickhouse"],
            "the order is the one the probes were registered in"
        );
        assert!(body.dependencies.iter().all(|d| d.reason.is_none()));
    }

    /// A failing dependency is named, and the healthy ones are still reported.
    ///
    /// Naming it is the point of the body: "not ready" alone sends an operator
    /// to check three services.
    #[actix_web::test]
    async fn test_ready_names_the_failing_dependency() {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Readiness::new(vec![
                    Arc::new(Switch::new("redis", false)),
                    Arc::new(Switch::new("mongodb", true)),
                ])))
                .route(READY_PATH, web::get().to(ready)),
        )
        .await;

        let response =
            test::call_service(&app, test::TestRequest::get().uri(READY_PATH).to_request()).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body: ReadinessResponse = test::read_body_json(response).await;
        assert_eq!(body.status, ReadinessState::NotReady);
        match body
            .dependencies
            .iter()
            .find(|dependency| dependency.name == "redis")
        {
            Some(redis) => {
                assert_eq!(redis.status, DependencyState::Down);
                // The category, not the probe's own words: the body is
                // unauthenticated.
                assert_eq!(redis.reason.as_deref(), Some("unreachable"));
            }
            None => panic!("the failing dependency must be in the body: {body:?}"),
        }
        match body
            .dependencies
            .iter()
            .find(|dependency| dependency.name == "mongodb")
        {
            Some(mongodb) => assert!(mongodb.is_up(), "a healthy dependency is still reported"),
            None => panic!("every dependency must be in the body: {body:?}"),
        }
    }

    /// A dependency that comes back flips the status code, with no restart.
    #[actix_web::test]
    async fn test_ready_recovers_without_a_restart() {
        let switch = Arc::new(Switch::new("redis", false));
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Readiness::new(vec![switch.clone()])))
                .route(READY_PATH, web::get().to(ready)),
        )
        .await;

        let down =
            test::call_service(&app, test::TestRequest::get().uri(READY_PATH).to_request()).await;
        assert_eq!(down.status(), StatusCode::SERVICE_UNAVAILABLE);

        switch.up.store(true, Ordering::SeqCst);

        let up =
            test::call_service(&app, test::TestRequest::get().uri(READY_PATH).to_request()).await;
        assert_eq!(
            up.status(),
            StatusCode::OK,
            "the same process must report ready once the dependency answers"
        );
    }

    /// With nothing configured to check, an instance is ready.
    #[actix_web::test]
    async fn test_no_probes_is_ready() {
        let body = ReadinessResponse::new(&[]);

        assert!(body.is_ready());
        assert_eq!(body.status, ReadinessState::Ready);
        assert!(body.dependencies.is_empty());
    }

    /// The excluded paths are the routes, not two strings that can drift.
    ///
    /// `async` only because `use actix_web::test` shadows the built-in
    /// attribute in this module.
    #[actix_web::test]
    async fn test_the_excluded_paths_are_the_probe_routes() {
        assert_eq!(PROBE_PATHS, [HEALTH_PATH, READY_PATH]);
        assert_eq!(HEALTH_PATH, "/health");
        assert_eq!(READY_PATH, "/ready");
    }
}
