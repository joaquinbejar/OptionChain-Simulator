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
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Barrier, mpsc};
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
        examples_integration::report_cleanup(&self.client, &self.path(""), &self.id);
    }
}

/// A simulation with a chosen seed, so two of them are genuinely different
/// walks rather than two copies of one.
impl Live {
    /// Creates a simulation with an explicit seed.
    fn create_seeded(client: &ServiceClient, steps: usize, seed: u64) -> Option<Self> {
        let mut request = reference_request("SPX");
        if let Some(object) = request.as_object_mut() {
            object.insert("steps".to_string(), serde_json::json!(steps));
            object.insert("chain_size".to_string(), serde_json::json!(2));
            object.insert("seed".to_string(), serde_json::json!(seed));
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

    /// The version this simulation is on.
    fn version(&self) -> u64 {
        let path = self.path("");
        match self.client.get(&path) {
            Ok(response) => {
                let body: serde_json::Value = match response.json(&path) {
                    Ok(body) => body,
                    Err(error) => panic!("{error}"),
                };
                body.get("version")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_else(|| panic!("the envelope must carry a version: {body}"))
            }
            Err(error) => panic!("{error}"),
        }
    }

    /// Serves one step and returns the market it served.
    fn serve(&self) -> serde_json::Value {
        let path = self.path("/step");
        match self.client.request("POST", &path, None) {
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
            Err(error) => panic!("advancing: {error}"),
        }
    }
}

/// The market part of a snapshot: what the walk decided, without the identity
/// and cursor that differ between two simulations by construction.
fn market(snapshot: &serde_json::Value) -> serde_json::Value {
    let mut market = serde_json::Map::new();
    for key in ["simulated_at", "underlying", "chains"] {
        let value = snapshot
            .get(key)
            .unwrap_or_else(|| panic!("a snapshot must carry {key}: {snapshot}"));
        market.insert(key.to_string(), value.clone());
    }
    serde_json::Value::Object(market)
}

/// A response read from a raw socket, headers first, so a test can act while
/// the body is still arriving.
struct Streaming {
    /// The status line's code.
    status: u16,
    /// The first body bytes that arrived with the headers, if any.
    first_body_bytes: usize,
    /// The socket, still open, mid-body.
    socket: TcpStream,
}

/// Sends a GET and reads only as far as the end of the headers plus whatever
/// body bytes came with them.
///
/// This is what proves an export STREAMS: a service that materialised the
/// whole thing first would not have sent a status line yet.
fn start_streaming(client: &ServiceClient, path: &str) -> Streaming {
    let address = client.base_url().trim_start_matches("http://").to_string();
    let address = if address.contains(':') {
        address
    } else {
        format!("{address}:80")
    };

    let mut socket = match TcpStream::connect(&address) {
        Ok(socket) => socket,
        Err(error) => panic!("connecting to {address}: {error}"),
    };
    if let Err(error) = socket.set_read_timeout(Some(Duration::from_secs(30))) {
        panic!("{error}");
    }
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    if let Err(error) = socket.write_all(request.as_bytes()) {
        panic!("{error}");
    }

    // Read byte by byte to the end of the headers, so nothing of the body is
    // consumed accidentally.
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match socket.read(&mut byte) {
            Ok(0) => panic!("{path}: the connection closed before the headers ended"),
            Ok(_) => {
                headers.push(byte[0]);
                if headers.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(error) => panic!("{path}: reading the headers: {error}"),
        }
    }

    let text = String::from_utf8_lossy(&headers).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("{path}: no status line in {text:?}"));

    // And then one read of the body, which is what tells us bytes are flowing
    // rather than being assembled.
    let mut buffer = [0_u8; 1024];
    let first_body_bytes = if status == 200 {
        match socket.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => panic!("{path}: reading the first body bytes: {error}"),
        }
    } else {
        0
    };

    Streaming {
        status,
        first_body_bytes,
        socket,
    }
}

/// The rows of a CSV export, header first, parsed enough to count.
fn csv_rows(body: &str) -> Vec<Vec<String>> {
    body.split("\r\n")
        .filter(|line| !line.is_empty())
        .map(|line| line.split(',').map(str::to_string).collect())
        .collect()
}

/// Several clients exporting one simulation SIMULTANEOUSLY receive identical
/// bytes, and none of them moves the cursor.
///
/// The clients are released by a barrier: threads started in a loop can
/// serialise completely, and a test that only proves sequential exports agree
/// has not exercised a simultaneous reader at all.
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
    let before_version = live.version();
    let query = live.path("/export?dataset=option_chains&format=csv");
    let barrier = Arc::new(Barrier::new(CLIENTS));
    let (sender, receiver) = mpsc::channel();

    let mut handles = Vec::new();
    for _ in 0..CLIENTS {
        let client = client.clone();
        let query = query.clone();
        let sender = sender.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
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
    assert_eq!(
        live.version(),
        before_version,
        "exporting must not bump the version"
    );
}

