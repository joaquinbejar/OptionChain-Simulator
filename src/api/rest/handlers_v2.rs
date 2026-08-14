//! Handlers for `/api/v2/simulations`.
//!
//! Separate from [`crate::api::rest::handlers`] because `/api/v1/chain` is
//! frozen (ADR 0001 §12.1). Nothing here touches a v1 route, DTO, status code
//! or stored shape.
//!
//! # Two concurrency mechanisms, kept apart
//!
//! v1 already ships both, and v2 reuses them with v1's names and v1's status
//! codes rather than inventing a third vocabulary:
//!
//! - **`expected_step`** is a client *precondition* on the cursor. A mismatch is
//!   `412` with the actual cursor, and nothing is persisted — which is what
//!   makes a retry after a lost response safe: it cannot double-advance.
//! - **The revision** is the compare-and-swap token. Two advances that both
//!   pass the precondition still produce one winner; the loser gets `409`.
//!
//! They are not the same thing and are deliberately not collapsed: `412` means
//! "the cursor is not where you thought", `409` means "someone else committed
//! first".

use crate::api::rest::error::map_error;
use crate::api::rest::requests_v2::CreateSimulationRequest;
use crate::api::rest::responses_v2::{SimulationResponse, SnapshotResponse, snapshot_response};
use crate::session::{SessionV2, SimulationManager, SimulationParametersV2};
use crate::utils::ChainError;
use actix_web::{HttpRequest, HttpResponse, Responder, web};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;
use utoipa::ToSchema;
use uuid::Uuid;

/// Path parameter for every per-simulation route.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub(crate) struct SimulationPath {
    /// The simulation's identifier.
    pub(crate) id: String,
}

/// Query parameters for the advance command.
#[derive(Debug, Default, Serialize, Deserialize, ToSchema)]
pub(crate) struct AdvanceQuery {
    /// Optional expected cursor. When supplied, the advance proceeds only if
    /// the simulation is at exactly this step; otherwise `412` is returned with
    /// the actual cursor and nothing is consumed.
    #[serde(default)]
    pub(crate) expected_step: Option<usize>,
}

/// Parses a path id, reporting a malformed one as a validation failure naming
/// the field rather than as an opaque `400`.
fn parse_id(raw: &str) -> Result<Uuid, ChainError> {
    Uuid::parse_str(raw).map_err(|_| ChainError::Validation {
        field: "id".to_string(),
        reason: format!("must be a UUID, got {raw:?}"),
    })
}

