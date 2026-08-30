//! The rejection contract, field by field.
//!
//! A client meets an error body far more often than a happy path, and the
//! shape is a promise: `{error, field}`, with `field` naming the offender
//! wherever the service knows it. These assert that promise against a
//! DEPLOYMENT, where a middleware or a route change can break it without any
//! in-process test noticing.
//!
//! Every case here is a CLIENT error. A 500 anywhere in these tables is a
//! failure of the service, not of the test.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{Response, ServiceClient, Simulation, reference_request, service};
use serde::Deserialize;

/// One case in the rejection table: what to send, and what the service must
/// answer. A struct rather than a tuple so each column is named at the point
/// it is written, which matters when a table has nine rows.
struct Case {
    /// What this case is, for the failure message.
    what: &'static str,
    /// The HTTP method.
    method: &'static str,
    /// The path, built from a live simulation where the case needs one.
    path: String,
    /// The request body, for the cases that carry one.
    body: Option<serde_json::Value>,
    /// The status the service must answer.
    status: u16,
    /// The field it must name, or empty where the service does not know one.
    field: &'static str,
}

/// The documented rejection body.
#[derive(Debug, Deserialize)]
struct Rejection {
    error: String,
    #[serde(default)]
    field: String,
}

/// Reads a rejection body, asserting the things that hold for EVERY rejection
/// before anything case-specific: JSON content type, a status in the 4xx
/// range, and a message that leaks nothing.
fn rejection(client: &ServiceClient, case: &str, response: &Response) -> Rejection {
    assert!(
        (400..500).contains(&response.status),
        "{case} must be a client error, got {} with {}",
        response.status,
        response.text()
    );

    let content_type = response.header("content-type").unwrap_or_default();
    assert!(
        content_type.contains("application/json"),
        "{case} answered {content_type:?} rather than JSON, which is what an unhandled path \
         produces: {}",
        response.text()
    );

    let body = response.text();
    assert_no_leak(case, &body, client);

    match serde_json::from_str::<Rejection>(&body) {
        Ok(rejection) => rejection,
        Err(error) => panic!(
            "{case} answered a body that is not the documented shape: {error}, body was {body}"
        ),
    }
}

/// A rejection may describe what the client got wrong and nothing about where
/// the service runs.
fn assert_no_leak(case: &str, body: &str, client: &ServiceClient) {
    let host = client
        .base_url()
        .trim_start_matches("http://")
        .split(':')
        .next()
        .unwrap_or_default();

    for forbidden in [
        "http://",
        "https://",
        "redis://",
        "mongodb://",
        "password",
        "/var/",
        "/etc/",
        "/usr/",
        "src/",
    ] {
        assert!(
            !body.to_ascii_lowercase().contains(forbidden),
            "{case} leaked {forbidden:?} in a rejection body: {body}"
        );
    }

    if !host.is_empty() && host != "localhost" && !host.starts_with("127.") {
        assert!(
            !body.contains(host),
            "{case} leaked the deployment host name in a rejection body: {body}"
        );
    }
}

/// Every v2 rejection this service documents, asserted by status AND by field.
#[test]
fn test_the_v2_rejection_contract_names_its_field() {
    let Some(client) = service() else {
        return;
    };

    let simulation = match Simulation::create(&client, &reference_request("SPX")) {
        Ok(simulation) => simulation,
        Err(error) => {
            println!("SKIP: this deployment has no usable v2 API: {error}");
            return;
        }
    };

    let cases = vec![
        Case {
            what: "a malformed id",
            method: "GET",
            path: "/api/v2/simulations/not-a-uuid".to_string(),
            body: None,
            status: 400,
            field: "id",
        },
        Case {
            // The service names the field it can act on: `from_step` is the
            // one that exceeds, so that is what a client has to change.
            what: "a reversed range",
            method: "GET",
            path: simulation.path("/export?dataset=underlying&format=csv&from_step=3&to_step=1"),
            body: None,
            status: 400,
            field: "from_step",
        },
        Case {
            what: "an unknown greek level",
            method: "GET",
            path: simulation.path("/snapshot?greeks=everything"),
            body: None,
            status: 400,
            field: "greeks",
        },
        Case {
            what: "an unknown query parameter",
            method: "GET",
            path: simulation.path("/snapshot?greek=first_order"),
            body: None,
            status: 400,
            field: "greek",
        },
        Case {
            what: "steps below one",
            method: "POST",
            path: "/api/v2/simulations".to_string(),
            body: Some(request_with("steps", serde_json::json!(0))),
            status: 400,
            field: "steps",
        },
        Case {
            what: "a non-positive initial price",
            method: "POST",
            path: "/api/v2/simulations".to_string(),
            body: Some(request_with("initial_price", serde_json::json!(0.0))),
            status: 400,
            field: "initial_price",
        },
        Case {
            what: "a non-positive volatility",
            method: "POST",
            path: "/api/v2/simulations".to_string(),
            body: Some(request_with("volatility", serde_json::json!(0.0))),
            status: 400,
            field: "volatility",
        },
        Case {
            what: "a zero step interval",
            method: "POST",
            path: "/api/v2/simulations".to_string(),
            body: Some(request_with("step_interval_seconds", serde_json::json!(0))),
            status: 400,
            field: "step_interval_seconds",
        },
        Case {
            what: "an unknown body key",
            method: "POST",
            path: "/api/v2/simulations".to_string(),
            body: Some(request_with("stepss", serde_json::json!(4))),
            status: 400,
            field: "",
        },
    ];

    let mut exercised = 0_usize;
    for case in cases {
        let Case {
            what,
            method,
            path,
            body,
            status,
            field,
        } = case;
        let rendered_body = body.map(|body| body.to_string());
        let response = match client.request(method, &path, rendered_body.as_deref()) {
            Ok(response) => response,
            Err(error) => panic!("{what}: {error}"),
        };

        // A deployment older than the route answers 404 for the path itself;
        // that is a lag to report, not a contract failure.
        if response.status == 404 && !path.contains("not-a-uuid") {
            println!("SKIP: {what} is not deployed here ({path})");
            continue;
        }

        let rejection = rejection(&client, what, &response);
        assert_eq!(
            response.status, status,
            "{what} must answer {status}, got {} with {}",
            response.status, rejection.error
        );
        if !field.is_empty() {
            assert_eq!(
                rejection.field, field,
                "{what} must name {field}, named {:?} with {}",
                rejection.field, rejection.error
            );
        }
        assert!(
            !rejection.error.is_empty(),
            "{what} must explain itself, not answer an empty message"
        );
        exercised += 1;
    }

    println!("INFO: {exercised} v2 rejection cases exercised");
}