/// Interleaved advances of two simulations stay independent, compared against
/// what each produces on its own.
///
/// Two simulations with the SAME seed would agree whatever the service did
/// with them, including serving one tape for both, so these carry different
/// seeds and are compared with standalone reference tapes walked beforehand.
#[test]
fn test_two_simulations_advance_independently() {
    let Some(client) = service() else {
        return;
    };

    // Reference tapes, walked alone, one simulation at a time.
    let mut expected = Vec::new();
    for seed in [11_u64, 22] {
        let Some(reference) = Live::create_seeded(&client, STEPS, seed) else {
            return;
        };
        let tape: Vec<serde_json::Value> = (0..STEPS).map(|_| reference.serve()).collect();
        expected.push(tape);
    }

    // The same two seeds again, this time interleaved.
    let (Some(left), Some(right)) = (
        Live::create_seeded(&client, STEPS, 11),
        Live::create_seeded(&client, STEPS, 22),
    ) else {
        return;
    };

    for round in 0..STEPS {
        for (index, (live, alone)) in [&left, &right].into_iter().zip(&expected).enumerate() {
            let served = live.serve();
            assert_eq!(
                served,
                alone[round],
                "round {round}: the interleaved simulation with seed {} served a different \
                 market than it does alone, so the two are not independent",
                if index == 0 { 11 } else { 22 }
            );
        }
        assert_eq!(
            left.cursor(),
            right.cursor(),
            "round {round}: interleaved simulations must advance one step each"
        );
    }

    assert_ne!(
        expected[0], expected[1],
        "the two seeds must produce different tapes, or this proves nothing"
    );
    assert_eq!(left.cursor(), STEPS);
    assert_eq!(right.cursor(), STEPS);
}

/// Advances fired simultaneously produce exactly one winner and
/// `CLIENTS - 1` conflicts, and move the cursor and the version by one.
#[test]
fn test_contended_advances_have_exactly_one_winner_per_step() {
    let Some(client) = service() else {
        return;
    };
    let Some(live) = Live::create(&client, STEPS * 3, 2) else {
        return;
    };

    let mut contended = false;
    for attempt in 0..5 {
        let before = live.cursor();
        let before_version = live.version();
        let barrier = Arc::new(Barrier::new(CLIENTS));
        let (sender, receiver) = mpsc::channel();

        let mut handles = Vec::new();
        for _ in 0..CLIENTS {
            let client = client.clone();
            let path = live.path("/step");
            let sender = sender.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
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
        assert_eq!(statuses.len(), CLIENTS);
        assert!(
            statuses
                .iter()
                .all(|status| *status == 200 || *status == 409),
            "a contended advance answers 200 or 409, never anything else, got {statuses:?}"
        );

        let winners = statuses.iter().filter(|status| **status == 200).count();
        let moved = live.cursor() - before;
        let versions = live.version() - before_version;
        assert_eq!(
            moved, winners,
            "the cursor moved {moved} for {winners} winners, whatever the contention was"
        );
        assert_eq!(
            versions as usize, winners,
            "the version moved {versions} for {winners} winners"
        );

        if winners == 1 {
            assert_eq!(
                statuses.iter().filter(|status| **status == 409).count(),
                CLIENTS - 1,
                "one winner means {} conflicts, got {statuses:?}",
                CLIENTS - 1
            );
            contended = true;
            break;
        }
        println!("INFO: attempt {attempt} serialised ({winners} winners), retrying");
    }

    assert!(
        contended,
        "five barrier-synchronised attempts never produced a single winner, so the conflict \
         path was not exercised"
    );
}

/// A peek during another client's advance returns a complete chain, and at
/// least one such peek really happened while the walk was moving.
#[test]
fn test_a_peek_during_an_advance_is_never_half_built() {
    let Some(client) = service() else {
        return;
    };
    let steps = STEPS * 3;
    let Some(live) = Live::create(&client, steps, 2) else {
        return;
    };

    let barrier = Arc::new(Barrier::new(2));
    let advancing = {
        let client = client.clone();
        let path = live.path("/step");
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            let mut served = 0_usize;
            for _ in 0..steps {
                match client.request("POST", &path, None) {
                    Ok(response) => {
                        assert!(
                            response.status == 200 || response.status == 410,
                            "an advance answered {} with {}",
                            response.status,
                            response.text()
                        );
                        if response.status == 200 {
                            served += 1;
                        }
                    }
                    Err(error) => panic!("advancing: {error}"),
                }
            }
            served
        })
    };

    barrier.wait();
    let snapshot_path = live.path("/snapshot");
    let mut complete_peeks = 0_usize;
    for _ in 0..steps * 2 {
        let response = match client.get(&snapshot_path) {
            Ok(response) => response,
            Err(error) => panic!("{error}"),
        };
        // 410 once the walk has finished is correct.
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
            // The shape a complete chain has: 2n + 1 strikes, each with both
            // sides. A half-built one is exactly what this catches.
            assert_eq!(
                contracts.len(),
                5,
                "a chain served mid-advance must carry its whole ladder: {chain}"
            );
            for contract in contracts {
                for key in ["strike", "implied_volatility", "call", "put"] {
                    assert!(
                        contract.get(key).is_some(),
                        "a contract served mid-advance must carry {key}: {contract}"
                    );
                }
            }
        }
        complete_peeks += 1;
    }

    let served = match advancing.join() {
        Ok(served) => served,
        Err(_) => panic!("the advancing client panicked"),
    };
    assert_eq!(served, steps, "every advance must have served a step");
    assert!(
        complete_peeks > 0,
        "no peek succeeded while the walk was moving, so nothing was exercised"
    );
    println!("INFO: {complete_peeks} complete peeks during a concurrent walk of {steps} steps");
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

    let path = live.path("/export?dataset=option_chains&format=csv&greeks=all");
    let started = Instant::now();
    let streaming = start_streaming(&client, &path);
    let first_byte = started.elapsed();

    // Body bytes, not merely a status line: the point is that data flowed
    // before the service could have finished pricing the whole tape.
    assert_eq!(streaming.status, 200, "the export must start with a 200");
    assert!(
        streaming.first_body_bytes > 0,
        "the export sent headers and no body, so nothing proves it streams"
    );
    println!(
        "INFO: {} body bytes arrived after {first_byte:?}, then the client walked away \
         mid-stream",
        streaming.first_body_bytes
    );
    drop(streaming.socket);

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

    // And the same export, read to the end this time, is COMPLETE: every step
    // present, every row as wide as the header. A CRLF at the end proves
    // nothing, since every row ends in one.
    let whole = match client.get(&path) {
        Ok(response) => response,
        Err(error) => panic!("{error}"),
    };
    assert_eq!(whole.status, 200);
    let text = whole.text();
    let rows = csv_rows(&text);
    let (header, data) = match rows.split_first() {
        Some((header, data)) => (header, data),
        None => panic!("the export must carry a header"),
    };
    assert!(!data.is_empty(), "the export must carry rows");
    for (index, row) in data.iter().enumerate() {
        assert_eq!(
            row.len(),
            header.len(),
            "row {index} is {} wide where the header is {}",
            row.len(),
            header.len()
        );
    }

    if let Some(step_at) = header.iter().position(|column| column == "step") {
        let steps: std::collections::BTreeSet<&str> = data
            .iter()
            .filter_map(|row| row.get(step_at).map(String::as_str))
            .collect();
        let expected: std::collections::BTreeSet<String> =
            (0..STEPS).map(|step| step.to_string()).collect();
        let expected: std::collections::BTreeSet<&str> =
            expected.iter().map(String::as_str).collect();
        assert_eq!(
            steps, expected,
            "the export must carry every step of the tape, it carried {steps:?}"
        );

        // Every step must carry the same number of rows: a stream cut at a row
        // boundary loses a whole step's tail and nothing else would notice.
        let per_step = data
            .iter()
            .filter(|row| row.get(step_at).map(String::as_str) == Some("0"))
            .count();
        for step in 1..STEPS {
            let step = step.to_string();
            let count = data
                .iter()
                .filter(|row| row.get(step_at) == Some(&step))
                .count();
            assert_eq!(
                count, per_step,
                "step {step} carries {count} rows where step 0 carries {per_step}"
            );
        }
    }

    println!(
        "INFO: the whole export is {} bytes over {STEPS} steps of a 25-strike chain",
        whole.body.len()
    );
}

