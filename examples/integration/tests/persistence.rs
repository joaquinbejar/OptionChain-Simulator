//! Which of the two export paths a deployment actually exercises.
//!
//! An export can be served two ways. With snapshot persistence off, the tape is
//! replayed: every step is walked again to render the rows. With it on, the
//! steps a client has already been served are in the warehouse, and the export
//! reads them back rather than re-walking, which is the whole point of storing
//! them.
//!
//! The rest of this suite cannot tell the two apart, and that is the problem
//! (issue #149): every comparison it makes has replay on both sides, so a
//! warehouse returning subtly different rows would be invisible. These tests
//! say which path the deployment in front of them uses, and hold the two to
//! producing the same bytes where they can be compared.
//!
//! What they cannot do is force the path. Nothing in the client-facing surface
//! reports whether an export was read back or re-walked, so a deployment with
//! persistence on and an empty warehouse simply replays and still passes. The
//! honest claim is the one asserted: the rows a client is served do not depend
//! on where they came from.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{Response, ServiceClient, reference_request, report_cleanup, service};

/// Steps in the tapes compared below.
const STEPS: usize = 4;

/// How long to let the warehouse queue drain before comparing, in attempts of
/// a fifth of a second.
///
/// The write is detached from the request that produced it — that is what
/// keeps a degraded warehouse from delaying a response — so a step served is
/// not a row stored yet.
const SETTLE_ATTEMPTS: usize = 25;

/// Whether this deployment persists its snapshots.
///
/// Read from the readiness probe rather than from a knob nobody exposes: the
/// warehouse probe is registered only when the manager has a warehouse, so a
/// `clickhouse` dependency in the body IS persistence being on.
fn persistence_is_on(client: &ServiceClient) -> Option<bool> {
    let response = match client.get("/ready") {
        Ok(response) => response,
        Err(error) => panic!("{error}"),
    };
    if response.status != 200 && response.status != 503 {
        panic!(
            "the readiness probe must answer 200 or 503, it answered {}",
            response.status
        );
    }
    let body: serde_json::Value = match response.json("/ready") {
        Ok(body) => body,
        Err(error) => panic!("{error}"),
    };
    let dependencies = body
        .get("dependencies")
        .and_then(serde_json::Value::as_array)?;

    Some(dependencies.iter().any(|dependency| {
        dependency.get("name").and_then(serde_json::Value::as_str) == Some("clickhouse")
    }))
}

/// A simulation with a chosen seed that deletes itself.
struct Tape {
    client: ServiceClient,
    id: String,
}

impl Tape {
    fn create(client: &ServiceClient, seed: u64) -> Option<Self> {
        let mut request = reference_request("SPX");
        if let Some(object) = request.as_object_mut() {
            object.insert("steps".to_string(), serde_json::json!(STEPS));
            object.insert("chain_size".to_string(), serde_json::json!(2));
            object.insert("seed".to_string(), serde_json::json!(seed));
            object.insert(
                "start_at".to_string(),
                serde_json::json!("2026-01-05T14:30:00Z"),
            );
        }

        let response = match client.post("/api/v2/simulations", &request) {
            Ok(response) => response,
            Err(error) => panic!("{error}"),
        };
        if response.status == 404 {
            println!("SKIP: this deployment does not serve /api/v2/simulations");
            return None;
        }
        assert_eq!(
            response.status,
            201,
            "creating a simulation must answer 201, got {}",
            response.text()
        );

        let body: serde_json::Value = match response.json("/api/v2/simulations") {
            Ok(body) => body,
            Err(error) => panic!("{error}"),
        };
        let id = match body.get("id").and_then(serde_json::Value::as_str) {
            Some(id) => id.to_string(),
            None => panic!("a created simulation must carry an id: {body}"),
        };

        Some(Self {
            client: client.clone(),
            id,
        })
    }

    /// Serves every step, which is what gives the warehouse something to file.
    fn walk(&self) {
        for step in 0..STEPS {
            let path = format!("/api/v2/simulations/{}/step", self.id);
            match self.client.request("POST", &path, None) {
                Ok(response) => assert_eq!(
                    response.status,
                    200,
                    "step {step} must serve: {}",
                    response.text()
                ),
                Err(error) => panic!("{error}"),
            }
        }
    }

    fn export(&self, query: &str) -> Response {
        let path = format!("/api/v2/simulations/{}/export?{query}", self.id);
        match self.client.get(&path) {
            Ok(response) => response,
            Err(error) => panic!("exporting {query}: {error}"),
        }
    }
}

impl Drop for Tape {
    fn drop(&mut self) {
        let path = format!("/api/v2/simulations/{}", self.id);
        report_cleanup(&self.client, &path, &self.id);
    }
}

/// Says which export path this deployment exercises, and never fails on it.
///
/// Both are valid deployments. What is not valid is a suite that tests one of
/// them and reports as though it had covered both, which is what every run
/// before this did.
#[test]
fn test_the_deployment_says_whether_it_persists_its_snapshots() {
    let Some(client) = service() else {
        return;
    };
    match persistence_is_on(&client) {
        Some(true) => println!(
            "INFO: snapshot persistence is ON here, so an export of steps already served can be \
             read back from the warehouse"
        ),
        Some(false) => println!(
            "INFO: snapshot persistence is OFF here, so every export replays the tape and the \
             warehouse read path is not exercised by this run"
        ),
        None => panic!("the readiness body must list its dependencies"),
    }
}