/// A missing simulation is 404 on every verb that takes an id, never 400 and
/// never 500.
#[test]
fn test_a_missing_simulation_is_not_found_on_every_verb() {
    let Some(client) = service() else {
        return;
    };

    // A well-formed id that cannot exist.
    let absent = "/api/v2/simulations/00000000-0000-4000-8000-000000000000";

    for (method, path) in [
        ("GET", absent.to_string()),
        ("GET", format!("{absent}/snapshot")),
        ("POST", format!("{absent}/step")),
        ("DELETE", absent.to_string()),
    ] {
        let response = match client.request(method, &path, None) {
            Ok(response) => response,
            Err(error) => panic!("{method} {path}: {error}"),
        };

        if response.status == 405 {
            println!("SKIP: {method} {path} is not deployed here");
            continue;
        }

        assert_eq!(
            response.status,
            404,
            "{method} {path} must be 404 for a simulation that does not exist, got {} with {}",
            response.status,
            response.text()
        );
        assert_no_leak(&format!("{method} {path}"), &response.text(), &client);
    }
}

/// The v1 surface shares the SHAPE but not the handlers, and today it does not
/// share all of it.
///
/// What a live service answers, and what this therefore asserts:
///
/// - a malformed `sessionid` is a 400 whose body carries `error` and NO
///   `field` key, where v2 answers `field: "id"` for the same mistake;
/// - a missing `sessionid` is a 400 with an empty `field`.
///
/// That difference is recorded in issue #119 as a contract decision rather
/// than fixed here: `/api/v1/chain` is frozen on rendered values, so adding a
/// key to its error body changes what an existing client parses. This test
/// asserts the CURRENT behaviour deliberately, and moves with the decision.
#[test]
fn test_the_v1_rejection_contract_is_a_400_that_explains_itself() {
    let Some(client) = service() else {
        return;
    };

    for (case, path) in [
        (
            "a malformed session id",
            "/api/v1/chain?sessionid=not-a-uuid",
        ),
        ("a missing session id", "/api/v1/chain"),
    ] {
        let response = match client.get(path) {
            Ok(response) => response,
            Err(error) => panic!("{case}: {error}"),
        };

        assert_eq!(
            response.status,
            400,
            "{case} must be a client error, got {} with {}",
            response.status,
            response.text()
        );

        let content_type = response.header("content-type").unwrap_or_default();
        assert!(
            content_type.contains("application/json"),
            "{case} answered {content_type:?} rather than JSON: {}",
            response.text()
        );
        assert_no_leak(case, &response.text(), &client);

        // `field` is what issue #119 is about, so only `error` is required
        // here; when #119 is decided, this becomes an assertion on the field.
        let body: serde_json::Value = match response.json(path) {
            Ok(body) => body,
            Err(error) => panic!("{case} must answer JSON: {error}"),
        };
        let message = body.get("error").and_then(|error| error.as_str());
        assert!(
            message.is_some_and(|message| !message.is_empty()),
            "{case} must explain itself: {body}"
        );
        if body.get("field").is_none() {
            println!("INFO: {case} answers no `field` key, the v1 gap recorded in issue #119");
        }
    }
}

/// A body that does not deserialise at all still answers the documented
/// shape, rather than actix's own plaintext error.
#[test]
fn test_an_undeserialisable_body_still_answers_the_documented_shape() {
    let Some(client) = service() else {
        return;
    };

    let response = match client.request("POST", "/api/v2/simulations", Some("{not json at all")) {
        Ok(response) => response,
        Err(error) => panic!("{error}"),
    };

    if response.status == 404 {
        println!("SKIP: this deployment has no v2 API");
        return;
    }

    let rejection = rejection(&client, "an undeserialisable body", &response);
    assert!(
        !rejection.error.is_empty(),
        "a body that cannot be parsed must still say so"
    );
}

/// The reference request with one field replaced, so each case varies exactly
/// one thing.
fn request_with(field: &str, value: serde_json::Value) -> serde_json::Value {
    let mut request = reference_request("SPX");
    if let Some(object) = request.as_object_mut() {
        object.insert(field.to_string(), value);
    }
    request
}
