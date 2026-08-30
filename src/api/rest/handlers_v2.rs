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
use crate::api::rest::greeks::{GreekLevel, render_body};
use crate::api::rest::requests_v2::CreateSimulationRequest;
use crate::api::rest::responses_v2::{SimulationResponse, SnapshotResponse, snapshot_response};
use crate::domain::series::SeriesSnapshot;
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
#[serde(deny_unknown_fields)]
pub(crate) struct AdvanceQuery {
    /// Optional expected cursor. When supplied, the advance proceeds only if
    /// the simulation is at exactly this step; otherwise `412` is returned with
    /// the actual cursor and nothing is consumed.
    #[serde(default)]
    pub(crate) expected_step: Option<usize>,
    /// How much of the greek set the snapshot should carry: `none` (the
    /// default), `first` or `all`. Kept as a raw string so an unknown value is
    /// a typed `400` naming the field rather than actix's untyped query
    /// rejection.
    #[serde(default)]
    pub(crate) greeks: Option<String>,
}

/// The query of a snapshot peek.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotQuery {
    /// How much of the greek set to carry. See [`AdvanceQuery::greeks`].
    #[serde(default)]
    pub(crate) greeks: Option<String>,
}

/// Renders a snapshot body, under the shared greek admission bound.
///
/// See [`crate::api::rest::greeks::admit_render`]: above the default level
/// the pricing AND the serialisation happen together in one admitted blocking
/// job, and the handler writes the bytes it returns.
///
/// How much of that job is pricing depends on the deployment. With a warehouse
/// registered the snapshot already carries its greeks (issue #74) and the job
/// is mostly encoding; without one, upstream's `calculate_greeks` runs per
/// strike per style at roughly 40 µs a contract, and
/// `DEFAULT_MAX_SNAPSHOT_CONTRACTS` is 200 000. The handler cannot tell the two
/// apart from here, so it admits and offloads whenever a level is asked for.
///
/// The simulation and the snapshot are moved rather than borrowed: both are
/// already owned clones handed back by the manager, and moving them keeps the
/// job `'static` without a second copy of a snapshot that may hold hundreds of
/// thousands of contracts.
async fn render_snapshot(
    simulation: SessionV2,
    snapshot: SeriesSnapshot,
    level: GreekLevel,
) -> Result<Vec<u8>, ChainError> {
    render_body(level, move || {
        snapshot_response(&simulation, &snapshot, level)
    })
    .await
}

