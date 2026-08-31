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
use std::sync::{Arc, Barrier};
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

/// The value at `key`, or a failure naming what was missing.
fn field<'a>(parameters: &'a Parameters, key: &str) -> &'a serde_json::Value {
    parameters
        .get(key)
        .unwrap_or_else(|| panic!("the echoed parameters must carry {key}: {parameters:?}"))
}

/// The replay inputs a client has to record to reproduce a run.
///
/// Deserialised as a raw object rather than a struct on purpose: a struct
/// silently ignores every field it does not declare, so a subset of it stays
/// green while `timezone`, `calendar` or the whole spread model disappears
/// from the response. The assertions below name every key the contract
/// promises, so a missing one fails.
type Parameters = serde_json::Map<String, serde_json::Value>;

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
        Self::create_with(client, steps, &reference_schedules())
    }

    /// Creates a simulation with an explicit schedule list.
    fn create_with(
        client: &ServiceClient,
        steps: usize,
        schedules: &serde_json::Value,
    ) -> Option<(Self, Created)> {
        let mut request = reference_request("SPX");
        if let Some(object) = request.as_object_mut() {
            object.insert("steps".to_string(), serde_json::json!(steps));
            object.insert("chain_size".to_string(), serde_json::json!(2));
            object.insert("schedules".to_string(), schedules.clone());
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
        // `delete` answers Ok for a 401, a 405 or a 500, and counting those as
        // cleanup leaks simulations onto a shared deployment.
        examples_integration::report_cleanup(&self.client, &self.path(""), &self.id);
    }
}