/// A tape whose steps were served exports the same bytes as one exported
/// straight away.
///
/// The first has been through the warehouse where there is one; the second is
/// rendered from a walk that happens inside the export itself. A row that
/// survived a round trip through storage differently — a truncated decimal, a
/// timestamp that lost its zone, a column that came back in another order —
/// would show here and nowhere else in this suite, because every other
/// comparison it makes has replay on both sides.
///
/// Skipped, loudly, when persistence is off: both sides would then be the same
/// path and the test would assert nothing.
#[test]
fn test_a_stored_tape_exports_what_a_replayed_one_does() {
    let Some(client) = service() else {
        return;
    };
    match persistence_is_on(&client) {
        Some(true) => {}
        Some(false) => {
            println!(
                "SKIP: snapshot persistence is off on this deployment, so both sides of this \
                 comparison would be the same replay"
            );
            return;
        }
        None => panic!("the readiness body must list its dependencies"),
    }

    let (Some(served), Some(fresh)) = (Tape::create(&client, 8_191), Tape::create(&client, 8_191))
    else {
        return;
    };

    // Only the first is walked. Its steps are what the warehouse has to file;
    // the second's tape exists only inside the export that asks for it.
    served.walk();

    let mut near_misses = 0_usize;
    for dataset in ["underlying", "option_chains"] {
        let query = format!("dataset={dataset}&format=csv");
        let expected = fresh.export(&query);
        assert_eq!(
            expected.status,
            200,
            "a replayed {dataset} export must serve: {}",
            expected.text()
        );

        // The write is detached from the advance that caused it, so the rows
        // may still be in the queue. Retried rather than slept on once: rows
        // that arrive make the two agree, and a disagreement that survives the
        // budget is the finding.
        let mut stored_body = Vec::new();
        let mut differences = Vec::new();
        for _ in 0..SETTLE_ATTEMPTS {
            let stored = served.export(&query);
            assert_eq!(
                stored.status,
                200,
                "a stored {dataset} export must serve: {}",
                stored.text()
            );
            stored_body = stored.body.clone();
            differences = compare(dataset, &stored_body, &expected.body);
            if differences.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        assert!(
            differences.is_empty(),
            "the {dataset} export of a tape whose steps were served differs from the same export \
             of the same seed rendered on the spot: {differences:?}. Same seed, same parameters, \
             so the rows must not depend on whether they came back from storage"
        );

        if stored_body != expected.body {
            // Equal as values, different as bytes. Storage renders a mid price
            // a digit or two shorter than the replay does, because it crosses
            // a float on the way in and back. It is not a corrupted row, and
            // it is not nothing either: a consumer that diffs two exports of
            // the same tape sees a change that is not there.
            near_misses += 1;
            println!(
                "INFO: the {dataset} export agrees value by value but not byte for byte, {} bytes \
                 stored against {} replayed, which is issue #152",
                stored_body.len(),
                expected.body.len()
            );
        }
    }

    if near_misses == 0 {
        println!(
            "INFO: a stored tape and a replayed one export the same bytes for every dataset \
             compared"
        );
    }
}

/// Every cell of two CSV exports, compared as values.
///
/// Numbers are compared as numbers, within a relative tolerance far tighter
/// than anything a client could act on but wide enough to ignore the last bit
/// of a float. Everything else is compared as text, so a timestamp that lost
/// its zone, a column that came back in another order or a row that never
/// arrived is a difference, which is what a warehouse can plausibly get wrong.
fn compare(dataset: &str, stored: &[u8], replayed: &[u8]) -> Vec<String> {
    let (Ok(stored), Ok(replayed)) = (std::str::from_utf8(stored), std::str::from_utf8(replayed))
    else {
        return vec![format!("the {dataset} export is not text")];
    };

    let stored: Vec<&str> = stored.lines().collect();
    let replayed: Vec<&str> = replayed.lines().collect();
    if stored.len() != replayed.len() {
        return vec![format!(
            "{dataset} has {} rows stored against {} replayed",
            stored.len(),
            replayed.len()
        )];
    }

    let mut differences = Vec::new();
    for (index, (left, right)) in stored.iter().zip(replayed.iter()).enumerate() {
        let left_cells: Vec<&str> = left.split(',').collect();
        let right_cells: Vec<&str> = right.split(',').collect();
        if left_cells.len() != right_cells.len() {
            differences.push(format!(
                "{dataset} row {index} has {} columns stored against {}",
                left_cells.len(),
                right_cells.len()
            ));
            continue;
        }
        for (column, (left, right)) in left_cells.iter().zip(right_cells.iter()).enumerate() {
            if left == right {
                continue;
            }
            match (left.parse::<f64>(), right.parse::<f64>()) {
                (Ok(left_value), Ok(right_value)) => {
                    let scale = left_value.abs().max(right_value.abs()).max(1.0);
                    if (left_value - right_value).abs() > scale * 1e-9 {
                        differences.push(format!(
                            "{dataset} row {index} column {column} is {left} stored against \
                             {right} replayed"
                        ));
                    }
                }
                _ => differences.push(format!(
                    "{dataset} row {index} column {column} is {left:?} stored against \
                     {right:?} replayed"
                )),
            }
        }
    }
    differences
}
