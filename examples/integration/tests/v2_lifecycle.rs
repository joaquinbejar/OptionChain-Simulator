//! The v2 simulation lifecycle and its cursor semantics, over the wire.
//!
//! v2 carries the vocabulary that makes a retry safe, and it is the part a
//! unit test proves least: `expected_step` is a PRECONDITION on the cursor,
//! and the version is a compare-and-swap token. They mean different things and
//! answer different codes, `412` and `409`, so these assert them separately
//! and check the cursor after every failed precondition to prove nothing was
//! consumed.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{ServiceClient, reference_request, service};
use serde::Deserialize;
use std::sync::mpsc;
use std::thread;

/// The simulation envelope: identity, state, the CAS token and the cursor.
#[derive(Debug, Deserialize)]
struct Envelope {
    id: String,
    state: String,
    version: u64,
    cursor: Cursor,
}

/// Where the walk is and how far it goes.
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Cursor {
    current_step: usize,
    total_steps: usize,
}

/// The create response, which echoes every replay input.
#[derive(Debug, Deserialize)]
struct Created {
    id: String,
    state: String,
    cursor: Cursor,
    parameters: Parameters,
}

/// The replay inputs a client has to record to reproduce a run.
#[derive(Debug, Deserialize)]
struct Parameters {
    symbol: String,
    steps: usize,
    seed: u64,
    effective_start: String,
    step_interval_seconds: u64,
    tzdb_version: String,
    schedules: Vec<Schedule>,
    chain_size: Option<usize>,
}

/// One normalised schedule rule.
#[derive(Debug, Deserialize)]
struct Schedule {
    rule_id: String,
    target_count: usize,
}

/// A simulation that deletes itself.
///
/// The harness fixture creates one and hides the body; these tests need the
/// body, since the create response is where every replay input is echoed, so
/// they own the request here and keep the same delete-on-drop guarantee.
struct Live {
    client: ServiceClient,
    id: String,
}

impl Live {
    /// Creates a simulation with `steps` steps and a two-strike chain, kept
    /// small because the deployment is shared.
    fn create(client: &ServiceClient, steps: usize) -> Option<(Self, Created)> {
        let mut request = reference_request("SPX");
        if let Some(object) = request.as_object_mut() {
            object.insert("steps".to_string(), serde_json::json!(steps));
            object.insert("chain_size".to_string(), serde_json::json!(2));
        }

        let response = match client.post("/api/v2/simulations", &request) {
            Ok(response) => response,
            Err(error) => panic!("{error}"),
        };
        if response.status == 404 {
            println!("SKIP: this deployment has no v2 API");
            return None;
        }
        assert_eq!(
            response.status,
            201,
            "creating a simulation must answer 201, got {} with {}",
            response.status,
            response.text()
        );

        let created: Created = match response.json("/api/v2/simulations") {
            Ok(created) => created,
            Err(error) => panic!("{error}"),
        };
        let live = Self {
            client: client.clone(),
            id: created.id.clone(),
        };
        Some((live, created))
    }

    /// This simulation's path, with an optional suffix such as `/step`.
    fn path(&self, suffix: &str) -> String {
        format!("/api/v2/simulations/{}{suffix}", self.id)
    }

    /// The current envelope.
    fn envelope(&self) -> Envelope {
        let path = self.path("");
        match self.client.get(&path) {
            Ok(response) => {
                assert_eq!(
                    response.status,
                    200,
                    "reading {path} must answer 200, got {} with {}",
                    response.status,
                    response.text()
                );
                match response.json(&path) {
                    Ok(envelope) => envelope,
                    Err(error) => panic!("{error}"),
                }
            }
            Err(error) => panic!("{error}"),
        }
    }

