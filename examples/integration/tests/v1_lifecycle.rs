//! The v1 session lifecycle, over the wire.
//!
//! `/api/v1/chain` is frozen on RENDERED VALUES, not only on shapes (ADR 0001
//! §12.1), and IronCondor consumes it. What is frozen is what a client
//! receives, so the contract deserves a test that talks to a deployment rather
//! than to a handler.
//!
//! Each test creates its own session and deletes it, including on failure, so
//! none depends on another and none leaves state on a shared service.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{ServiceClient, service};
use serde::Deserialize;

/// The session envelope every v1 verb but the snapshot answers with.
#[derive(Debug, Deserialize)]
struct SessionEnvelope {
    id: String,
    parameters: Parameters,
    current_step: usize,
    total_steps: usize,
    state: String,
}

/// The parameters echoed back, including the effective seed.
#[derive(Debug, Deserialize)]
struct Parameters {
    symbol: String,
    volatility: f64,
    seed: Option<u64>,
}

/// One snapshot of the chain.
#[derive(Debug, Deserialize)]
struct Snapshot {
    underlying: String,
    price: f64,
    contracts: Vec<Contract>,
}

/// One contract in a snapshot.
#[derive(Debug, Deserialize)]
struct Contract {
    strike: f64,
}

/// A v1 session that deletes itself, so a failing test leaves nothing behind.
struct Session {
    client: ServiceClient,
    id: String,
}

impl Session {
    /// Creates a session with `steps` steps and a deliberately narrow chain,
    /// since these run against a shared deployment.
    fn create(
        client: &ServiceClient,
        steps: usize,
        seed: Option<u64>,
    ) -> Option<(Self, SessionEnvelope)> {
        let mut request = serde_json::json!({
            "symbol": "AAPL",
            "steps": steps,
            "initial_price": 100.0,
            "days_to_expiration": 30.0,
            "volatility": 0.2,
            "risk_free_rate": 0.05,
            "dividend_yield": 0.01,
            "method": {"GeometricBrownian": {"dt": 0.004, "drift": 0.05, "volatility": 0.2}},
            "time_frame": "Day",
            "chain_size": 2,
            "strike_interval": 5.0
        });
        if let (Some(object), Some(seed)) = (request.as_object_mut(), seed) {
            object.insert("seed".to_string(), serde_json::json!(seed));
        }

        let response = match client.post("/api/v1/chain", &request) {
            Ok(response) => response,
            Err(error) => panic!("creating a v1 session: {error}"),
        };
        if response.status == 404 {
            println!("SKIP: this deployment does not serve /api/v1/chain");
            return None;
        }
        assert_eq!(
            response.status,
            201,
            "creating a session must answer 201, got {} with {}",
            response.status,
            response.text()
        );

        let envelope: SessionEnvelope = match response.json("/api/v1/chain") {
            Ok(envelope) => envelope,
            Err(error) => panic!("{error}"),
        };
        let session = Self {
            client: client.clone(),
            id: envelope.id.clone(),
        };
        Some((session, envelope))
    }

    /// The query every v1 verb takes.
    fn query(&self) -> String {
        format!("/api/v1/chain?sessionid={}", self.id)
    }

    /// Peeks the snapshot the next advance would serve.
    fn peek(&self) -> examples_integration::Response {
        match self.client.get(&self.query()) {
            Ok(response) => response,
            Err(error) => panic!("peeking: {error}"),
        }
    }

