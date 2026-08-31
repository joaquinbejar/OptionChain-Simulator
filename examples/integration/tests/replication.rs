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
    ServiceClient, instance_clients, instances_behind, reference_request, report_cleanup,
    responding_instance, service,
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

    match instances_behind(&client, SCRAPES) {
        Some(1) => println!(
            "INFO: one instance answered every probe; the replicated paths below still run, \
             trivially"
        ),
        Some(instances) => println!("INFO: {instances} distinct instances answered"),
        None => println!(
            "INFO: this deployment reports no instance identity, so answers cannot be \
             attributed; the tests that need attribution say so themselves"
        ),
    }

    // Whether the replicas can be reached individually decides what the rest
    // of this file can prove, so it is reported here too.
    let direct = instance_clients();
    if direct.is_empty() {
        println!(
            "INFO: {} is unset, so the tests needing a known replica will skip",
            examples_integration::INSTANCE_URLS_VARIABLE
        );
    } else {
        println!("INFO: {} replicas can be reached directly", direct.len());
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
/// The baseline is walked against ONE named replica, bypassing the balancer,
/// and the subject through the balanced address. Two simulations walked
/// through the same balanced client would prove less than it looks: a
/// replica-specific defect affects both equally, and both can land on the same
/// replica anyway.
///
/// Skipped, loudly, when the replicas cannot be reached individually: a
/// single-instance baseline cannot be established through a balancer.
#[test]
fn test_a_tape_is_the_same_whichever_instance_serves_it() {
    let Some(client) = service() else {
        return;
    };
    let direct = instance_clients();
    let Some(baseline_client) = direct.first() else {
        println!(
            "SKIP: {} is unset, so there is no single-instance baseline to compare against",
            examples_integration::INSTANCE_URLS_VARIABLE
        );
        return;
    };

    let (Some(baseline), Some(balanced)) = (
        Live::create(baseline_client, STEPS, 99),
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

    let serve = |with: &ServiceClient, live: &Live| -> (serde_json::Value, Option<String>) {
        let path = live.path("/step");
        match with.request("POST", &path, None) {
            Ok(response) => {
                assert_eq!(
                    response.status,
                    200,
                    "a step must serve: {}",
                    response.text()
                );
                let instance = responding_instance(&response);
                match response.json::<serde_json::Value>(&path) {
                    Ok(body) => (market(&body), instance),
                    Err(error) => panic!("{error}"),
                }
            }
            Err(error) => panic!("{error}"),
        }
    };

    let mut baseline_instances = std::collections::BTreeSet::new();
    let mut balanced_instances = std::collections::BTreeSet::new();

    for step in 0..STEPS {
        let (from_one, one_instance) = serve(baseline_client, &baseline);
        let (through_balancer, balanced_instance) = serve(&client, &balanced);

        if let Some(instance) = one_instance {
            baseline_instances.insert(instance);
        }
        if let Some(instance) = balanced_instance {
            balanced_instances.insert(instance);
        }

        assert_eq!(
            from_one, through_balancer,
            "step {step} differs between a tape walked against one replica and the same seed \
             walked through the balancer"
        );
    }

    assert!(
        baseline_instances.len() <= 1,
        "the baseline must come from ONE instance, it came from {}",
        baseline_instances.len()
    );
    println!(
        "INFO: baseline from {} instance, balanced walk served by {} instance(s)",
        baseline_instances.len().max(1),
        balanced_instances.len().max(1)
    );
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

/// Each replica answers readiness for ITSELF.
///
/// Asked of every replica directly, which is the only way to see that a
/// reply is that instance's own view: through a balancer, a replica whose
/// Redis is gone is indistinguishable from one whose Redis is fine, because
/// the next request may go to either.
///
/// What this cannot do is BREAK a dependency on one replica; that needs an
/// operator-level fixture and is issue #145. What it proves is that each
/// replica answers for itself: a distinct identity per address, and an
/// aggregate that follows from the dependencies that same replica reported.
#[test]
fn test_each_replica_answers_readiness_for_itself() {
    let Some(client) = service() else {
        return;
    };

    let direct = instance_clients();
    let targets: Vec<&ServiceClient> = if direct.is_empty() {
        println!(
            "SKIP: {} is unset, so readiness is checked through the balancer only",
            examples_integration::INSTANCE_URLS_VARIABLE
        );
        vec![&client]
    } else {
        direct.iter().collect()
    };

    let mut identities = std::collections::BTreeSet::new();

    for target in &targets {
        let response = match target.get("/ready") {
            Ok(response) => response,
            Err(error) => panic!("{}: {error}", target.base_url()),
        };
        if response.status == 404 {
            println!("SKIP: {} predates /ready", target.base_url());
            return;
        }
        assert!(
            response.status == 200 || response.status == 503,
            "{} answered {} for /ready: {}",
            target.base_url(),
            response.status,
            response.text()
        );

        if let Some(instance) = responding_instance(&response) {
            identities.insert(instance);
        }

        let body: serde_json::Value = match response.json("/ready") {
            Ok(body) => body,
            Err(error) => panic!("{error}"),
        };
        let dependencies = body
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
            .unwrap_or_else(|| panic!("{} must name its dependencies: {body}", target.base_url()));
        assert!(
            !dependencies.is_empty(),
            "a readiness answer names what it probed"
        );

        let all_up = dependencies.iter().all(|dependency| {
            dependency.get("status").and_then(serde_json::Value::as_str) == Some("up")
        });
        let status = body
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("{} must report an aggregate: {body}", target.base_url()));
        let expected = if all_up {
            ("ready", 200)
        } else {
            ("not_ready", 503)
        };
        assert_eq!(
            (status, response.status),
            expected,
            "{}: the aggregate must follow from the dependencies THIS replica probed: {body}",
            target.base_url()
        );
    }

    if direct.len() > 1 {
        assert_eq!(
            identities.len(),
            direct.len(),
            "{} addresses answered with {} distinct instances, so they are not separate replicas",
            direct.len(),
            identities.len()
        );
        println!(
            "INFO: {} replicas each answered readiness for themselves",
            direct.len()
        );
    }
}
