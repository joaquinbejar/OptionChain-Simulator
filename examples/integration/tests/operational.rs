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

    // The EXACT series this test moves: one endpoint, one method, one status.
    // Summing every label set would let parallel traffic from the other tests
    // in this suite satisfy the assertion without these three requests being
    // recorded at all.
    let series = "api_requests_total{endpoint=\"/api/v1/chain\",method=\"GET\",status=\"404\"}";
    let before = sample(&body, series).unwrap_or(0.0);

    // Three requests that land on exactly that series: a well-formed id that
    // cannot exist is a 404 from the v1 route.
    let absent = "/api/v1/chain?sessionid=00000000-0000-4000-8000-000000000000";
    let requests = 3;
    for _ in 0..requests {
        match client.get(absent) {
            Ok(response) => assert_eq!(
                response.status,
                404,
                "the fixture request must be a 404 for this series to mean anything, got {} \
                 with {}",
                response.status,
                response.text()
            ),
            Err(error) => panic!("{error}"),
        }
    }

    let after = match client.get("/metrics") {
        Ok(response) => sample(&response.text(), series).unwrap_or(0.0),
        Err(error) => panic!("{error}"),
    };
    let moved = after - before;

    // A counter is per PROCESS, and a deployment may run several behind one
    // address: this suite found exactly that, two replicas answering in turn,
    // where three requests and the scrape after them land on whichever
    // instance the balancer picked. So the assertion is what holds for one
    // instance or many — the series moved, and by no more than the traffic
    // this test generated — rather than an exact delta that only holds on a
    // single-process deployment.
    assert!(
        moved >= 1.0,
        "three requests moved {series} by {moved}; a served request must be counted somewhere"
    );
    assert!(
        moved <= f64::from(requests),
        "{series} moved {moved} for {requests} requests, which is more traffic than this test \
         produced"
    );
    if moved < f64::from(requests) {
        println!(
            "INFO: {series} moved {moved} for {requests} requests, so this deployment serves \
             them from more than one process; a scrape sees one instance at a time"
        );
    }
}

/// One exact series, or `None` when the exposition does not carry it.
///
/// An absent series is a real answer: a counter with no observations yet is
/// zero, and the caller decides what that means.
fn sample(body: &str, series: &str) -> Option<f64> {
    body.lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let (name, value) = line.rsplit_once(' ')?;
            (name == series).then(|| value.parse::<f64>().ok())?
        })
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
        // The document IS the contract this test exists to enforce. A
        // deployment that stopped serving it is the regression, and skipping
        // would remove the whole advertised-route check exactly when it
        // matters.
        Ok(response) => panic!(
            "the deployment must serve an OpenAPI document, it answered {}: {}",
            response.status,
            response.text()
        ),
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
                // A 404 is only acceptable where the operation takes an id: it
                // then means "no such simulation", which is a documented
                // result. Everywhere else it means the route is not mounted.
                assert!(
                    template.contains("{id}"),
                    "{method} {path} takes no id, so a 404 means the route is not mounted"
                );

                // And it must be THIS service's 404, not a proxy's or a
                // framework default: proxies answer HTML happily, which is why
                // a non-empty body proves nothing.
                let content_type = response.header("content-type").unwrap_or_default();
                assert!(
                    content_type.contains("application/json"),
                    "{method} {path} answered a {content_type:?} 404, which is what a proxy or \
                     an unmounted route produces; this service answers JSON"
                );
                let body: serde_json::Value = match response.json(&path) {
                    Ok(body) => body,
                    Err(error) => {
                        panic!("{method} {path}: a 404 must be this service's shape: {error}")
                    }
                };
                assert!(
                    body.get("error")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|message| !message.is_empty()),
                    "{method} {path}: a 404 must explain itself: {body}"
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

    // The dependencies this service has. A readiness answer that stopped
    // probing one of them would otherwise look healthier than the deployment
    // is.
    let named: Vec<&str> = dependencies
        .iter()
        .filter_map(|dependency| dependency.get("name").and_then(serde_json::Value::as_str))
        .collect();
    for required in ["redis", "mongodb"] {
        assert!(
            named.contains(&required),
            "/ready must probe {required}, it named {named:?}"
        );
    }

    let mut all_up = true;
    for dependency in dependencies {
        let name = dependency
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("every dependency must be named: {dependency}"));
        let status = dependency
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("{name} must report a status: {dependency}"));
        assert!(
            status == "up" || status == "down",
            "{name} reports {status:?}, where the contract is up or down"
        );

        match status {
            "up" => assert!(
                dependency.get("reason").is_none()
                    || dependency.get("reason") == Some(&serde_json::Value::Null),
                "{name} is up and still carries a reason: {dependency}"
            ),
            _ => {
                all_up = false;
                // A reason is a CATEGORY, never a message: anything else risks
                // carrying a host name or a credential into an operator's logs.
                let reason = dependency
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_else(|| panic!("{name} is down and says nothing: {dependency}"));
                assert!(
                    reason == "unreachable" || reason == "timed out",
                    "{name} is down for {reason:?}, which is not one of the documented \
                     categories"
                );
            }
        }
    }

    // The aggregate must follow from the parts, in both directions: a 200 with
    // something down, or a 503 with everything up, is a probe an orchestrator
    // cannot trust.
    let status = body
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("/ready must report an aggregate status: {body}"));
    let expected = if all_up {
        ("ready", 200)
    } else {
        ("not_ready", 503)
    };
    assert_eq!(
        (status, ready.status),
        expected,
        "the aggregate must follow from the dependencies: {body}"
    );

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