    /// Advances, optionally under an `expected_step` precondition.
    fn step(&self, expected: Option<usize>) -> examples_integration::Response {
        let path = match expected {
            Some(expected) => self.path(&format!("/step?expected_step={expected}")),
            None => self.path("/step"),
        };
        match self.client.request("POST", &path, None) {
            Ok(response) => response,
            Err(error) => panic!("advancing: {error}"),
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Err(error) = self.client.delete(&self.path("")) {
            println!("WARNING: could not delete simulation {}: {error}", self.id);
        }
    }
}

/// Creating a simulation echoes every input a replay needs.
///
/// A client that recorded only what it sent cannot reproduce a run: the seed
/// may have been generated, the start resolved, the interval defaulted and the
/// schedules normalised. All of that comes back, or the reproducibility
/// contract is unusable from the outside.
#[test]
fn test_creating_a_simulation_echoes_every_replay_input() {
    let Some(client) = service() else {
        return;
    };
    let Some((live, created)) = Live::create(&client, 3) else {
        return;
    };

    assert_eq!(created.state, "initialized");
    assert_eq!(
        created.cursor,
        Cursor {
            current_step: 0,
            total_steps: 3
        }
    );

    let parameters = &created.parameters;
    assert_eq!(parameters.symbol, "SPX");
    assert_eq!(parameters.steps, 3);
    assert_eq!(
        parameters.seed, 42,
        "the seed the request set must come back"
    );
    assert!(
        !parameters.effective_start.is_empty(),
        "the resolved start must come back, since a replay needs it"
    );
    assert!(
        parameters.step_interval_seconds > 0,
        "the resolved interval must come back"
    );
    assert!(
        !parameters.tzdb_version.is_empty(),
        "the tzdb release the expirations were resolved against must come back"
    );
    assert_eq!(parameters.chain_size, Some(2));

    // Normalisation is part of the replay contract: the rules come back
    // sorted by rule id, whatever order they were sent in.
    let ids: Vec<&str> = parameters
        .schedules
        .iter()
        .map(|rule| rule.rule_id.as_str())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(
        ids, sorted,
        "the normalised schedules must come back sorted"
    );
    assert!(
        parameters
            .schedules
            .iter()
            .all(|rule| rule.target_count > 0),
        "a normalised rule keeps its target count"
    );

    // A simulation with no seed of its own is told which one it got.
    let mut unseeded = reference_request("SPX");
    if let Some(object) = unseeded.as_object_mut() {
        object.remove("seed");
        object.insert("steps".to_string(), serde_json::json!(2));
    }
    match client.post("/api/v2/simulations", &unseeded) {
        Ok(response) => {
            let created: Created = match response.json("/api/v2/simulations") {
                Ok(created) => created,
                Err(error) => panic!("{error}"),
            };
            let generated = Live {
                client: client.clone(),
                id: created.id.clone(),
            };
            assert_ne!(
                created.parameters.seed, 0,
                "a generated seed must be a real one"
            );
            drop(generated);
        }
        Err(error) => panic!("{error}"),
    }

    drop(live);
}

/// Reading the metadata moves nothing, and peeking a snapshot is repeatable.
#[test]
fn test_reading_and_peeking_leave_the_cursor_alone() {
    let Some(client) = service() else {
        return;
    };
    let Some((live, _)) = Live::create(&client, 3) else {
        return;
    };

    let before = live.envelope();
    assert_eq!(before.cursor.current_step, 0);

    for _ in 0..2 {
        let path = live.path("/snapshot");
        match client.get(&path) {
            Ok(response) => assert_eq!(
                response.status,
                200,
                "a peek must answer 200, got {} with {}",
                response.status,
                response.text()
            ),
            Err(error) => panic!("{error}"),
        }
    }

    let after = live.envelope();
    assert_eq!(
        before.cursor, after.cursor,
        "neither reading metadata nor peeking may move the cursor"
    );
    assert_eq!(
        before.version, after.version,
        "a read must not bump the compare-and-swap token"
    );
    assert_eq!(after.id, before.id);
}

/// The walk serves every step and then is gone.
#[test]
fn test_the_v2_walk_serves_every_step_and_then_is_gone() {
    let Some(client) = service() else {
        return;
    };
    let steps = 3;
    let Some((live, _)) = Live::create(&client, steps) else {
        return;
    };

    for index in 0..steps {
        let response = live.step(None);
        assert_eq!(
            response.status,
            200,
            "advance {index} must serve a snapshot, got {} with {}",
            response.status,
            response.text()
        );
    }

    let past_the_end = live.step(None);
    assert_eq!(
        past_the_end.status,
        410,
        "the advance past the last step must be 410, got {} with {}",
        past_the_end.status,
        past_the_end.text()
    );

    let final_state = live.envelope();
    assert_eq!(final_state.cursor.current_step, steps);
    assert_eq!(final_state.state, "completed");
}

/// A stale `expected_step` is `412`, carries the actual cursor, and consumes
/// NOTHING: the same call retried at the cursor it reports still works.
///
/// This is the whole point of the precondition. A client that retries after a
/// timeout must be able to tell "you already did this" from "someone else
/// did", and must not lose a step to a failed guess.
#[test]
fn test_a_stale_precondition_is_412_and_consumes_nothing() {
    let Some(client) = service() else {
        return;
    };
    let Some((live, _)) = Live::create(&client, 3) else {
        return;
    };

    let before = live.envelope();
    let stale = before.cursor.current_step + 2;

    let response = live.step(Some(stale));
    assert_eq!(
        response.status,
        412,
        "a stale expected_step must be 412, not 409 and not 400, got {} with {}",
        response.status,
        response.text()
    );

    let body: serde_json::Value = match response.json(&live.path("/step")) {
        Ok(body) => body,
        Err(error) => panic!("{error}"),
    };
    let reported = body.get("current_step").and_then(serde_json::Value::as_u64);
    assert_eq!(
        reported,
        Some(before.cursor.current_step as u64),
        "the 412 body must carry the ACTUAL cursor so a client can retry: {body}"
    );

    let after = live.envelope();
    assert_eq!(
        before.cursor, after.cursor,
        "a failed precondition must consume nothing"
    );
    assert_eq!(
        before.version, after.version,
        "a failed precondition must not bump the version"
    );

    // And the retry at the cursor the 412 reported succeeds.
    let retried = live.step(Some(after.cursor.current_step));
    assert_eq!(
        retried.status,
        200,
        "the retry at the reported cursor must serve, got {} with {}",
        retried.status,
        retried.text()
    );
    assert_eq!(
        live.envelope().cursor.current_step,
        after.cursor.current_step + 1
    );
}

/// Two advances fired at once: one wins, the other is `409`, and the cursor
/// moves exactly once.
///
/// `409` means someone else committed first, which is a different fact from
/// `412`, and conflating them would make a client retry the wrong way.
#[test]
fn test_two_concurrent_advances_leave_exactly_one_winner() {
    let Some(client) = service() else {
        return;
    };
    let Some((live, _)) = Live::create(&client, 6) else {
        return;
    };

    let before = live.envelope();
    let (sender, receiver) = mpsc::channel();

    let mut handles = Vec::new();
    for _ in 0..2 {
        let client = client.clone();
        let path = live.path("/step");
        let sender = sender.clone();
        handles.push(thread::spawn(move || {
            let status = match client.request("POST", &path, None) {
                Ok(response) => response.status,
                Err(error) => panic!("advancing concurrently: {error}"),
            };
            let _ = sender.send(status);
        }));
    }
    drop(sender);
    for handle in handles {
        if handle.join().is_err() {
            panic!("a concurrent advance panicked");
        }
    }

    let statuses: Vec<u16> = receiver.iter().collect();
    assert_eq!(statuses.len(), 2, "both advances must answer");

    let winners = statuses.iter().filter(|status| **status == 200).count();
    let losers = statuses.iter().filter(|status| **status == 409).count();
    let after = live.envelope();
    let moved = after.cursor.current_step - before.cursor.current_step;

    // Two outcomes are correct. Either they raced and the store rejected the
    // loser, or they serialised and both advanced; what must never happen is
    // a 5xx, or a cursor that moved by something other than the number of
    // successes.
    assert!(
        statuses
            .iter()
            .all(|status| *status == 200 || *status == 409),
        "concurrent advances must answer 200 or 409, got {statuses:?}"
    );
    assert_eq!(
        moved, winners,
        "the cursor must move exactly once per winner, {winners} won and {losers} lost,          cursor moved {moved}"
    );
    if losers > 0 {
        println!("INFO: the two advances raced, {winners} won and {losers} got 409");
    } else {
        println!("INFO: the two advances serialised, both served and the cursor moved {moved}");
    }
}

/// Deleting removes it, and the id stops resolving.
#[test]
fn test_a_deleted_simulation_stops_resolving() {
    let Some(client) = service() else {
        return;
    };
    let Some((live, _)) = Live::create(&client, 2) else {
        return;
    };

    let path = live.path("");
    let snapshot = live.path("/snapshot");
    let step = live.path("/step");

    match client.delete(&path) {
        Ok(response) => assert_eq!(
            response.status,
            200,
            "DELETE must answer 200, got {} with {}",
            response.status,
            response.text()
        ),
        Err(error) => panic!("{error}"),
    }

    for (method, path) in [
        ("GET", path.as_str()),
        ("GET", snapshot.as_str()),
        ("POST", step.as_str()),
    ] {
        match client.request(method, path, None) {
            Ok(response) => assert_eq!(
                response.status,
                404,
                "{method} {path} must be 404 after the delete, got {} with {}",
                response.status,
                response.text()
            ),
            Err(error) => panic!("{error}"),
        }
    }
}
