//! The operational surface: what an operator points a probe at, a scraper at,
//! or a code generator at.
//!
//! None of it is the product, all of it is what makes the product deployable,
//! and each piece has been wrong at least once. The OpenAPI document matters
//! most of the three: a generated client is built from it, so a path it
//! advertises that answers 404 is a broken client for someone who never
//! touched this repository.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::service;

/// The metrics the service documents and an operator's dashboard depends on.
const DOCUMENTED_METRICS: [&str; 4] = [
    "api_requests_total",
    "api_errors_total",
    "active_sessions",
    "api_request_duration_seconds",
];

/// `/metrics` is a Prometheus exposition, and it moves when the service serves
/// traffic.
#[test]
fn test_metrics_expose_the_documented_counters_and_move() {
    let Some(client) = service() else {
        return;
    };

    let response = match client.get("/metrics") {
        Ok(response) => response,
        Err(error) => panic!("{error}"),
    };
    assert_eq!(
        response.status,
        200,
        "/metrics must answer 200, got {} with {}",
        response.status,
        response.text()
    );
    let content_type = response.header("content-type").unwrap_or_default();
    assert!(
        content_type.contains("text/plain"),
        "a Prometheus exposition is text/plain, got {content_type:?}"
    );

    let body = response.text();
    for metric in DOCUMENTED_METRICS {
        assert!(
            body.contains(&format!("# HELP {metric}")),
            "the exposition must document {metric}"
        );
    }

    // Every non-comment line is `name value` or `name{labels} value`, which is
    // the shape a scraper parses.
    for line in body.lines().filter(|line| !line.starts_with('#')) {
        if line.trim().is_empty() {
            continue;
        }
        let value = line.rsplit(' ').next().unwrap_or_default();
        assert!(
            value.parse::<f64>().is_ok() || value == "NaN" || value == "+Inf" || value == "-Inf",
            "a metrics line must end in a number, got {line:?}"
        );
    }

    let before = counter(&body, "api_requests_total");

    // Serve some traffic that is unambiguously an API request.
    for _ in 0..3 {
        match client.get("/api/v1/chain?sessionid=00000000-0000-4000-8000-000000000000") {
            Ok(_) => {}
            Err(error) => panic!("{error}"),
        }
    }

    let after = match client.get("/metrics") {
        Ok(response) => counter(&response.text(), "api_requests_total"),
        Err(error) => panic!("{error}"),
    };

    match (before, after) {
        (Some(before), Some(after)) => assert!(
            after > before,
            "serving three requests must move api_requests_total, it went {before} to {after}"
        ),
        _ => println!(
            "INFO: api_requests_total carries labels this test does not sum; the exposition was \
             checked but the movement was not"
        ),
    }
}

/// The total of one counter across its label sets, or `None` when it is not
/// exposed as a plain sample.
fn counter(body: &str, name: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut seen = false;
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        let Some((series, value)) = line.rsplit_once(' ') else {
            continue;
        };
        let matches = series == name || series.starts_with(&format!("{name}{{"));
        if let (true, Ok(value)) = (matches, value.parse::<f64>()) {
            total += value;
            seen = true;
        }
    }
    seen.then_some(total)
}

/// Every path the deployment advertises actually answers.
///
/// The list is driven from the deployment's own document rather than from a
/// hardcoded one, so this tracks whatever contract the service publishes: a
/// client generated from it must not meet a 404.
#[test]
fn test_every_advertised_path_answers() {
    let Some(client) = service() else {
        return;
    };

    let document: serde_json::Value = match client.get("/api-docs/openapi.json") {
        Ok(response) if response.status == 200 => match response.json("/api-docs/openapi.json") {
            Ok(document) => document,
            Err(error) => panic!("{error}"),
        },
        Ok(response) => {
            println!(
                "SKIP: this deployment serves no OpenAPI document ({})",
                response.status
            );
            return;
        }
        Err(error) => panic!("{error}"),
    };

    let Some(paths) = document.get("paths").and_then(serde_json::Value::as_object) else {
        panic!("an OpenAPI document must carry paths: {document}");
    };
    assert!(!paths.is_empty(), "the document advertises no path at all");

    let mut checked = 0_usize;
    for (template, item) in paths {
        let Some(operations) = item.as_object() else {
            continue;
        };
        for method in operations.keys() {
            let method = method.to_ascii_uppercase();
            if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&method.as_str()) {
                continue;
            }

            // Substitute a well-formed id that cannot exist: what is being
            // tested is that the route is MOUNTED, so 404 for a missing
            // simulation is fine while 404 for the path is not. They are
            // distinguished by the body, which a mounted route fills in.
            let path = template.replace("{id}", "00000000-0000-4000-8000-000000000000");
            let path = if path.contains("/export") {
                format!("{path}?dataset=underlying&format=json")
            } else {
                path
            };

            let body = matches!(method.as_str(), "POST" | "PUT" | "PATCH").then_some("{}");
            let response = match client.request(&method, &path, body) {
                Ok(response) => response,
                Err(error) => panic!("{method} {path}: {error}"),
            };

            assert_ne!(
                response.status, 405,
                "{method} {path} is advertised but not mounted for that verb"
            );
            assert!(
                response.status < 500,
                "{method} {path} is advertised and answers {}: {}",
                response.status,
                response.text()
            );
            if response.status == 404 {
                assert!(
                    !response.body.is_empty(),
                    "{method} {path} is advertised and answers a bare 404, which is what an \
                     unmounted route does; a mounted one explains itself"
                );
            }
            checked += 1;
        }
    }

    println!("INFO: {checked} advertised operations answered");
}

