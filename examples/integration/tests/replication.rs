//! What a client sees when more than one instance answers.
//!
//! This service is meant to run replicated: session and simulation state lives
//! in Redis and every write there is an atomic Lua script, so two instances
//! cannot both commit the same step. These tests hold the DEPLOYMENT to that,
//! from outside, where a balancer decides which instance takes each request.
//!
//! They are written to pass against a single instance too. A deployment that
//! runs one process satisfies every one of them trivially, which is the point:
//! the client contract does not depend on how many processes are behind the
//! address.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{
    ServiceClient, instances_behind, reference_request, report_cleanup, service,
};

/// Steps walked by the tests that need a tape.
const STEPS: usize = 8;

/// How many scrapes are used to count instances.
const SCRAPES: usize = 8;

/// A simulation that deletes itself.
struct Live {
    client: ServiceClient,
    id: String,
}

impl Live {
    /// Creates a simulation with an explicit seed and a narrow chain.
    fn create(client: &ServiceClient, steps: usize, seed: u64) -> Option<Self> {
        let mut request = reference_request("SPX");
        if let Some(object) = request.as_object_mut() {
            object.insert("steps".to_string(), serde_json::json!(steps));
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

    /// This simulation's path, with an optional suffix.
    fn path(&self, suffix: &str) -> String {
        format!("/api/v2/simulations/{}{suffix}", self.id)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        report_cleanup(&self.client, &self.path(""), &self.id);
    }
}

/// Reports how many instances answered, so every other test's output can be
/// read in that light.
///
/// Never fails on the count: one instance is a valid deployment and so is ten.
#[test]
fn test_the_deployment_reports_how_many_instances_answer() {
    let Some(client) = service() else {
        return;
    };

    let instances = instances_behind(&client, SCRAPES);
    assert!(instances >= 1, "something must be answering");
    if instances == 1 {
        println!(
            "INFO: one instance answered every scrape; the replicated paths below are still exercised, trivially"
        );
    } else {
        println!("INFO: at least {instances} instances are behind this address");
    }
}

/// A simulation created through the balancer is immediately readable through
/// it, whichever instance takes each request.
///
/// Redis holds the document, so this is really a test that no instance is
/// serving from a local store: with an in-memory store and two replicas, half
/// these reads would be 404.
#[test]
fn test_a_new_simulation_is_visible_from_every_instance() {
    let Some(client) = service() else {
        return;
    };
    let Some(live) = Live::create(&client, STEPS, 4242) else {
        return;
    };

    // Enough reads to cross instances several times over.
    let path = live.path("");
    for attempt in 0..(SCRAPES * 2) {
        match client.get(&path) {
            Ok(response) => assert_eq!(
                response.status, 200,
                "read {attempt} of a simulation that exists answered {}; an instance serving \
                 from its own memory rather than the shared store would look exactly like this",
                response.status
            ),
            Err(error) => panic!("{error}"),
        }
    }
}

/// Walking a simulation through the balancer serves every step exactly once,
/// in order, with no gaps and no repeats.
///
/// This is the cross-instance half of the compare-and-swap the Redis store
/// implements: whichever instance takes an advance, the cursor is the shared
/// one, so the served steps must be exactly `0..N-1`.
#[test]
fn test_a_walk_through_the_balancer_serves_every_step_once() {
    let Some(client) = service() else {
        return;
    };
    let Some(live) = Live::create(&client, STEPS, 7) else {
        return;
    };

    let mut served = Vec::new();
    for index in 0..STEPS {
        let path = live.path("/step");
        let response = match client.request("POST", &path, None) {
            Ok(response) => response,
            Err(error) => panic!("{error}"),
        };
        assert_eq!(
            response.status,
            200,
            "advance {index} answered {}: {}",
            response.status,
            response.text()
        );
        let body: serde_json::Value = match response.json(&path) {
            Ok(body) => body,
            Err(error) => panic!("{error}"),
        };
        let cursor = body
            .get("cursor")
            .and_then(|cursor| cursor.get("current_step"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| panic!("a served snapshot must carry its cursor: {body}"));
        served.push(cursor);
    }

    let expected: Vec<u64> = (1..=STEPS as u64).collect();
    assert_eq!(
        served, expected,
        "the walk must report the cursor after each serve, once each and in order, whichever \
         instance answered"
    );

    // And it is finished for everyone, not just for the instance that served
    // the last step.
    let past_the_end = match client.request("POST", &live.path("/step"), None) {
        Ok(response) => response,
        Err(error) => panic!("{error}"),
    };
    assert_eq!(
        past_the_end.status,
        410,
        "a completed simulation must be completed on every instance, got {}",
        past_the_end.text()
    );
}

/// The same seed produces the same market whichever instance serves each step.
///
/// The reproducibility file compares two simulations against each other, which
/// cannot see a divergence that affects both equally. This walks one
/// simulation through the balancer and compares it against a tape built from a
/// second simulation with identical parameters, then requires each step to
/// match the corresponding step of the other: a per-instance difference in how
/// a step is priced would break exactly this.
#[test]
fn test_a_tape_is_the_same_whichever_instance_serves_it() {
    let Some(client) = service() else {
        return;
    };
    let (Some(left), Some(right)) = (
        Live::create(&client, STEPS, 99),
        Live::create(&client, STEPS, 99),
    ) else {
        return;
    };

    let market = |body: &serde_json::Value| -> serde_json::Value {
        let mut market = serde_json::Map::new();
        for key in ["simulated_at", "underlying", "chains"] {
            let value = body
                .get(key)
                .unwrap_or_else(|| panic!("a snapshot must carry {key}: {body}"));
            market.insert(key.to_string(), value.clone());
        }
        serde_json::Value::Object(market)
    };

    let serve = |live: &Live| -> serde_json::Value {
        let path = live.path("/step");
        match client.request("POST", &path, None) {
            Ok(response) => {
                assert_eq!(
                    response.status,
                    200,
                    "a step must serve: {}",
                    response.text()
                );
                match response.json::<serde_json::Value>(&path) {
                    Ok(body) => market(&body),
                    Err(error) => panic!("{error}"),
                }
            }
            Err(error) => panic!("{error}"),
        }
    };

    // Interleaved, so consecutive steps of each simulation land on different
    // instances as often as the balancer allows.
    for step in 0..STEPS {
        let one = serve(&left);
        let two = serve(&right);
        assert_eq!(
            one, two,
            "step {step} differs between two simulations with the same seed; whichever instance \
             served each one, the market must be the same"
        );
    }
}

/// An export of one simulation is byte-identical however many instances serve
/// it.
///
/// Repeated enough times to cross instances, which turns the byte-identity
/// contract from a statement about one process into one about the deployment:
/// a per-instance difference in rendering, ordering or block boundaries would
/// show up here and nowhere else.
#[test]
fn test_an_export_is_byte_identical_across_instances() {
    let Some(client) = service() else {
        return;
    };
    let Some(live) = Live::create(&client, STEPS, 31337) else {
        return;
    };

    for _ in 0..STEPS {
        match client.request("POST", &live.path("/step"), None) {
            Ok(response) => assert_eq!(response.status, 200, "{}", response.text()),
            Err(error) => panic!("{error}"),
        }
    }

    let query = live.path("/export?dataset=option_chains&format=csv&greeks=all");
    let first = match client.get(&query) {
        Ok(response) => {
            assert_eq!(response.status, 200, "{}", response.text());
            response.body
        }
        Err(error) => panic!("{error}"),
    };

    for attempt in 1..(SCRAPES * 2) {
        let again = match client.get(&query) {
            Ok(response) => {
                assert_eq!(response.status, 200, "{}", response.text());
                response.body
            }
            Err(error) => panic!("{error}"),
        };
        assert_eq!(
            again.len(),
            first.len(),
            "export {attempt} is {} bytes against {} the first time; whichever instance built \
             it, the bytes must match",
            again.len(),
            first.len()
        );
        assert!(
            again == first,
            "export {attempt} differs from the first byte for byte, so two instances render the \
             same tape differently"
        );
    }
}

/// The probes describe the instance that answers them, and a deployment whose
/// instances disagree says so rather than averaging.
///
/// `/ready` is asked repeatedly: every answer must be internally consistent —
/// ready if and only if every dependency it names is up — regardless of which
/// instance produced it. An instance whose Redis is gone must not be able to
/// hide behind one whose Redis is fine, because each answer is that instance's
/// own.
#[test]
fn test_every_readiness_answer_is_consistent_with_itself() {
    let Some(client) = service() else {
        return;
    };

    let mut ready_answers = 0_usize;
    let mut not_ready_answers = 0_usize;

    for attempt in 0..(SCRAPES * 2) {
        let response = match client.get("/ready") {
            Ok(response) => response,
            Err(error) => panic!("{error}"),
        };
        if response.status == 404 {
            println!("SKIP: this deployment predates /ready");
            return;
        }
        assert!(
            response.status == 200 || response.status == 503,
            "/ready answered {} on attempt {attempt}: {}",
            response.status,
            response.text()
        );

        let body: serde_json::Value = match response.json("/ready") {
            Ok(body) => body,
            Err(error) => panic!("{error}"),
        };
        let dependencies = body
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
            .unwrap_or_else(|| panic!("/ready must name its dependencies: {body}"));
        let all_up = dependencies.iter().all(|dependency| {
            dependency.get("status").and_then(serde_json::Value::as_str) == Some("up")
        });

        let status = body
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("/ready must report an aggregate: {body}"));

        let expected = if all_up {
            ("ready", 200)
        } else {
            ("not_ready", 503)
        };
        assert_eq!(
            (status, response.status),
            expected,
            "attempt {attempt}: the aggregate must follow from the dependencies of the instance \
             that answered: {body}"
        );

        if all_up {
            ready_answers += 1;
        } else {
            not_ready_answers += 1;
        }
    }

    if not_ready_answers > 0 {
        println!(
            "INFO: {not_ready_answers} of {} readiness answers reported a dependency down; a \
             replicated deployment reports per instance, so this is one replica's view rather \
             than the deployment's",
            ready_answers + not_ready_answers
        );
    }
}