/// Writes an already-serialised JSON body.
#[must_use]
fn json_body(bytes: Vec<u8>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(bytes)
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
        configuration is immutable: changing any of it means creating a new simulation. \
        `strike_ladder` decides which strikes the simulation quotes and is the one choice \
        worth making deliberately: `rolling`, the default, rebuilds the ladder around the \
        underlying at every step, so the quoted strikes stay near the money and a contract \
        can leave the chain as the spot moves; `pinned` fixes the ladder at creation from \
        `initial_price`, `chain_size` and `strike_interval`, so a contract quoted once is \
        quoted for the simulation's whole life, which is what a client holding a position \
        across steps needs. A pinned simulation must supply `strike_interval`, because \
        without one the interval is derived per expiration and there is no fixed grid to \
        pin, and a pinned ladder does not follow a large move: if the spot leaves its range \
        every quoted strike ends up on one side of the money rather than the simulation \
        inventing new ones, and a spot that drifts further than the service will widen for \
        is a 400 naming strike_ladder.",
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
    params(
        ("id" = String, Path, description = "The simulation's identifier"),
        ("greeks" = Option<GreekLevel>, Query, description = "How much of the greek set to carry: `none` (default), `first` (adds theta, vega, rho, rho_d) or `all` (the full twelve-value snapshot per style). Every value is per ONE LONG CONTRACT: the client applies position sign and size. The one exception is `alpha`, the ratio gamma/theta, which a short position leaves unchanged and which must NOT be scaled or sign-flipped. An unknown value is a 400")
    ),
    responses(
        (status = 200, description = "The snapshot at the current cursor", body = SnapshotResponse),
        (status = 400, description = "Malformed id, an unknown `greeks` level, or a simulation in a terminal error state. The malformed-id and unknown-level bodies are the typed `{error, field}` of ValidationErrorResponse, with `field` = `id` or `greeks`; a terminal state carries `error` alone."),
        (status = 404, description = "Simulation not found"),
        (status = 410, description = "Simulation completed; there is no current step"),
        (status = 500, description = "Internal server error")
    )
)]
pub(crate) async fn peek_snapshot(
    req: HttpRequest,
    manager: web::Data<Arc<SimulationManager>>,
    path: web::Path<SimulationPath>,
    query: web::Query<SnapshotQuery>,
) -> impl Responder {
    info!("{} {}", req.method(), req.path());

    let id = match parse_id(&path.id) {
        Ok(id) => id,
        Err(error) => return map_error(error),
    };
    // Resolved before the snapshot is built: an unknown level costs nothing.
    let level = match GreekLevel::parse(query.greeks.as_deref()) {
        Ok(level) => level,
        Err(error) => return map_error(error),
    };

    match manager.peek(id).await {
        Ok((simulation, snapshot)) => match render_snapshot(simulation, snapshot, level).await {
            Ok(bytes) => json_body(bytes),
            Err(error) => map_error(error),
        },
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
        ("expected_step" = Option<usize>, Query, description = "Expected current cursor; a mismatch returns 412 without advancing"),
        ("greeks" = Option<GreekLevel>, Query, description = "How much of the greek set to carry: `none` (default), `first` (adds theta, vega, rho, rho_d) or `all` (the full twelve-value snapshot per style). Every value is per ONE LONG CONTRACT: the client applies position sign and size. The one exception is `alpha`, the ratio gamma/theta, which a short position leaves unchanged and which must NOT be scaled or sign-flipped. An unknown value is a 400")
    ),
    responses(
        (status = 200, description = "Served the snapshot and advanced once", body = SnapshotResponse),
        (status = 400, description = "Malformed id, an unknown `greeks` level, or a simulation in a terminal error state. The malformed-id and unknown-level bodies are the typed `{error, field}` of ValidationErrorResponse, with `field` = `id` or `greeks`; a terminal state carries `error` alone."),
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
    // Rejected before the step is consumed: an unknown level must not advance
    // the cursor on its way to a 400.
    let level = match GreekLevel::parse(query.greeks.as_deref()) {
        Ok(level) => level,
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
        Ok((simulation, snapshot)) => match render_snapshot(simulation, snapshot, level).await {
            Ok(bytes) => json_body(bytes),
            Err(error) => map_error(error),
        },
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

/// Renders a rejected QUERY string in the documented `{error, field}` shape.
///
/// Without it actix answers a query that fails to deserialize with an untyped
/// plaintext `400`, which is the one outcome the greek level was built not to
/// have: `?greek=all` is a misspelling, and a client that gets the default
/// payload back with a `200` prices a position against greeks it never
/// received. The query DTOs carry `deny_unknown_fields` so the misspelling is
/// an error at all, and this turns that error into something a client can act
/// on — the same treatment [`json_error_handler`] gives a rejected body.
pub(crate) fn query_error_handler(
    error: actix_web::error::QueryPayloadError,
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
/// which are formatted as ``Validation Error: `field`: `reason` ``). Anything else
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
            let manager = Arc::new(crate::session::SimulationManager::new(
                Arc::new(InMemorySimulationStore::new()),
                crate::infrastructure::SimulationV2Config::default(),
            ));
            actix_test::init_service(
                // No warehouse: these tests exercise the lifecycle, which is
                // identical with and without snapshot persistence.
                App::new().configure(|cfg| configure_v2_routes(cfg, manager.clone(), None)),
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

    /// Every quote in a snapshot body, call and put alike.
    fn quotes(body: &Value) -> Vec<&Value> {
        let chains = match body.get("chains").and_then(Value::as_array) {
            Some(chains) => chains,
            None => panic!("the snapshot must carry chains: {body}"),
        };
        let mut quotes = Vec::new();
        for chain in chains {
            let contracts = match chain.get("contracts").and_then(Value::as_array) {
                Some(contracts) => contracts,
                None => panic!("every chain must carry contracts: {chain}"),
            };
            for contract in contracts {
                for side in ["call", "put"] {
                    match contract.get(side) {
                        Some(quote) => quotes.push(quote),
                        None => panic!("every contract must carry a {side}: {contract}"),
                    }
                }
            }
        }
        assert!(!quotes.is_empty(), "the snapshot must quote something");
        quotes
    }

    /// Fetches the current snapshot at a greek level, or with no parameter at
    /// all when `query` is empty.
    macro_rules! snapshot {
        ($app:expr, $id:expr, $query:expr) => {{
            let uri = format!("/api/v2/simulations/{}/snapshot{}", $id, $query);
            let request = actix_test::TestRequest::get().uri(&uri).to_request();
            let response = actix_test::call_service(&$app, request).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = actix_test::read_body_json(response).await;
            body
        }};
    }

    /// The regression that protects every existing client: with no `greeks`
    /// parameter the snapshot is byte-identical to `greeks=none`, and neither
    /// carries the key at all. A client that ignores unknown fields is
    /// unaffected either way, but one that compares payloads is not, and the
    /// tape is meant to be comparable.
    #[actix_web::test]
    async fn test_the_default_snapshot_is_identical_to_greeks_none() {
        let app = v2_service!();
        let id = id_of(&create!(app));

        let default = snapshot!(app, id, "");
        let explicit = snapshot!(app, id, "?greeks=none");

        assert_eq!(default, explicit);
        for quote in quotes(&default) {
            assert!(
                quote.get("greeks").is_none(),
                "the default quote must carry no greeks key: {quote}"
            );
        }
    }

    /// `greeks=all` carries all twelve values for both styles on every strike.
    #[actix_web::test]
    async fn test_greeks_all_carries_the_twelve_values_per_style() {
        const EXPECTED: [&str; 12] = [
            "delta", "gamma", "theta", "vega", "rho", "rho_d", "alpha", "vanna", "vomma", "veta",
            "charm", "color",
        ];

        let app = v2_service!();
        let id = id_of(&create!(app));
        let body = snapshot!(app, id, "?greeks=all");

        for quote in quotes(&body) {
            let greeks = match quote.get("greeks").and_then(Value::as_object) {
                Some(greeks) => greeks,
                None => panic!("greeks=all must carry a greeks object: {quote}"),
            };
            for key in EXPECTED {
                assert!(
                    greeks.contains_key(key),
                    "greeks=all must carry {key}: {quote}"
                );
            }
            assert_eq!(
                greeks.len(),
                EXPECTED.len(),
                "greeks=all must carry exactly the twelve values: {quote}"
            );
        }
    }

    /// `greeks=first` carries the four first-order values the default response
    /// does not already have, and nothing beyond them. `delta` stays where it
    /// is, on the quote itself, so the two cannot drift.
    #[actix_web::test]
    async fn test_greeks_first_carries_only_the_remaining_first_order_set() {
        let app = v2_service!();
        let id = id_of(&create!(app));
        let body = snapshot!(app, id, "?greeks=first");

        for quote in quotes(&body) {
            let greeks = match quote.get("greeks").and_then(Value::as_object) {
                Some(greeks) => greeks,
                None => panic!("greeks=first must carry a greeks object: {quote}"),
            };
            let mut keys: Vec<&str> = greeks.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, vec!["rho", "rho_d", "theta", "vega"], "{quote}");
            assert!(
                quote.get("delta").is_some(),
                "delta stays on the quote itself: {quote}"
            );
        }
    }

    /// The per-style split is real, not one value copied twice: `charm`
    /// differs between the call and the put, and `rho` carries opposite signs.
    #[actix_web::test]
    async fn test_the_call_and_put_greeks_are_genuinely_different() {
        let app = v2_service!();
        let id = id_of(&create!(app));
        let body = snapshot!(app, id, "?greeks=all");

        let chains = match body.get("chains").and_then(Value::as_array) {
            Some(chains) => chains,
            None => panic!("the snapshot must carry chains: {body}"),
        };
        let contracts = match chains
            .first()
            .and_then(|chain| chain.get("contracts"))
            .and_then(Value::as_array)
        {
            Some(contracts) => contracts,
            None => panic!("the first chain must carry contracts: {body}"),
        };

        // Decimal-valued greeks arrive as strings; parsing keeps the assertion
        // about the numbers rather than about their rendering.
        let greek = |contract: &Value, side: &str, name: &str| -> f64 {
            let raw = contract
                .get(side)
                .and_then(|quote| quote.get("greeks"))
                .and_then(|greeks| greeks.get(name))
                .and_then(Value::as_f64);
            match raw {
                Some(value) => value,
                None => panic!("{side}.{name} must be present: {contract}"),
            }
        };

        for contract in contracts {
            assert_ne!(
                greek(contract, "call", "charm"),
                greek(contract, "put", "charm"),
                "charm must differ between the styles: {contract}"
            );
            let call_rho = greek(contract, "call", "rho");
            let put_rho = greek(contract, "put", "rho");
            assert!(
                call_rho * put_rho < 0.0,
                "rho must carry opposite signs, got {call_rho} and {put_rho}"
            );
        }
    }

    /// An unknown level is a typed `400` naming the field, not a silent
    /// downgrade to the default.
    #[actix_web::test]
    async fn test_an_unknown_greek_level_is_a_typed_400() {
        let app = v2_service!();
        let id = id_of(&create!(app));

        for uri in [
            format!("/api/v2/simulations/{id}/snapshot?greeks=second"),
            format!("/api/v2/simulations/{id}/snapshot?greeks=ALL"),
        ] {
            let request = actix_test::TestRequest::get().uri(&uri).to_request();
            let response = actix_test::call_service(&app, request).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "for {uri}");
            let body: Value = actix_test::read_body_json(response).await;
            assert_eq!(
                body.get("field").and_then(Value::as_str),
                Some("greeks"),
                "the 400 must name the field: {body}"
            );
        }
    }

    /// A MISSPELLED parameter is a typed 400, not a silent downgrade.
    ///
    /// `?greek=all` used to be accepted and ignored, so a client that fat
    /// fingered the key got a `200` carrying the default payload and priced a
    /// position against greeks it never received — exactly the failure the
    /// unknown-VALUE rejection exists to prevent, reached through the key
    /// instead. The query DTOs reject unknown keys, and the rejection is
    /// rendered in the documented shape rather than actix's plaintext.
    #[actix_web::test]
    async fn test_a_misspelled_query_key_is_a_typed_400() {
        let app = v2_service!();
        let id = id_of(&create!(app));

        for key in ["greek", "Greeks", "greeks_level"] {
            let uri = format!("/api/v2/simulations/{id}/snapshot?{key}=all");
            let request = actix_test::TestRequest::get().uri(&uri).to_request();
            let response = actix_test::call_service(&app, request).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "?{key}=all must be rejected, not ignored"
            );
            let body: Value = actix_test::read_body_json(response).await;
            assert_eq!(
                body.get("field").and_then(Value::as_str),
                Some(key),
                "the 400 must name the offending key: {body}"
            );
        }
    }

    /// The step endpoint takes the same parameter, and rejects an unknown one
    /// WITHOUT consuming a step — a 400 that advanced the cursor would make an
    /// error unrepeatable.
    #[actix_web::test]
    async fn test_an_unknown_greek_level_on_step_does_not_advance() {
        let app = v2_service!();
        let id = id_of(&create!(app));

        let request = actix_test::TestRequest::post()
            .uri(&format!("/api/v2/simulations/{id}/step?greeks=second"))
            .to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let request = actix_test::TestRequest::get()
            .uri(&format!("/api/v2/simulations/{id}"))
            .to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = actix_test::read_body_json(response).await;
        assert_eq!(
            body.get("cursor")
                .and_then(|cursor| cursor.get("current_step"))
                .and_then(Value::as_u64),
            Some(0),
            "a rejected level must not consume a step: {body}"
        );
    }

    /// The step endpoint serves the greeks too, so a stepping client does not
    /// have to peek separately to get them.
    #[actix_web::test]
    async fn test_the_step_endpoint_serves_the_requested_greeks() {
        let app = v2_service!();
        let id = id_of(&create!(app));

        let request = actix_test::TestRequest::post()
            .uri(&format!("/api/v2/simulations/{id}/step?greeks=all"))
            .to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = actix_test::read_body_json(response).await;

        for quote in quotes(&body) {
            assert!(
                quote
                    .get("greeks")
                    .and_then(|greeks| greeks.get("vomma"))
                    .is_some(),
                "a stepped snapshot must carry the full set: {quote}"
            );
        }
    }

    /// Asking for greeks changes nothing about the market itself: the prices,
    /// the delta and the gamma a snapshot serves are identical at all three
    /// levels. The greeks are read off the same option, never fed back into it.
    #[actix_web::test]
    async fn test_the_greek_level_does_not_move_the_quoted_market() {
        let app = v2_service!();
        let id = id_of(&create!(app));

        let strip = |body: &Value| -> Value {
            let mut stripped = body.clone();
            if let Some(chains) = stripped.get_mut("chains").and_then(Value::as_array_mut) {
                for chain in chains {
                    if let Some(contracts) =
                        chain.get_mut("contracts").and_then(Value::as_array_mut)
                    {
                        for contract in contracts {
                            for side in ["call", "put"] {
                                if let Some(quote) =
                                    contract.get_mut(side).and_then(Value::as_object_mut)
                                {
                                    quote.remove("greeks");
                                }
                            }
                        }
                    }
                }
            }
            stripped
        };

        let none = snapshot!(app, id, "?greeks=none");
        let first = snapshot!(app, id, "?greeks=first");
        let all = snapshot!(app, id, "?greeks=all");

        assert_eq!(strip(&first), none);
        assert_eq!(strip(&all), none);
    }

    /// A warehouse that accepts everything and returns nothing.
    ///
    /// Registering one is what makes `SeriesBuilder` build the greek snapshots
    /// (issue #74), so it selects the OTHER branch of `greeks_for` — the one
    /// that reads a snapshot instead of pricing it. Every other test here runs
    /// without a warehouse and therefore only ever exercises the repricing
    /// branch.
    #[derive(Default)]
    struct AcceptingWarehouse;

    #[async_trait::async_trait]
    impl crate::infrastructure::SimulationSnapshotRepository for AcceptingWarehouse {
        /// No server behind it; reachable exactly as long as the process is.
        async fn ping(&self) -> Result<(), ChainError> {
            Ok(())
        }

        async fn persist(
            &self,
            _record: crate::infrastructure::SnapshotRecord,
        ) -> Result<(), ChainError> {
            Ok(())
        }

        async fn get(
            &self,
            _simulation: Uuid,
            _generation: u64,
            _step: usize,
        ) -> Result<Option<crate::infrastructure::SnapshotRecord>, ChainError> {
            Ok(None)
        }

        async fn read_range(
            &self,
            _simulation: Uuid,
            _generation: u64,
            _from_step: usize,
            _to_step: usize,
        ) -> Result<Vec<crate::infrastructure::SnapshotRecord>, ChainError> {
            Ok(Vec::new())
        }

        async fn contract_series(
            &self,
            _query: crate::infrastructure::ContractSeriesQuery,
        ) -> Result<Vec<crate::infrastructure::ContractQuote>, ChainError> {
            Ok(Vec::new())
        }
    }

    /// The greeks a client receives do not depend on how they were produced.
    ///
    /// With a warehouse registered the chain already carries its snapshots and
    /// the response reads them; without one the API prices them per request.
    /// Two code paths, one seed, and the bytes must be identical — otherwise
    /// registering persistence would quietly change what clients are served,
    /// which is the regression the whole split exists not to cause.
    #[actix_web::test]
    async fn test_the_greeks_are_the_same_whether_read_or_priced() {
        let read_service = {
            let manager = Arc::new(
                crate::session::SimulationManager::new(
                    Arc::new(InMemorySimulationStore::new()),
                    crate::infrastructure::SimulationV2Config::default(),
                )
                .with_warehouse(Arc::new(AcceptingWarehouse)
                    as Arc<dyn crate::infrastructure::SimulationSnapshotRepository>),
            );
            actix_test::init_service(
                App::new().configure(|cfg| configure_v2_routes(cfg, manager.clone(), None)),
            )
            .await
        };
        let priced_service = v2_service!();

        let read_id = id_of(&create!(read_service));
        let priced_id = id_of(&create!(priced_service));

        let mut read = snapshot!(read_service, read_id, "?greeks=all");
        let mut priced = snapshot!(priced_service, priced_id, "?greeks=all");
        // The id is the one thing that legitimately differs.
        for body in [&mut read, &mut priced] {
            if let Some(object) = body.as_object_mut() {
                object.remove("id");
            }
        }

        assert_eq!(read, priced);
        // And the comparison is not vacuous.
        assert!(
            quotes(&read)
                .iter()
                .all(|quote| quote.get("greeks").is_some()),
            "both bodies must actually carry greeks"
        );
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

    /// The spread model is accepted over HTTP and echoed back verbatim.
    ///
    /// Both halves matter: `CreateSimulationRequest` denies unknown fields, so
    /// this is what proves the new coefficients are actually part of the wire
    /// contract, and the echo is what lets a client replay a run.
    #[actix_web::test]
    async fn test_the_spread_model_is_accepted_and_echoed() {
        let app = v2_service!();

        let mut request_body = reference_body();
        match request_body.as_object_mut() {
            Some(map) => {
                map.insert("spread_proportional".to_string(), json!(0.02));
                map.insert("spread_moneyness_widening".to_string(), json!(0.5));
                map.insert("spread_tenor_widening".to_string(), json!(0.1));
                map.insert("spread_tick".to_string(), json!(0.05));
            }
            None => panic!("the reference body must be an object"),
        }

        let request = actix_test::TestRequest::post()
            .uri("/api/v2/simulations")
            .set_json(request_body)
            .to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::CREATED);

        let body: Value = actix_test::read_body_json(response).await;
        let parameters = match body.get("parameters") {
            Some(parameters) => parameters,
            None => panic!("the response must echo its parameters: {body}"),
        };
        for (field, expected) in [
            ("spread", json!(0.02)),
            ("spread_proportional", json!(0.02)),
            ("spread_moneyness_widening", json!(0.5)),
            ("spread_tenor_widening", json!(0.1)),
            ("spread_tick", json!(0.05)),
        ] {
            assert_eq!(
                parameters.get(field),
                Some(&expected),
                "the echo must carry {field}: {parameters}"
            );
        }
    }

    /// The strike ladder is accepted over HTTP and echoed back.
    ///
    /// `CreateSimulationRequest` denies unknown fields, so this is what proves
    /// `strike_ladder` is part of the wire contract rather than only of the
    /// stored shape, and the echo is what lets a client replay the run.
    #[actix_web::test]
    async fn test_the_strike_ladder_is_accepted_and_echoed() {
        let app = v2_service!();

        let mut request_body = reference_body();
        match request_body.as_object_mut() {
            Some(map) => {
                map.insert("strike_ladder".to_string(), json!("pinned"));
                // A pinned ladder needs an explicit grid, and one narrow enough
                // that upstream can build every strike of it.
                map.insert("strike_interval".to_string(), json!(25.0));
                map.insert("chain_size".to_string(), json!(3));
            }
            None => panic!("the reference body must be an object"),
        }

        let request = actix_test::TestRequest::post()
            .uri("/api/v2/simulations")
            .set_json(request_body)
            .to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::CREATED);

        let body: Value = actix_test::read_body_json(response).await;
        assert_eq!(
            body.get("parameters").and_then(|p| p.get("strike_ladder")),
            Some(&json!("pinned")),
            "the echo must carry the ladder: {body}"
        );
    }

    /// A request that says nothing gets the ladder the service always had.
    #[actix_web::test]
    async fn test_the_strike_ladder_defaults_to_rolling() {
        let app = v2_service!();
        let body = create!(app);

        assert_eq!(
            body.get("parameters").and_then(|p| p.get("strike_ladder")),
            Some(&json!("rolling")),
            "an untouched request must read as rolling: {body}"
        );
    }

    /// A pinned ladder without a grid is a typed 400 naming the field.
    #[actix_web::test]
    async fn test_a_pinned_ladder_without_an_interval_is_a_typed_400() {
        let app = v2_service!();

        let mut request_body = reference_body();
        match request_body.as_object_mut() {
            Some(map) => {
                map.insert("strike_ladder".to_string(), json!("pinned"));
                map.remove("strike_interval");
            }
            None => panic!("the reference body must be an object"),
        }

        let request = actix_test::TestRequest::post()
            .uri("/api/v2/simulations")
            .set_json(request_body)
            .to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body: Value = actix_test::read_body_json(response).await;
        assert_eq!(
            body.get("field"),
            Some(&json!("strike_interval")),
            "the rejection must name the field: {body}"
        );
    }

    /// A pinned ladder wider than the spot is refused at creation.
    ///
    /// Upstream stops building a chain once the offset passes the anchor, so
    /// those strikes would never exist; refusing here beats failing on the
    /// first step of a simulation the client already holds.
    #[actix_web::test]
    async fn test_a_pinned_ladder_wider_than_the_spot_is_refused() {
        let app = v2_service!();

        let mut request_body = reference_body();
        match request_body.as_object_mut() {
            Some(map) => {
                map.insert("strike_ladder".to_string(), json!("pinned"));
                map.insert("initial_price".to_string(), json!(100.0));
                map.insert("strike_interval".to_string(), json!(5.0));
                map.insert("chain_size".to_string(), json!(25));
            }
            None => panic!("the reference body must be an object"),
        }

        let request = actix_test::TestRequest::post()
            .uri("/api/v2/simulations")
            .set_json(request_body)
            .to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body: Value = actix_test::read_body_json(response).await;
        assert_eq!(
            body.get("field"),
            Some(&json!("chain_size")),
            "the rejection must name what to lower: {body}"
        );
    }

    /// A spread coefficient outside its range is a typed 400 naming the field.
    #[actix_web::test]
    async fn test_an_out_of_range_spread_coefficient_is_a_typed_400() {
        let app = v2_service!();

        let mut request_body = reference_body();
        match request_body.as_object_mut() {
            Some(map) => map.insert("spread_proportional".to_string(), json!(-0.01)),
            None => panic!("the reference body must be an object"),
        };

        let request = actix_test::TestRequest::post()
            .uri("/api/v2/simulations")
            .set_json(request_body)
            .to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body: Value = actix_test::read_body_json(response).await;
        assert_eq!(
            body.get("field"),
            Some(&json!("spread_proportional")),
            "the rejection must name the field: {body}"
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

    /// Reproducibility holds at the new level too: two simulations built from
    /// the same body — hence the same seed — serve identical `greeks=all`
    /// snapshots, greek for greek.
    ///
    /// The domain tape test compares two walks and never sees a greek level,
    /// so it cannot cover this. IronCondor gates on the served bytes, and the
    /// greeks are now part of them.
    #[actix_web::test]
    async fn test_the_same_seed_serves_the_same_greeks() {
        let app = v2_service!();
        let first = id_of(&create!(app));
        let second = id_of(&create!(app));
        assert_ne!(first, second, "two creations must be distinct simulations");

        let mut left = snapshot!(app, first, "?greeks=all");
        let mut right = snapshot!(app, second, "?greeks=all");
        // The id is the one thing that legitimately differs.
        for body in [&mut left, &mut right] {
            if let Some(object) = body.as_object_mut() {
                object.remove("id");
            }
        }

        assert_eq!(left, right);
    }

    /// The style-independent greeks really are shared, and the style-dependent
    /// ones really are not.
    ///
    /// The crate docs make this claim to clients deciding what to store per
    /// side; without a test it is just a sentence. `gamma`, `vega`, `vanna`,
    /// `vomma`, `veta` and `color` do not depend on the option style, so a
    /// call and a put on the same strike must agree on them exactly.
    #[actix_web::test]
    async fn test_the_style_independent_greeks_agree_across_the_two_sides() {
        const SHARED: [&str; 6] = ["gamma", "vega", "vanna", "vomma", "veta", "color"];
        const PER_STYLE: [&str; 4] = ["delta", "theta", "rho", "rho_d"];

        let app = v2_service!();
        let id = id_of(&create!(app));
        let body = snapshot!(app, id, "?greeks=all");

        let chains = match body.get("chains").and_then(Value::as_array) {
            Some(chains) => chains,
            None => panic!("the snapshot must carry chains: {body}"),
        };
        for chain in chains {
            let contracts = match chain.get("contracts").and_then(Value::as_array) {
                Some(contracts) => contracts,
                None => panic!("every chain must carry contracts: {chain}"),
            };
            for contract in contracts {
                let side = |name: &str, greek: &str| -> Value {
                    contract
                        .get(name)
                        .and_then(|quote| quote.get("greeks"))
                        .and_then(|greeks| greeks.get(greek))
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                for greek in SHARED {
                    assert_eq!(
                        side("call", greek),
                        side("put", greek),
                        "{greek} does not depend on the style: {contract}"
                    );
                }
                for greek in PER_STYLE {
                    assert_ne!(
                        side("call", greek),
                        side("put", greek),
                        "{greek} depends on the style: {contract}"
                    );
                }
            }
        }
    }

    /// The snapshot's `delta` and `gamma` agree with the `f64` mirrors the
    /// quote and the contract already carried.
    ///
    /// At `greeks=all` a response carries each of those numbers twice, from
    /// two independent upstream computations. If they ever disagreed a client
    /// would have to know which one to believe, so the agreement is pinned
    /// rather than assumed.
    #[actix_web::test]
    async fn test_the_snapshot_agrees_with_the_mirrors_it_duplicates() {
        // The mirrors are rendered as `f64`, the snapshot at full decimal
        // precision, so they agree to within the `f64` round-trip and no more.
        const TOLERANCE: f64 = 1e-12;

        let app = v2_service!();
        let id = id_of(&create!(app));
        let body = snapshot!(app, id, "?greeks=all");

        let parse = |value: Option<&Value>, what: &str| -> f64 {
            match value.and_then(Value::as_f64) {
                Some(number) => number,
                None => panic!("{what} must be present"),
            }
        };

        let chains = match body.get("chains").and_then(Value::as_array) {
            Some(chains) => chains,
            None => panic!("the snapshot must carry chains: {body}"),
        };
        for chain in chains {
            let contracts = match chain.get("contracts").and_then(Value::as_array) {
                Some(contracts) => contracts,
                None => panic!("every chain must carry contracts: {chain}"),
            };
            for contract in contracts {
                let mirror_gamma = match contract.get("gamma").and_then(Value::as_f64) {
                    Some(gamma) => gamma,
                    None => continue,
                };
                for side in ["call", "put"] {
                    let quote = match contract.get(side) {
                        Some(quote) => quote,
                        None => panic!("every contract must carry a {side}: {contract}"),
                    };
                    let greeks = match quote.get("greeks") {
                        Some(greeks) => greeks,
                        None => panic!("greeks=all must carry a greeks object: {quote}"),
                    };
                    let snapshot_gamma = parse(greeks.get("gamma"), "the snapshot gamma");
                    assert!(
                        (snapshot_gamma - mirror_gamma).abs() < TOLERANCE,
                        "gamma disagrees with its mirror: {snapshot_gamma} vs {mirror_gamma}"
                    );

                    if let Some(mirror_delta) = quote.get("delta").and_then(Value::as_f64) {
                        let snapshot_delta = parse(greeks.get("delta"), "the snapshot delta");
                        assert!(
                            (snapshot_delta - mirror_delta).abs() < TOLERANCE,
                            "{side} delta disagrees with its mirror: \
                             {snapshot_delta} vs {mirror_delta}"
                        );
                    }
                }
            }
        }
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