/// An export in flight when the simulation is deleted either completes or
/// fails cleanly, and the service keeps serving either way.
///
/// The delete waits for a handshake: without it the delete can win the race
/// before the export ever starts, and the test then passes having exercised
/// nothing.
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
    let (started, wait_for_start) = mpsc::channel();

    let exporting = {
        let client = client.clone();
        let path = path.clone();
        thread::spawn(move || {
            // Headers and the first body bytes first, then signal: the delete
            // must race an export that is genuinely in flight.
            let streaming = start_streaming(&client, &path);
            let _ = started.send((streaming.status, streaming.first_body_bytes));

            // Keep reading to the end, which is where a delete mid-stream
            // would show up.
            let mut rest = Vec::new();
            let mut reader = BufReader::new(streaming.socket);
            let read = reader.read_to_end(&mut rest);
            (streaming.status, streaming.first_body_bytes, read.is_ok())
        })
    };

    let (status, first_bytes) = match wait_for_start.recv_timeout(Duration::from_secs(30)) {
        Ok(signal) => signal,
        Err(error) => panic!("the export never started: {error}"),
    };
    assert_eq!(
        status, 200,
        "the export must have started before the delete"
    );
    assert!(
        first_bytes > 0,
        "the export must be streaming body bytes before the delete"
    );

    // The id is known-live at this point, so the delete must succeed.
    let deleted = match client.delete(&delete_path) {
        Ok(response) => response.status,
        Err(error) => panic!("{error}"),
    };
    assert_eq!(
        deleted, 200,
        "deleting a live simulation must answer 200 even with an export in flight"
    );

    match exporting.join() {
        Ok((_, _, read_ok)) => assert!(
            read_ok,
            "the in-flight export neither completed nor failed cleanly: the read errored"
        ),
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

    // The fixture's own delete is now a no-op; that is expected.
    drop(live);
}