    /// Serves the snapshot at the cursor and advances.
    fn step(&self) -> examples_integration::Response {
        let path = format!("/api/v1/chain/step?sessionid={}", self.id);
        match self.client.request("POST", &path, None) {
            Ok(response) => response,
            Err(error) => panic!("advancing: {error}"),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.client.delete(&self.query()) {
            println!("WARNING: could not delete session {}: {error}", self.id);
        }
    }
}

/// Creating a session echoes its parameters and an EFFECTIVE seed, generated
/// because the request omitted one. That echo is what lets a client record a
/// run and replay it, so its absence is a contract break, not a cosmetic one.
#[test]
fn test_creating_a_session_echoes_an_effective_seed() {
    let Some(client) = service() else {
        return;
    };
    let Some((session, envelope)) = Session::create(&client, 3, None) else {
        return;
    };

    assert_eq!(envelope.parameters.symbol, "AAPL");
    assert_eq!(envelope.total_steps, 3);
    assert_eq!(envelope.current_step, 0);
    assert_eq!(envelope.state, "Initialized");
    assert!(
        envelope.parameters.seed.is_some(),
        "a session created without a seed must be told which one it got: {envelope:?}"
    );

    // And the seed is stable for the session's life, since replaying depends
    // on it not changing under the client. A no-op PATCH is the cheapest verb
    // that answers the envelope again.
    assert_eq!(session.peek().status, 200);
    let reread: SessionEnvelope = match client.request("PATCH", &session.query(), Some("{}")) {
        Ok(response) => match response.json(&session.query()) {
            Ok(envelope) => envelope,
            Err(error) => panic!("{error}"),
        },
        Err(error) => panic!("{error}"),
    };
    assert_eq!(
        reread.parameters.seed, envelope.parameters.seed,
        "the effective seed must not change under the client"
    );
}

/// A peek is safe and repeatable: the same market twice, and the cursor does
/// not move.
#[test]
fn test_a_peek_is_repeatable_and_does_not_advance() {
    let Some(client) = service() else {
        return;
    };
    let Some((session, _)) = Session::create(&client, 3, Some(42)) else {
        return;
    };

    let first = session.peek();
    let second = session.peek();
    assert_eq!(first.status, 200);
    assert_eq!(second.status, 200);

    let first: Snapshot = match first.json(&session.query()) {
        Ok(snapshot) => snapshot,
        Err(error) => panic!("{error}"),
    };
    let second: Snapshot = match second.json(&session.query()) {
        Ok(snapshot) => snapshot,
        Err(error) => panic!("{error}"),
    };

    assert_eq!(first.underlying, "AAPL");
    assert!(
        !first.contracts.is_empty(),
        "a snapshot must quote contracts"
    );
    assert_eq!(
        first.price, second.price,
        "peeking twice must describe the same market"
    );
    let strikes = |snapshot: &Snapshot| -> Vec<String> {
        snapshot
            .contracts
            .iter()
            .map(|contract| contract.strike.to_string())
            .collect()
    };
    assert_eq!(
        strikes(&first),
        strikes(&second),
        "peeking twice must quote the same ladder"
    );
}

/// The serve-then-advance walk, and the `410` boundary exactly.
///
/// A session with `steps = N` serves N snapshots over N advances; the advance
/// after the last one is `410 Gone`, and so is every peek from then on.
#[test]
fn test_the_walk_serves_every_step_and_then_is_gone() {
    let Some(client) = service() else {
        return;
    };
    let steps = 3;
    let Some((session, _)) = Session::create(&client, steps, Some(7)) else {
        return;
    };

    for index in 0..steps {
        let response = session.step();
        assert_eq!(
            response.status,
            200,
            "advance {index} of {steps} must serve a snapshot, got {} with {}",
            response.status,
            response.text()
        );
        let snapshot: Snapshot = match response.json("/api/v1/chain/step") {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("{error}"),
        };
        assert!(
            !snapshot.contracts.is_empty(),
            "every served snapshot must quote contracts"
        );
    }

    let past_the_end = session.step();
    assert_eq!(
        past_the_end.status,
        410,
        "the advance past the last step must be 410 Gone, got {} with {}",
        past_the_end.status,
        past_the_end.text()
    );

    assert_eq!(
        session.peek().status,
        410,
        "a completed session has no next snapshot to peek"
    );
}

/// `PATCH` updates parameters and the change is visible in what the session
/// reports; `PUT` replaces them wholesale.
#[test]
fn test_updating_and_replacing_a_session_take_effect() {
    let Some(client) = service() else {
        return;
    };
    let Some((session, created)) = Session::create(&client, 4, Some(11)) else {
        return;
    };

    let patched: SessionEnvelope =
        match client.request("PATCH", &session.query(), Some(r#"{"volatility":0.35}"#)) {
            Ok(response) => {
                assert_eq!(
                    response.status,
                    200,
                    "PATCH must answer 200, got {} with {}",
                    response.status,
                    response.text()
                );
                match response.json(&session.query()) {
                    Ok(envelope) => envelope,
                    Err(error) => panic!("{error}"),
                }
            }
            Err(error) => panic!("{error}"),
        };

    assert_ne!(
        patched.parameters.volatility, created.parameters.volatility,
        "a patched parameter must actually change"
    );
    assert!(
        (patched.parameters.volatility - 0.35).abs() < 1e-9,
        "volatility must be what the patch asked for, got {}",
        patched.parameters.volatility
    );
    assert_eq!(
        patched.id, created.id,
        "PATCH must not create a new session"
    );

    let replacement = serde_json::json!({
        "symbol": "AAPL",
        "steps": 4,
        "initial_price": 120.0,
        "days_to_expiration": 30.0,
        "volatility": 0.25,
        "risk_free_rate": 0.05,
        "dividend_yield": 0.01,
        "method": {"GeometricBrownian": {"dt": 0.004, "drift": 0.05, "volatility": 0.25}},
        "time_frame": "Day",
        "chain_size": 2,
        "strike_interval": 5.0
    });
    match client.request("PUT", &session.query(), Some(&replacement.to_string())) {
        Ok(response) => {
            assert_eq!(
                response.status,
                200,
                "PUT must answer 200, got {} with {}",
                response.status,
                response.text()
            );
            let replaced: SessionEnvelope = match response.json(&session.query()) {
                Ok(envelope) => envelope,
                Err(error) => panic!("{error}"),
            };
            assert_eq!(replaced.id, created.id, "PUT must not create a new session");
            assert_eq!(
                replaced.current_step, 0,
                "a replaced session restarts its walk"
            );
        }
        Err(error) => panic!("{error}"),
    }
}

/// `DELETE` removes the session, and every verb answers 404 afterwards.
#[test]
fn test_a_deleted_session_is_gone_for_every_verb() {
    let Some(client) = service() else {
        return;
    };
    let Some((session, _)) = Session::create(&client, 2, Some(3)) else {
        return;
    };

    let query = session.query();
    let step = format!("/api/v1/chain/step?sessionid={}", session.id);

    match client.delete(&query) {
        Ok(response) => assert_eq!(
            response.status,
            200,
            "DELETE must answer 200, got {}",
            response.text()
        ),
        Err(error) => panic!("{error}"),
    }

    for (method, path) in [
        ("GET", query.as_str()),
        ("POST", step.as_str()),
        ("DELETE", query.as_str()),
    ] {
        let response = match client.request(method, path, None) {
            Ok(response) => response,
            Err(error) => panic!("{method} {path}: {error}"),
        };
        assert_eq!(
            response.status,
            404,
            "{method} on a deleted session must be 404, got {} with {}",
            response.status,
            response.text()
        );
    }
}
