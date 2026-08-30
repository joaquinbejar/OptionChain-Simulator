//! Behaviour under concurrent clients, and the streaming path.
//!
//! The service is meant to be read by more than one client at a time and to
//! STREAM an export rather than materialise it. Neither property exists in a
//! single-threaded test, so nothing else in this suite touches them.
//!
//! Everything here is bounded: a handful of clients, a short horizon, small
//! chains. It runs against a shared deployment.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{ServiceClient, reference_request, service};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Steps in the simulations these tests walk.
const STEPS: usize = 6;

/// How many clients contend.
const CLIENTS: usize = 3;

/// A simulation that deletes itself.
struct Live {
    client: ServiceClient,
    id: String,
}

impl Live {
    /// Creates a simulation with `steps` steps and a narrow chain.
    fn create(client: &ServiceClient, steps: usize, chain_size: usize) -> Option<Self> {
        let mut request = reference_request("SPX");
        if let Some(object) = request.as_object_mut() {
            object.insert("steps".to_string(), serde_json::json!(steps));
            object.insert("chain_size".to_string(), serde_json::json!(chain_size));
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

    /// The cursor right now.
    fn cursor(&self) -> usize {
        let path = self.path("");
        match self.client.get(&path) {
            Ok(response) => {
                assert_eq!(response.status, 200, "{}", response.text());
                let body: serde_json::Value = match response.json(&path) {
                    Ok(body) => body,
                    Err(error) => panic!("{error}"),
                };
                match body
                    .get("cursor")
                    .and_then(|cursor| cursor.get("current_step"))
                    .and_then(serde_json::Value::as_u64)
                {
                    Some(step) => step as usize,
                    None => panic!("the envelope must carry a cursor: {body}"),
                }
            }
            Err(error) => panic!("{error}"),
        }
    }

    /// Walks it to exhaustion, so it has a tape to export.
    fn walk(&self) {
        for step in 0..STEPS {
            match self.client.request("POST", &self.path("/step"), None) {
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
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Err(error) = self.client.delete(&self.path("")) {
            println!("WARNING: could not delete simulation {}: {error}", self.id);
        }
    }
}

/// Two clients exporting the same simulation at once receive identical bytes,
/// and neither disturbs the other's cursor.
#[test]
fn test_concurrent_exports_of_one_simulation_are_identical() {
    let Some(client) = service() else {
        return;
    };
    let Some(live) = Live::create(&client, STEPS, 2) else {
        return;
    };
    live.walk();

    let before = live.cursor();
    let query = live.path("/export?dataset=option_chains&format=csv");
    let (sender, receiver) = mpsc::channel();

    let mut handles = Vec::new();
    for _ in 0..CLIENTS {
        let client = client.clone();
        let query = query.clone();
        let sender = sender.clone();
        handles.push(thread::spawn(move || {
            let body = match client.get(&query) {
                Ok(response) => {
                    assert_eq!(response.status, 200, "a concurrent export must serve");
                    response.body
                }
                Err(error) => panic!("exporting concurrently: {error}"),
            };
            let _ = sender.send(body);
        }));
    }
    drop(sender);
    for handle in handles {
        if handle.join().is_err() {
            panic!("a concurrent export panicked");
        }
    }

    let bodies: Vec<Vec<u8>> = receiver.iter().collect();
    assert_eq!(bodies.len(), CLIENTS, "every client must get an answer");
    for (index, body) in bodies.iter().enumerate() {
        assert!(!body.is_empty(), "client {index} received an empty export");
        assert_eq!(
            body, &bodies[0],
            "client {index} received a different export of the same simulation"
        );
    }

    assert_eq!(
        live.cursor(),
        before,
        "exporting must not move the cursor, however many clients do it at once"
    );
}

/// Interleaved advances of two simulations stay independent.
#[test]
fn test_two_simulations_advance_independently() {
    let Some(client) = service() else {
        return;
    };
    let (Some(left), Some(right)) = (
        Live::create(&client, STEPS, 2),
        Live::create(&client, STEPS, 2),
    ) else {
        return;
    };

    // Interleave: left, right, left, right...
    for round in 0..STEPS {
        for (name, live) in [("left", &left), ("right", &right)] {
            match client.request("POST", &live.path("/step"), None) {
                Ok(response) => assert_eq!(
                    response.status,
                    200,
                    "round {round} of {name} must serve: {}",
                    response.text()
                ),
                Err(error) => panic!("{error}"),
            }
        }
        assert_eq!(
            left.cursor(),
            right.cursor(),
            "round {round}: interleaved simulations must advance one step each"
        );
    }

    assert_eq!(left.cursor(), STEPS);
    assert_eq!(right.cursor(), STEPS);
}

/// Several advances fired at once produce exactly one winner per step, and the
/// cursor ends where the arithmetic says.
#[test]
fn test_contended_advances_have_exactly_one_winner_per_step() {
    let Some(client) = service() else {
        return;
    };
    let Some(live) = Live::create(&client, STEPS, 2) else {
        return;
    };

    let before = live.cursor();
    let (sender, receiver) = mpsc::channel();

    let mut handles = Vec::new();
    for _ in 0..CLIENTS {
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
    assert_eq!(statuses.len(), CLIENTS);
    assert!(
        statuses
            .iter()
            .all(|status| *status == 200 || *status == 409),
        "a contended advance answers 200 or 409, never anything else, got {statuses:?}"
    );

    let winners = statuses.iter().filter(|status| **status == 200).count();
    assert!(
        winners >= 1,
        "at least one advance must win, got {statuses:?}"
    );

    let moved = live.cursor() - before;
    assert_eq!(
        moved, winners,
        "the cursor moved {moved} for {winners} winners out of {statuses:?}"
    );
}

/// A peek during another client's advance never returns a half-built chain.
#[test]
fn test_a_peek_during_an_advance_is_never_half_built() {
    let Some(client) = service() else {
        return;
    };
    let Some(live) = Live::create(&client, STEPS, 2) else {
        return;
    };

    let advancing = {
        let client = client.clone();
        let path = live.path("/step");
        thread::spawn(move || {
            for _ in 0..STEPS {
                let _ = client.request("POST", &path, None);
            }
        })
    };

    let mut peeks = 0_usize;
    let snapshot_path = live.path("/snapshot");
    for _ in 0..STEPS * 2 {
        let response = match client.get(&snapshot_path) {
            Ok(response) => response,
            Err(error) => panic!("{error}"),
        };
        // 410 once the walk has finished is correct, not a failure.
        if response.status == 410 {
            continue;
        }
        assert_eq!(
            response.status,
            200,
            "a peek during an advance answered {}: {}",
            response.status,
            response.text()
        );

        let body: serde_json::Value = match response.json(&snapshot_path) {
            Ok(body) => body,
            Err(error) => panic!("a peek during an advance must be complete JSON: {error}"),
        };
        let chains = body
            .get("chains")
            .and_then(serde_json::Value::as_array)
            .unwrap_or_else(|| panic!("a snapshot must carry chains: {body}"));
        assert!(!chains.is_empty(), "a snapshot must quote an expiration");
        for chain in chains {
            let contracts = chain
                .get("contracts")
                .and_then(serde_json::Value::as_array)
                .unwrap_or_else(|| panic!("a chain must carry contracts: {chain}"));
            assert!(
                !contracts.is_empty(),
                "a chain served mid-advance must be complete, not empty: {chain}"
            );
        }
        peeks += 1;
    }

    if advancing.join().is_err() {
        panic!("the advancing client panicked");
    }
    println!("INFO: {peeks} peeks during a concurrent walk, all complete");
}

/// The export streams, and a client may walk away from it mid-flight without
/// the service faulting.
#[test]
fn test_an_abandoned_export_leaves_the_service_serving() {
    let Some(client) = service() else {
        return;
    };
    // A wider chain and the full greek set, so the export is big enough that
    // abandoning it lands mid-stream rather than after the last byte.
    let Some(live) = Live::create(&client, STEPS, 12) else {
        return;
    };
    live.walk();

    let address = client.base_url().trim_start_matches("http://").to_string();
    let address = if address.contains(':') {
        address
    } else {
        format!("{address}:80")
    };
    let path = live.path("/export?dataset=option_chains&format=csv&greeks=all");

    let started = Instant::now();
    let (first_byte, taken) = {
        let mut socket = match TcpStream::connect(&address) {
            Ok(socket) => socket,
            Err(error) => panic!("connecting to {address}: {error}"),
        };
        if let Err(error) = socket.set_read_timeout(Some(Duration::from_secs(30))) {
            panic!("{error}");
        }
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
            address
        );
        if let Err(error) = socket.write_all(request.as_bytes()) {
            panic!("{error}");
        }

        // Read a little and then walk away, which is what a client that
        // cancels a download does.
        let mut buffer = [0_u8; 1024];
        let read = match socket.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => panic!("reading the start of the export: {error}"),
        };
        (started.elapsed(), read)
        // The socket drops here, mid-stream.
    };

    assert!(taken > 0, "the export delivered nothing at all");
    println!(
        "INFO: the export delivered its first {taken} bytes after {:?}, then the client walked \
         away mid-stream",
        first_byte
    );

    // What matters is what happens next: the service still serves.
    match client.get("/api/v1/chain?sessionid=00000000-0000-4000-8000-000000000000") {
        Ok(response) => assert!(
            response.status < 500,
            "the service answered {} after a client abandoned an export: {}",
            response.status,
            response.text()
        ),
        Err(error) => panic!("the service stopped serving after an abandoned export: {error}"),
    }

    // And the same export, read to the end this time, is complete.
    let whole = match client.get(&path) {
        Ok(response) => response,
        Err(error) => panic!("{error}"),
    };
    assert_eq!(whole.status, 200);
    let text = whole.text();
    assert!(
        text.ends_with("\r\n"),
        "the export read to the end must be complete"
    );
    println!(
        "INFO: the whole export is {} bytes over {STEPS} steps of a 25-strike chain, read in {:?}",
        whole.body.len(),
        started.elapsed()
    );
}

/// An export in flight when the simulation is deleted either completes or
/// fails cleanly, and the service keeps serving either way.
#[test]
fn test_deleting_a_simulation_under_an_export_is_clean() {
    let Some(client) = service() else {
        return;
    };
    let Some(live) = Live::create(&client, STEPS, 8) else {
        return;
    };
    live.walk();

    let path = live.path("/export?dataset=option_chains&format=csv&greeks=all");
    let delete_path = live.path("");

    let exporting = {
        let client = client.clone();
        let path = path.clone();
        thread::spawn(move || client.get(&path).map(|response| response.status))
    };

    // Delete while that export is in flight.
    let deleted = match client.delete(&delete_path) {
        Ok(response) => response.status,
        Err(error) => panic!("{error}"),
    };
    assert!(
        deleted == 200 || deleted == 404,
        "deleting under an export answered {deleted}"
    );

    match exporting.join() {
        Ok(Ok(status)) => assert!(
            status == 200 || status == 404 || status == 410,
            "an export racing a delete must complete or fail cleanly, got {status}"
        ),
        Ok(Err(error)) => panic!("the export failed at the transport level: {error}"),
        Err(_) => panic!("the exporting client panicked"),
    }

    // The service is still there.
    match client.get("/api/v1/chain?sessionid=00000000-0000-4000-8000-000000000000") {
        Ok(response) => assert!(
            response.status < 500,
            "the service answered {} after a delete raced an export",
            response.status
        ),
        Err(error) => panic!("the service stopped serving: {error}"),
    }

    // The fixture's own delete is now a no-op; that is fine and expected.
    drop(live);
}