/// Two rules, deliberately sent out of `rule_id` order and of different kinds,
/// so the normalisation this contract promises has something to do.
fn reference_schedules() -> serde_json::Value {
    serde_json::json!([
        {"rule_id": "zz_weeklies", "kind": "weekly", "target_count": 2, "weekdays": ["Mon", "Fri"]},
        {"rule_id": "aa_dailies", "kind": "daily", "target_count": 1}
    ])
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

    // Every key the replay contract promises, asserted by name and by value.
    // A struct with a subset of these would ignore a field that vanished, so
    // each one is looked up explicitly and a missing key fails.
    assert_eq!(field(parameters, "symbol"), "SPX");
    assert_eq!(field(parameters, "steps"), 3);
    assert_eq!(field(parameters, "seed"), 42);
    assert_eq!(field(parameters, "timezone"), "America/New_York");
    assert_eq!(field(parameters, "expiration_time"), "17:00:00");
    assert_eq!(field(parameters, "time_frame"), "day");
    assert_eq!(field(parameters, "initial_price"), 5000.0);
    assert_eq!(field(parameters, "volatility"), 0.2);
    assert_eq!(field(parameters, "risk_free_rate"), 0.05);
    assert_eq!(field(parameters, "dividend_yield"), 0.0);
    assert_eq!(field(parameters, "chain_size"), 2);
    assert_eq!(field(parameters, "strike_interval"), 25.0);

    // Resolved rather than sent, so the assertion is on shape: a replay needs
    // them and cannot invent them.
    assert!(
        field(parameters, "effective_start")
            .as_str()
            .is_some_and(|start| start.ends_with('Z') && start.len() >= 20),
        "the resolved start must come back as an instant: {parameters:?}"
    );
    assert!(
        field(parameters, "step_interval_seconds")
            .as_u64()
            .is_some_and(|interval| interval > 0),
        "the resolved interval must come back"
    );
    assert!(
        field(parameters, "tzdb_version")
            .as_str()
            .is_some_and(|version| !version.is_empty()),
        "the tzdb release the expirations were resolved against must come back"
    );
    assert!(
        field(parameters, "calendar")
            .as_str()
            .is_some_and(|calendar| !calendar.is_empty()),
        "the calendar must come back"
    );
    assert!(
        field(parameters, "method")
            .get("GeometricBrownian")
            .is_some(),
        "the walk model must come back as sent: {:?}",
        field(parameters, "method")
    );

    // Normalisation is part of the replay contract, and the request sent its
    // two rules deliberately out of order, of different kinds, so sorting has
    // something to do. The whole normalised list is compared, weekdays
    // included: a rule that lost its weekdays would replay differently.
    assert_eq!(
        field(parameters, "schedules"),
        &serde_json::json!([
            {"rule_id": "aa_dailies", "kind": "daily", "target_count": 1},
            {"rule_id": "zz_weeklies", "kind": "weekly", "target_count": 2,
             "weekdays": ["Mon", "Fri"]}
        ]),
        "the normalised schedules must come back sorted by rule_id and complete"
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
            assert!(
                field(&created.parameters, "seed")
                    .as_u64()
                    .is_some_and(|seed| seed != 0),
                "a generated seed must be a real one: {:?}",
                created.parameters.get("seed")
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

    // The bodies matter: stable metadata proves the cursor did not move, and
    // says nothing about whether the same market came back. A service can
    // reprice on every GET without touching the cursor, and that is exactly
    // what a repeatable peek must not do.
    let peek = || -> serde_json::Value {
        let path = live.path("/snapshot");
        match client.get(&path) {
            Ok(response) => {
                assert_eq!(
                    response.status,
                    200,
                    "a peek must answer 200, got {} with {}",
                    response.status,
                    response.text()
                );
                match response.json::<serde_json::Value>(&path) {
                    Ok(body) => body,
                    Err(error) => panic!("{error}"),
                }
            }
            Err(error) => panic!("{error}"),
        }
    };
    let first = peek();
    let second = peek();
    for key in ["simulated_at", "underlying", "chains"] {
        assert_eq!(
            first.get(key),
            second.get(key),
            "peeking twice must return the same {key}"
        );
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

    // And the advance serves the market the peek showed, which is what makes
    // a peek a preview rather than a different sample.
    let served = live.step(None);
    assert_eq!(served.status, 200, "{}", served.text());
    let served: serde_json::Value = match served.json(&live.path("/step")) {
        Ok(body) => body,
        Err(error) => panic!("{error}"),
    };
    for key in ["simulated_at", "underlying", "chains"] {
        assert_eq!(
            served.get(key),
            first.get(key),
            "the advance must serve the {key} the peek showed"
        );
    }
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

/// Two advances fired at once: exactly one wins, the other is `409`, the
/// cursor moves by one and the version by one.
///
/// `409` means someone else committed first, which is a different fact from
/// `412`, and conflating them would make a client retry the wrong way. The two
/// requests are released by a barrier so they genuinely overlap rather than
/// happening to; a deployment that serialised them would move the cursor twice
/// and is retried, because the contract this asserts is about contention and a
/// run that never contended has not exercised it.
#[test]
fn test_two_concurrent_advances_leave_exactly_one_winner() {
    let Some(client) = service() else {
        return;
    };
    let Some((live, _)) = Live::create(&client, 8) else {
        return;
    };

    let mut contended = false;
    for attempt in 0..5 {
        let before = live.envelope();
        let barrier = Arc::new(Barrier::new(2));
        let (sender, receiver) = mpsc::channel();

        let mut handles = Vec::new();
        for _ in 0..2 {
            let client = client.clone();
            let path = live.path("/step");
            let sender = sender.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                // Both threads are ready before either sends, so the requests
                // are in flight together rather than one after the other.
                barrier.wait();
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
        assert!(
            statuses
                .iter()
                .all(|status| *status == 200 || *status == 409),
            "a contended advance answers 200 or 409, never anything else, got {statuses:?}"
        );

        let winners = statuses.iter().filter(|status| **status == 200).count();
        let losers = statuses.iter().filter(|status| **status == 409).count();
        let after = live.envelope();

        if losers == 0 {
            // Serialised rather than raced. The cursor arithmetic must still
            // hold, and the attempt is repeated so the contract under test is
            // actually exercised.
            assert_eq!(
                after.cursor.current_step - before.cursor.current_step,
                winners,
                "attempt {attempt} serialised, so the cursor must move once per winner"
            );
            println!("INFO: attempt {attempt} serialised, retrying for a real race");
            continue;
        }

        assert_eq!(winners, 1, "exactly one advance may win, got {statuses:?}");
        assert_eq!(
            losers, 1,
            "exactly one advance may conflict, got {statuses:?}"
        );
        assert_eq!(
            after.cursor.current_step - before.cursor.current_step,
            1,
            "a contended step is consumed exactly once"
        );
        assert_eq!(
            after.version - before.version,
            1,
            "one commit means one version, {} became {}",
            before.version,
            after.version
        );
        contended = true;
        break;
    }

    assert!(
        contended,
        "five barrier-synchronised attempts never contended, so the 409 path was not \
         exercised; either the deployment serialises every advance or the race is not being \
         created"
    );
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
