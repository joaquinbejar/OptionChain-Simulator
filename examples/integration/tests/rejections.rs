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
///
/// `field` is an `Option` rather than a defaulted `String` on purpose: a body
/// that omits the key and one that carries an empty string are different
/// facts, and #119 is about exactly that difference on the v1 surface.
#[derive(Debug, Deserialize)]
struct Rejection {
    error: String,
    field: Option<String>,
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

    // A deployment without the v2 API answers 404 for the route itself, which
    // is the only creation failure that means "not deployed here". Anything
    // else — a 500, a wrong success status, a body that will not decode — is a
    // defect, and turning it into a green skip is how a suite quietly stops
    // testing anything.
    let probe = match client.post("/api/v2/simulations", &reference_request("SPX")) {
        Ok(probe) => probe,
        Err(error) => panic!("creating a simulation: {error}"),
    };
    if probe.status == 404 {
        println!("SKIP: this deployment does not serve /api/v2/simulations");
        return;
    }
    assert_eq!(
        probe.status,
        201,
        "creating a simulation must answer 201, got {} with {}",
        probe.status,
        probe.text()
    );
    let simulation = match Simulation::create(&client, &reference_request("SPX")) {
        Ok(simulation) => simulation,
        Err(error) => panic!("the v2 API is deployed, so creating a simulation must work: {error}"),
    };
    // The probe created one too; delete it rather than leaving it behind.
    if let Ok(Some(id)) = probe
        .json::<serde_json::Value>("/api/v2/simulations")
        .map(|body| {
            body.get("id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
    {
        examples_integration::report_cleanup(
            &client,
            &format!("/api/v2/simulations/{id}"),
            "the probe simulation",
        );
    }

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
            // Issue #104 names this one explicitly: a range whose end is past
            // the tape must name `to_step`, which is the field at fault here,
            // unlike the inverted range above.
            what: "a range past the end of the tape",
            method: "GET",
            path: simulation
                .path("/export?dataset=underlying&format=csv&from_step=0&to_step=99999"),
            body: None,
            status: 400,
            field: "to_step",
        },
        Case {
            what: "steps above the cap",
            method: "POST",
            path: "/api/v2/simulations".to_string(),
            body: Some(request_with("steps", serde_json::json!(10_000_000))),
            status: 400,
            field: "steps",
        },
        Case {
            what: "a chain size above the cap",
            method: "POST",
            path: "/api/v2/simulations".to_string(),
            body: Some(request_with("chain_size", serde_json::json!(10_000_000))),
            status: 400,
            field: "chain_size",
        },
        Case {
            what: "an unknown body key",
            method: "POST",
            path: "/api/v2/simulations".to_string(),
            body: Some(request_with("stepss", serde_json::json!(4))),
            status: 400,
            // The handler recovers the offending key, so the contract is that
            // it names it rather than shrugging.
            field: "stepss",
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

        // A regression that ACCEPTS one of these requests hands back a
        // simulation, and failing before deleting it would leave it on a
        // shared deployment. Clean up first, then fail.
        if response.status == 200 || response.status == 201 {
            if let Ok(Some(id)) = response.json::<serde_json::Value>(&path).map(|body| {
                body.get("id")
                    .and_then(|id| id.as_str())
                    .map(str::to_string)
            }) {
                examples_integration::report_cleanup(
                    &client,
                    &format!("/api/v2/simulations/{id}"),
                    "a simulation an invalid request should not have created",
                );
            }
            panic!(
                "{what} was ACCEPTED with {}: {}",
                response.status,
                response.text()
            );
        }

        let rejection = rejection(&client, what, &response);
        assert_eq!(
            response.status, status,
            "{what} must answer {status}, got {} with {}",
            response.status, rejection.error
        );
        assert_eq!(
            rejection.field.as_deref(),
            Some(field),
            "{what} must name {field:?}, named {:?} with {}",
            rejection.field,
            rejection.error
        );
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

        // The same shared assertions as every other rejection: a JSON body in
        // the documented shape, leaking nothing. A plaintext 404 is what an
        // unmounted route produces and must not pass here.
        let what = format!("{method} {path}");
        let rejection = rejection(&client, &what, &response);
        assert!(
            !rejection.error.is_empty(),
            "{what} must explain itself rather than answer an empty message"
        );
        // A not-found carries no field: no request field is at fault, the
        // resource simply is not there. Deliberate, so asserted.
        assert_eq!(
            rejection.field, None,
            "{what} names a field, which a not-found has no business doing: {:?}",
            rejection.field
        );
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

    // What a live service answers, asserted exactly, so a change in either
    // direction is visible rather than silently tolerated: the malformed case
    // carries no `field` key at all and renders with an `Invalid State`
    // prefix, and the missing one carries an empty field.
    //
    // Issue #119 DECIDED this rather than left it open: v1 is frozen on
    // rendered values, so the shapes stay and are documented in the OpenAPI
    // document and the crate docs instead. These assertions are therefore the
    // contract now, not a snapshot of an accident.
    for (case, path, expects_field, prefix) in [
        (
            "a malformed session id",
            "/api/v1/chain?sessionid=not-a-uuid",
            None,
            "Invalid State",
        ),
        (
            "a missing session id",
            "/api/v1/chain",
            Some(String::new()),
            "Query deserialize error",
        ),
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

        let rejection = rejection(&client, case, &response);
        assert!(
            rejection.error.starts_with(prefix),
            "{case} renders as {prefix:?} today, which is part of what #119 decides; it \
             rendered as {:?}",
            rejection.error
        );
        assert_eq!(
            rejection.field, expects_field,
            "{case} carries {expects_field:?} today, which is the v1 gap #119 records; it \
             carried {:?}",
            rejection.field
        );
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