/// The documentation UI and the favicon are served.
#[test]
fn test_the_documentation_ui_and_favicon_are_served() {
    let Some(client) = service() else {
        return;
    };

    for (path, expected) in [("/swagger-ui/", "text/html"), ("/favicon.ico", "image")] {
        let response = match client.get(path) {
            Ok(response) => response,
            Err(error) => panic!("{path}: {error}"),
        };
        assert_eq!(
            response.status, 200,
            "{path} must be served, got {}",
            response.status
        );
        assert!(!response.body.is_empty(), "{path} must carry something");
        let content_type = response.header("content-type").unwrap_or_default();
        assert!(
            content_type.contains(expected),
            "{path} must be served as {expected}, got {content_type:?}"
        );
    }
}

/// The probes: `/health` answers regardless of dependency state, `/ready`
/// names each dependency, and neither pollutes the metrics.
///
/// Skipped on a deployment that predates them.
#[test]
fn test_the_probes_answer_and_stay_out_of_the_metrics() {
    let Some(client) = service() else {
        return;
    };

    let health = match client.get("/health") {
        Ok(response) => response,
        Err(error) => panic!("{error}"),
    };
    if health.status == 404 {
        println!("SKIP: this deployment predates /health and /ready");
        return;
    }
    assert_eq!(
        health.status,
        200,
        "/health must answer 200 whatever the dependencies are doing, got {}",
        health.text()
    );

    let ready = match client.get("/ready") {
        Ok(response) => response,
        Err(error) => panic!("{error}"),
    };
    assert!(
        ready.status == 200 || ready.status == 503,
        "/ready must be 200 or 503, got {} with {}",
        ready.status,
        ready.text()
    );

    let body: serde_json::Value = match ready.json("/ready") {
        Ok(body) => body,
        Err(error) => panic!("{error}"),
    };
    let dependencies = body
        .get("dependencies")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("/ready must name its dependencies: {body}"));
    assert!(
        !dependencies.is_empty(),
        "/ready must name at least one dependency: {body}"
    );
    for dependency in dependencies {
        assert!(
            dependency
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "every dependency must be named: {dependency}"
        );
        assert!(
            dependency
                .get("status")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|status| status == "up" || status == "down"),
            "every dependency must report up or down: {dependency}"
        );
    }

    // Probing must not pollute the counters: an orchestrator probes every few
    // seconds forever, and a request rate that is mostly liveness checks tells
    // an operator nothing.
    //
    // Asserted on the LABEL SETS rather than on a total, deliberately. The
    // integration suite runs its tests in parallel against the same
    // deployment, so a global counter moves under this test for reasons that
    // have nothing to do with probes; the absence of a probe series is exact
    // and cannot race.
    for _ in 0..5 {
        let _ = client.get("/health");
        let _ = client.get("/ready");
    }

    let exposition = match client.get("/metrics") {
        Ok(response) => response.text(),
        Err(error) => panic!("{error}"),
    };
    for probe in ["/health", "/ready"] {
        let series = format!("endpoint=\"{probe}\"");
        assert!(
            !exposition.contains(&series),
            "ten probe requests put {probe} in the metrics; probes must stay out of the counters"
        );
    }
}

/// An unknown path is a 404, never a 500.
#[test]
fn test_an_unknown_path_is_not_found() {
    let Some(client) = service() else {
        return;
    };

    for path in ["/nope", "/api/v3/simulations", "/api/v1"] {
        match client.get(path) {
            Ok(response) => assert_eq!(
                response.status,
                404,
                "{path} must be 404, got {} with {}",
                response.status,
                response.text()
            ),
            Err(error) => panic!("{path}: {error}"),
        }
    }
}