#[utoipa::path(
    post,
    path = "/api/v2/simulations",
    description = "Create a deterministic rolling multi-expiration simulation. Resolves the \
        effective seed, simulated start and step interval once, and returns them with the \
        normalised schedules — together they are everything needed to replay the run. The \
        configuration is immutable: changing any of it means creating a new simulation.",
    request_body = CreateSimulationRequest,
    responses(
        (status = 201, description = "Simulation created", body = SimulationResponse),
        (status = 400, description = "Invalid request; body carries `error` and the offending `field`"),
        (status = 409, description = "A simulation with the generated id already exists"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn create_simulation(
    req: HttpRequest,
    manager: web::Data<Arc<SimulationManager>>,
    body: web::Json<CreateSimulationRequest>,
) -> impl Responder {
    info!("{} {}", req.method(), req.path());

    let parameters = match SimulationParametersV2::try_from(body.into_inner()) {
        Ok(parameters) => parameters,
        Err(error) => return map_error(error),
    };

    match manager.create(parameters).await {
        Ok(simulation) => HttpResponse::Created().json(SimulationResponse::from(&simulation)),
        Err(error) => map_error(error),
    }
}

#[utoipa::path(
    get,
    path = "/api/v2/simulations/{id}",
    description = "Read a simulation's metadata and effective parameters. Does not build a \
        snapshot and does not move the cursor.",
    params(("id" = String, Path, description = "The simulation's identifier")),
    responses(
        (status = 200, description = "The simulation", body = SimulationResponse),
        (status = 400, description = "Malformed id"),
        (status = 404, description = "Simulation not found"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn get_simulation(
    req: HttpRequest,
    manager: web::Data<Arc<SimulationManager>>,
    path: web::Path<SimulationPath>,
) -> impl Responder {
    info!("{} {}", req.method(), req.path());

    let id = match parse_id(&path.id) {
        Ok(id) => id,
        Err(error) => return map_error(error),
    };

    match manager.get(id).await {
        Ok(simulation) => HttpResponse::Ok().json(SimulationResponse::from(&simulation)),
        Err(error) => map_error(error),
    }
}

#[utoipa::path(
    get,
    path = "/api/v2/simulations/{id}/snapshot",
    description = "Peek the snapshot at the current cursor. Safe and repeatable: it never \
        advances the cursor and never persists anything, so calling it twice returns the \
        same market. To advance, use POST /api/v2/simulations/{id}/step.",
    params(("id" = String, Path, description = "The simulation's identifier")),
    responses(
        (status = 200, description = "The snapshot at the current cursor", body = SnapshotResponse),
        (status = 400, description = "Malformed id, or the simulation is in a terminal error state"),
        (status = 404, description = "Simulation not found"),
        (status = 410, description = "Simulation completed; there is no current step"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn peek_snapshot(
    req: HttpRequest,
    manager: web::Data<Arc<SimulationManager>>,
    path: web::Path<SimulationPath>,
) -> impl Responder {
    info!("{} {}", req.method(), req.path());

    let id = match parse_id(&path.id) {
        Ok(id) => id,
        Err(error) => return map_error(error),
    };

    match manager.peek(id).await {
        Ok((simulation, snapshot)) => {
            HttpResponse::Ok().json(snapshot_response(&simulation, &snapshot))
        }
        Err(error) => map_error(error),
    }
}

#[utoipa::path(
    post,
    path = "/api/v2/simulations/{id}/step",
    description = "Serve the snapshot at the current cursor, then advance exactly once. A \
        simulation with steps = N serves indices 0..N-1 over N calls; the advance that \
        serves the last snapshot marks it completed, and any further call returns 410. \
        Pass `expected_step` to make a retry safe: if a previous attempt already consumed \
        the step, the call returns 412 with the actual cursor instead of consuming another.",
    params(
        ("id" = String, Path, description = "The simulation's identifier"),
        ("expected_step" = Option<usize>, Query, description = "Expected current cursor; a mismatch returns 412 without advancing")
    ),
    responses(
        (status = 200, description = "Served the snapshot and advanced once", body = SnapshotResponse),
        (status = 400, description = "Malformed id, or the simulation is in a terminal error state"),
        (status = 404, description = "Simulation not found"),
        (status = 409, description = "Another request advanced the simulation first; re-read and retry"),
        (status = 410, description = "Simulation completed; no further steps"),
        (status = 412, description = "expected_step does not match the cursor; body carries `error` and `current_step`"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn advance_simulation(
    req: HttpRequest,
    manager: web::Data<Arc<SimulationManager>>,
    path: web::Path<SimulationPath>,
    query: web::Query<AdvanceQuery>,
) -> impl Responder {
    info!("{} {}", req.method(), req.path());

    let id = match parse_id(&path.id) {
        Ok(id) => id,
        Err(error) => return map_error(error),
    };

    // The precondition is a transport-level check, resolved before anything is
    // built or persisted, so a mismatch costs nothing.
    if let Some(expected) = query.expected_step {
        match manager.get(id).await {
            Ok(simulation) if simulation.current_step != expected => {
                return precondition_failed(&simulation);
            }
            Ok(_) => {}
            Err(error) => return map_error(error),
        }
    }

    match manager.advance(id).await {
        Ok((simulation, snapshot)) => {
            HttpResponse::Ok().json(snapshot_response(&simulation, &snapshot))
        }
        Err(error) => map_error(error),
    }
}

/// The `412` body: the same shape v1 uses for the same precondition.
fn precondition_failed(simulation: &SessionV2) -> HttpResponse {
    HttpResponse::PreconditionFailed().json(serde_json::json!({
        "error": "expected_step does not match the simulation's current cursor",
        "current_step": simulation.current_step,
    }))
}

#[utoipa::path(
    delete,
    path = "/api/v2/simulations/{id}",
    description = "Delete a simulation and evict everything cached for it.",
    params(("id" = String, Path, description = "The simulation's identifier")),
    responses(
        (status = 200, description = "Deleted", body = Object),
        (status = 400, description = "Malformed id"),
        (status = 404, description = "Simulation not found"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn delete_simulation(
    req: HttpRequest,
    manager: web::Data<Arc<SimulationManager>>,
    path: web::Path<SimulationPath>,
) -> impl Responder {
    info!("{} {}", req.method(), req.path());

    let id = match parse_id(&path.id) {
        Ok(id) => id,
        Err(error) => return map_error(error),
    };

    match manager.delete(id).await {
        Ok(true) => HttpResponse::Ok().json(serde_json::json!({
            "message": format!("Simulation deleted successfully: {id}"),
            "simulation_id": id.to_string(),
        })),
        Ok(false) => map_error(ChainError::NotFound(format!(
            "Simulation with id {id} not found"
        ))),
        Err(error) => map_error(error),
    }
}

/// Turns a rejected JSON body into the documented `400` shape.
///
/// This is load-bearing rather than cosmetic. A schedule rule validates *during*
/// deserialization — the rule type owns its own invariants, which is what stops
/// a stored schedule from smuggling an invalid rule past the constructor — so
/// its `ChainError::Validation` is flattened into a serde error before any
/// handler sees it. Without this, actix would render that as a plaintext `400`
/// with no `field`, and the whole rule-level class of ADR 0001 §4.4 failures
/// would silently lose the structured field the section promises.
///
/// The message serde carries already names the offending field, so it is
/// surfaced verbatim rather than guessed at.
pub(crate) fn json_error_handler(
    error: actix_web::error::JsonPayloadError,
    _req: &HttpRequest,
) -> actix_web::Error {
    let message = error.to_string();
    let response =
        HttpResponse::BadRequest().json(crate::api::rest::responses::ValidationErrorResponse {
            error: message.clone(),
            field: field_from_serde_message(&message),
        });
    actix_web::error::InternalError::from_response(error, response).into()
}

/// Recovers the offending field from a serde error message.
///
/// Best effort by design: serde's own messages name the field for the cases
/// that matter (`unknown field \`x\``, and this crate's own validation errors,
/// which are formatted as `Validation Error: <field>: <reason>`). Anything else
/// reports an empty field rather than inventing one, which is more useful to a
/// client than a confident wrong answer.
#[must_use]
fn field_from_serde_message(message: &str) -> String {
    if let Some(rest) = message.split("unknown field `").nth(1)
        && let Some(field) = rest.split('`').next()
    {
        return field.to_string();
    }
    if let Some(rest) = message.split("Validation Error: ").nth(1)
        && let Some(field) = rest.split(':').next()
    {
        return field.trim().to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::routes::configure_v2_routes;
    use crate::session::InMemorySimulationStore;
    use actix_web::App;
    use actix_web::http::StatusCode;
    use actix_web::test as actix_test;
    use serde_json::{Value, json};

    /// The reference configuration of ADR 0001 §14, as a request body.
    fn reference_body() -> Value {
        json!({
            "symbol": "SPX",
            "steps": 4,
            "start_at": "2026-01-05T14:30:00Z",
            "step_interval_seconds": 86400,
            "timezone": "America/New_York",
            "expiration_time": "17:00",
            "schedules": [
                { "rule_id": "zero_dte", "kind": "daily", "target_count": 1 },
                { "rule_id": "weeklies", "kind": "weekly", "target_count": 3,
                  "weekdays": ["Mon", "Wed", "Fri"] },
                { "rule_id": "monthlies", "kind": "monthly", "target_count": 12,
                  "weekday": "Fri" }
            ],
            "initial_price": 5000.0,
            "volatility": 0.18,
            "risk_free_rate": 0.04,
            "dividend_yield": 0.012,
            "method": { "Brownian": { "dt": 0.004, "drift": 0.0, "volatility": 0.18 } },
            "time_frame": "Day",
            "chain_size": 3,
            "strike_interval": 25.0,
            "spread": 0.02,
            "seed": 42
        })
    }

    /// Mounts the real v2 routes over an in-memory store.
    macro_rules! v2_service {
        () => {{
            let manager = Arc::new(crate::session::SimulationManager::new(Arc::new(
                InMemorySimulationStore::new(),
            )));
            actix_test::init_service(
                App::new().configure(|cfg| configure_v2_routes(cfg, manager.clone())),
            )
            .await
        }};
    }

    /// Creates a simulation and returns the parsed response body.
    macro_rules! create {
        ($app:expr) => {{
            let request = actix_test::TestRequest::post()
                .uri("/api/v2/simulations")
                .set_json(reference_body())
                .to_request();
            let response = actix_test::call_service(&$app, request).await;
            assert_eq!(response.status(), StatusCode::CREATED);
            let body: Value = actix_test::read_body_json(response).await;
            body
        }};
    }

    fn id_of(body: &Value) -> String {
        match body.get("id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => panic!("the response must carry an id: {body}"),
        }
    }

    /// Creating returns 201 and echoes every replay input.
    #[actix_web::test]
    async fn test_create_returns_the_replay_inputs() {
        let app = v2_service!();
        let body = create!(app);

        let parameters = match body.get("parameters") {
            Some(parameters) => parameters,
            None => panic!("the response must echo its parameters: {body}"),
        };
        for field in [
            "seed",
            "effective_start",
            "step_interval_seconds",
            "time_frame",
            "timezone",
            "calendar",
            "tzdb_version",
            "expiration_time",
            "schedules",
        ] {
            assert!(
                parameters.get(field).is_some(),
                "the echo must carry {field}: {parameters}"
            );
        }
        assert_eq!(parameters.get("seed"), Some(&json!(42)));
        assert_eq!(body.get("state"), Some(&json!("initialized")));
        assert_eq!(
            body.get("cursor").and_then(|c| c.get("current_step")),
            Some(&json!(0))
        );
    }

    /// The echoed schedules are normalised — ordered by rule id.
    #[actix_web::test]
    async fn test_the_echoed_schedules_are_normalised() {
        let app = v2_service!();
        let body = create!(app);

        let ids: Vec<String> = match body
            .get("parameters")
            .and_then(|p| p.get("schedules"))
            .and_then(Value::as_array)
        {
            Some(rules) => rules
                .iter()
                .filter_map(|rule| rule.get("rule_id").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect(),
            None => panic!("the echo must carry schedules: {body}"),
        };

        assert_eq!(ids, vec!["monthlies", "weeklies", "zero_dte"]);
    }

    /// A peek is repeatable and byte-stable, and does not move the cursor.
    #[actix_web::test]
    async fn test_a_peek_is_byte_stable_and_does_not_advance() {
        let app = v2_service!();
        let id = id_of(&create!(app));
        let uri = format!("/api/v2/simulations/{id}/snapshot");

        let first: Value = {
            let response = actix_test::call_service(
                &app,
                actix_test::TestRequest::get().uri(&uri).to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            actix_test::read_body_json(response).await
        };
        let second: Value = {
            let response = actix_test::call_service(
                &app,
                actix_test::TestRequest::get().uri(&uri).to_request(),
            )
            .await;
            actix_test::read_body_json(response).await
        };

        assert_eq!(first, second, "a peek must be repeatable");
        assert_eq!(
            first.get("cursor").and_then(|c| c.get("current_step")),
            Some(&json!(0)),
            "a peek must not advance"
        );
    }

    /// The snapshot carries the whole simulated market: the clock, the
    /// underlying, and the ordered chains with their labels and contracts.
    #[actix_web::test]
    async fn test_the_snapshot_carries_the_documented_shape() {
        let app = v2_service!();
        let id = id_of(&create!(app));

        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri(&format!("/api/v2/simulations/{id}/snapshot"))
                .to_request(),
        )
        .await;
        let body: Value = actix_test::read_body_json(response).await;

        assert_eq!(
            body.get("simulated_at"),
            Some(&json!("2026-01-05T14:30:00Z"))
        );
        let underlying = match body.get("underlying") {
            Some(underlying) => underlying,
            None => panic!("the snapshot must carry the underlying: {body}"),
        };
        assert_eq!(underlying.get("symbol"), Some(&json!("SPX")));
        assert!(underlying.get("price").is_some());
        assert!(underlying.get("base_volatility").is_some());

        let chains = match body.get("chains").and_then(Value::as_array) {
            Some(chains) => chains,
            None => panic!("the snapshot must carry chains: {body}"),
        };
        // 1 + 3 + 12 rule slots, with Monday's 0DTE shared with the first
        // weekly, so fifteen physical expirations.
        assert_eq!(chains.len(), 15);

        let first = match chains.first() {
            Some(first) => first,
            None => panic!("the snapshot must carry chains"),
        };
        assert_eq!(
            first.get("expires_at"),
            Some(&json!("2026-01-05T22:00:00Z"))
        );
        assert_eq!(
            first.get("labels"),
            Some(&json!(["weeklies", "zero_dte"])),
            "a coincident expiration carries every matching label"
        );
        let contracts = match first.get("contracts").and_then(Value::as_array) {
            Some(contracts) => contracts,
            None => panic!("a chain must carry contracts: {first}"),
        };
        let contract = match contracts.first() {
            Some(contract) => contract,
            None => panic!("a chain must carry contracts"),
        };
        for field in ["strike", "implied_volatility", "call", "put"] {
            assert!(
                contract.get(field).is_some(),
                "a contract must carry {field}: {contract}"
            );
        }
    }

    /// An advance serves the current snapshot and then moves the cursor once.
    #[actix_web::test]
    async fn test_an_advance_serves_then_advances() {
        let app = v2_service!();
        let id = id_of(&create!(app));

        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri(&format!("/api/v2/simulations/{id}/step"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = actix_test::read_body_json(response).await;

        assert_eq!(
            body.get("cursor").and_then(|c| c.get("current_step")),
            Some(&json!(1)),
            "the response reports the cursor after the advance"
        );
        assert_eq!(
            body.get("simulated_at"),
            Some(&json!("2026-01-05T14:30:00Z")),
            "the snapshot served is the one at the pre-advance cursor"
        );
    }

    /// A matching `expected_step` advances; a stale one is 412 with the actual
    /// cursor, and nothing is consumed.
    #[actix_web::test]
    async fn test_the_expected_step_precondition_protects_a_retry() {
        let app = v2_service!();
        let id = id_of(&create!(app));
        let step_uri =
            |expected: usize| format!("/api/v2/simulations/{id}/step?expected_step={expected}");

        let ok = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri(&step_uri(0))
                .to_request(),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);

        // The retry a client sends after a lost response: the cursor has moved,
        // so the precondition refuses rather than consuming another step.
        let stale = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri(&step_uri(0))
                .to_request(),
        )
        .await;
        assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
        let body: Value = actix_test::read_body_json(stale).await;
        assert_eq!(body.get("current_step"), Some(&json!(1)));
        assert!(body.get("error").is_some());

        // And the cursor really did not move.
        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri(&format!("/api/v2/simulations/{id}"))
                .to_request(),
        )
        .await;
        let simulation: Value = actix_test::read_body_json(response).await;
        assert_eq!(
            simulation.get("cursor").and_then(|c| c.get("current_step")),
            Some(&json!(1))
        );
    }

    /// Walking to the end completes the simulation; anything after is 410.
    #[actix_web::test]
    async fn test_an_exhausted_simulation_is_gone() {
        let app = v2_service!();
        let id = id_of(&create!(app));
        let uri = format!("/api/v2/simulations/{id}/step");

        for _ in 0..4 {
            let response = actix_test::call_service(
                &app,
                actix_test::TestRequest::post().uri(&uri).to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }

        let exhausted =
            actix_test::call_service(&app, actix_test::TestRequest::post().uri(&uri).to_request())
                .await;
        assert_eq!(exhausted.status(), StatusCode::GONE);

        let peeked = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri(&format!("/api/v2/simulations/{id}/snapshot"))
                .to_request(),
        )
        .await;
        assert_eq!(peeked.status(), StatusCode::GONE);
    }

    /// An unknown id is 404 on every per-simulation route.
    #[actix_web::test]
    async fn test_an_unknown_id_is_not_found() {
        let app = v2_service!();
        let missing = Uuid::new_v4();

        for (method, uri) in [
            ("GET", format!("/api/v2/simulations/{missing}")),
            ("GET", format!("/api/v2/simulations/{missing}/snapshot")),
            ("DELETE", format!("/api/v2/simulations/{missing}")),
        ] {
            let request = match method {
                "GET" => actix_test::TestRequest::get().uri(&uri),
                _ => actix_test::TestRequest::delete().uri(&uri),
            };
            let response = actix_test::call_service(&app, request.to_request()).await;
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{method} {uri} must be 404"
            );
        }
    }

    /// A malformed id is a 400 naming the field, not a 404 or a panic.
    #[actix_web::test]
    async fn test_a_malformed_id_is_a_bad_request() {
        let app = v2_service!();

        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri("/api/v2/simulations/not-a-uuid")
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = actix_test::read_body_json(response).await;
        assert_eq!(body.get("field"), Some(&json!("id")));
    }

    /// An invalid field is a 400 carrying the offending field name.
    #[actix_web::test]
    async fn test_an_invalid_field_is_reported_by_name() {
        let app = v2_service!();
        let mut body = reference_body();
        body["timezone"] = json!("Mars/Olympus");

        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/api/v2/simulations")
                .set_json(body)
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Value = actix_test::read_body_json(response).await;
        assert_eq!(error.get("field"), Some(&json!("timezone")));
    }

    /// An unknown request field is rejected, and the JSON error handler
    /// recovers the field name into the documented shape.
    ///
    /// This is the case that would otherwise come back as plaintext with no
    /// `field` at all.
    #[actix_web::test]
    async fn test_an_unknown_field_is_reported_in_the_documented_shape() {
        let app = v2_service!();
        let mut body = reference_body();
        body["days_to_expiration"] = json!(30.0);

        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/api/v2/simulations")
                .set_json(body)
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Value = actix_test::read_body_json(response).await;
        assert_eq!(error.get("field"), Some(&json!("days_to_expiration")));
    }

    /// A rule-level failure raised inside deserialization also arrives as
    /// `{error, field}` rather than plaintext.
    #[actix_web::test]
    async fn test_a_rule_level_failure_keeps_its_field() {
        let app = v2_service!();
        let mut body = reference_body();
        body["schedules"] = json!([
            { "rule_id": "zero_dte", "kind": "daily", "target_count": 1, "weekday": "Fri" }
        ]);

        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/api/v2/simulations")
                .set_json(body)
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Value = actix_test::read_body_json(response).await;
        let field = match error.get("field").and_then(Value::as_str) {
            Some(field) => field,
            None => panic!("the error must name a field: {error}"),
        };
        assert!(field.contains("weekday"), "got {field}");
    }

    /// Deleting reports success once and 404 afterwards.
    #[actix_web::test]
    async fn test_delete_then_get_is_not_found() {
        let app = v2_service!();
        let id = id_of(&create!(app));
        let uri = format!("/api/v2/simulations/{id}");

        let deleted = actix_test::call_service(
            &app,
            actix_test::TestRequest::delete().uri(&uri).to_request(),
        )
        .await;
        assert_eq!(deleted.status(), StatusCode::OK);
        let body: Value = actix_test::read_body_json(deleted).await;
        assert_eq!(body.get("simulation_id"), Some(&json!(id)));

        let gone =
            actix_test::call_service(&app, actix_test::TestRequest::get().uri(&uri).to_request())
                .await;
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    }

    /// Two simulations created with the same seed serve the same snapshot,
    /// over every value the wire carries.
    #[actix_web::test]
    async fn test_the_same_seed_serves_the_same_snapshot() {
        let app = v2_service!();
        let first = id_of(&create!(app));
        let second = id_of(&create!(app));
        assert_ne!(first, second, "two creations must be distinct simulations");

        let snapshot_of = async |id: &str| -> Value {
            let response = actix_test::call_service(
                &app,
                actix_test::TestRequest::get()
                    .uri(&format!("/api/v2/simulations/{id}/snapshot"))
                    .to_request(),
            )
            .await;
            let mut body: Value = actix_test::read_body_json(response).await;
            // The id is the one thing that legitimately differs.
            if let Some(object) = body.as_object_mut() {
                object.remove("id");
            }
            body
        };

        assert_eq!(snapshot_of(&first).await, snapshot_of(&second).await);
    }

    /// A malformed id is a validation failure naming the field, not an opaque
    /// bad request.
    #[test]
    fn test_a_malformed_id_names_the_field() {
        match parse_id("not-a-uuid") {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "id");
                assert!(reason.contains("UUID"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A well-formed id parses.
    #[test]
    fn test_a_well_formed_id_parses() {
        assert!(parse_id("6af613b6-569c-5c22-9c37-2ed93f31d3af").is_ok());
    }

    /// An unknown-field rejection is reported with the field serde named.
    #[test]
    fn test_an_unknown_field_is_recovered_from_the_serde_message() {
        let message = "unknown field `days_to_expiration`, expected one of `symbol`, `steps`";

        assert_eq!(field_from_serde_message(message), "days_to_expiration");
    }

    /// A rule-level validation failure is reported with the field the domain
    /// named, which is the case this handler exists for.
    #[test]
    fn test_a_rule_validation_failure_is_recovered_from_the_serde_message() {
        let message = "Validation Error: schedules.zero_dte.weekdays: does not belong to this rule kind at line 3 column 5";

        assert_eq!(
            field_from_serde_message(message),
            "schedules.zero_dte.weekdays"
        );
    }

    /// An unrecognised message reports no field rather than guessing.
    #[test]
    fn test_an_unrecognised_message_reports_no_field() {
        assert_eq!(field_from_serde_message("EOF while parsing a value"), "");
    }
}
