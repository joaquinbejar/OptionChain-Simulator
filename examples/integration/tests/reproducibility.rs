//! The seed reproducibility contract, seen from outside the process.
//!
//! Same parameters plus same seed yields an identical snapshot tape. Every
//! test that proves it today runs in process, where the walk is one function
//! call away; this proves it where a consumer meets it, over HTTP against a
//! running deployment, which is also where a cache, a store round trip or a
//! rebuilt walk could break it without any unit test noticing.
//!
//! Comparisons are made on PARSED values rather than on the response text.
//! The JSON rendering of a number is not the contract, the number is: a
//! service that changed how it formats a float would fail a string comparison
//! while serving exactly the same market, and a service that changed the
//! market would pass one if it kept the formatting.
//!
//! The horizon is deliberately short, four steps over a two-strike chain,
//! because these run against a shared deployment.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{Response, ServiceClient, reference_request, service};

/// How many steps each tape carries.
const HORIZON: usize = 4;

/// A simulation that deletes itself.
struct Live {
    client: ServiceClient,
    id: String,
    seed: u64,
}

impl Live {
    /// Creates a v2 simulation, optionally with an explicit seed, and reports
    /// the seed it ended up with.
    fn create(client: &ServiceClient, seed: Option<u64>) -> Option<Self> {
        let mut request = reference_request("SPX");
        if let Some(object) = request.as_object_mut() {
            object.insert("steps".to_string(), serde_json::json!(HORIZON));
            object.insert("chain_size".to_string(), serde_json::json!(2));
            match seed {
                Some(seed) => object.insert("seed".to_string(), serde_json::json!(seed)),
                None => object.remove("seed"),
            };
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

        let body: serde_json::Value = match response.json("/api/v2/simulations") {
            Ok(body) => body,
            Err(error) => panic!("{error}"),
        };
        let id = match body.get("id").and_then(serde_json::Value::as_str) {
            Some(id) => id.to_string(),
            None => panic!("a created simulation must carry an id: {body}"),
        };
        let seed = match body
            .get("parameters")
            .and_then(|parameters| parameters.get("seed"))
            .and_then(serde_json::Value::as_u64)
        {
            Some(seed) => seed,
            None => panic!("a created simulation must echo its effective seed: {body}"),
        };

        Some(Self {
            client: client.clone(),
            id,
            seed,
        })
    }

    /// Serves the snapshot at the cursor and advances.
    fn step(&self) -> Response {
        let path = format!("/api/v2/simulations/{}/step", self.id);
        match self.client.request("POST", &path, None) {
            Ok(response) => response,
            Err(error) => panic!("advancing: {error}"),
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let path = format!("/api/v2/simulations/{}", self.id);
        if let Err(error) = self.client.delete(&path) {
            println!("WARNING: could not delete simulation {}: {error}", self.id);
        }
    }
}

/// The market a snapshot describes, with everything that is not the market
/// stripped out: identity, the cursor, and the wall-clock timestamps differ
/// between two runs by construction and say nothing about the walk.
fn market(snapshot: &serde_json::Value) -> serde_json::Value {
    let mut market = serde_json::Map::new();
    for key in ["underlying", "chains", "simulated_at"] {
        if let Some(value) = snapshot.get(key) {
            market.insert(key.to_string(), value.clone());
        }
    }
    serde_json::Value::Object(market)
}

/// Walks two simulations in lockstep and compares the market at every step.
///
/// `identical` says what must hold: two runs of the same seed must agree at
/// every step, and two runs of different seeds must disagree at some step.
fn walk_together(left: &Live, right: &Live, identical: bool) {
    let mut differed_at = None;

    for step in 0..HORIZON {
        let (one, two) = (left.step(), right.step());
        assert_eq!(
            one.status,
            200,
            "step {step} of the first tape must serve, got {}",
            one.text()
        );
        assert_eq!(
            two.status,
            200,
            "step {step} of the second tape must serve, got {}",
            two.text()
        );

        let one: serde_json::Value = match one.json("/step") {
            Ok(body) => body,
            Err(error) => panic!("{error}"),
        };
        let two: serde_json::Value = match two.json("/step") {
            Ok(body) => body,
            Err(error) => panic!("{error}"),
        };

        let (one, two) = (market(&one), market(&two));
        if identical {
            assert_eq!(
                one, two,
                "step {step} differs between two runs of seed {}; the same parameters and seed \
                 must produce the same market at every step",
                left.seed
            );
        } else if one != two {
            differed_at = Some(step);
        }
    }

    if !identical {
        assert!(
            differed_at.is_some(),
            "seeds {} and {} produced the same tape over {HORIZON} steps, which would make the \
             reproduction assertion vacuous",
            left.seed,
            right.seed
        );
    }
}

/// Two simulations created with the same parameters and the same explicit
/// seed produce the same market at every step.
#[test]
fn test_the_same_seed_reproduces_the_tape_step_by_step() {
    let Some(client) = service() else {
        return;
    };
    let (Some(left), Some(right)) = (
        Live::create(&client, Some(4242)),
        Live::create(&client, Some(4242)),
    ) else {
        return;
    };

    assert_eq!(left.seed, right.seed);
    assert_ne!(left.id, right.id, "these must be two distinct simulations");
    walk_together(&left, &right, true);
}

/// A different seed produces a different tape, so the assertion above cannot
/// pass vacuously.
#[test]
fn test_a_different_seed_produces_a_different_tape() {
    let Some(client) = service() else {
        return;
    };
    let (Some(left), Some(right)) = (
        Live::create(&client, Some(1)),
        Live::create(&client, Some(2)),
    ) else {
        return;
    };

    walk_together(&left, &right, false);
}

/// A run nobody chose a seed for can still be replayed, from the seed the
/// service reported.
///
/// This is the whole reason the effective seed is echoed: a client records
/// what it was given and reproduces the run later.
#[test]
fn test_an_echoed_seed_replays_a_run_nobody_chose_a_seed_for() {
    let Some(client) = service() else {
        return;
    };
    let Some(original) = Live::create(&client, None) else {
        return;
    };
    assert_ne!(original.seed, 0, "a generated seed must be a real one");

    let Some(replay) = Live::create(&client, Some(original.seed)) else {
        return;
    };
    assert_eq!(replay.seed, original.seed);
    walk_together(&original, &replay, true);
}

/// The tape survives the service: a simulation walked halfway, left alone, and
/// then walked to the end matches one walked straight through.
///
/// Between the two halves the deployment may have evicted the cached walk,
/// re-read the simulation from its store, or served other traffic. None of
/// that may change what the second half quotes.
#[test]
fn test_a_tape_walked_in_two_passes_matches_one_walked_straight_through() {
    let Some(client) = service() else {
        return;
    };
    let (Some(paused), Some(straight)) = (
        Live::create(&client, Some(777)),
        Live::create(&client, Some(777)),
    ) else {
        return;
    };

    let half = HORIZON / 2;
    let mut expected = Vec::new();

    for _ in 0..half {
        let response = paused.step();
        assert_eq!(response.status, 200);
    }
    for _ in 0..HORIZON {
        let response = straight.step();
        assert_eq!(response.status, 200);
        let body: serde_json::Value = match response.json("/step") {
            Ok(body) => body,
            Err(error) => panic!("{error}"),
        };
        expected.push(market(&body));
    }

    // Read something else in between, so the second pass is not simply the
    // first one continuing in the same warm path.
    match client.get(&format!("/api/v2/simulations/{}", paused.id)) {
        Ok(response) => assert_eq!(response.status, 200),
        Err(error) => panic!("{error}"),
    }

    for (step, expected) in expected.iter().enumerate().skip(half) {
        let response = paused.step();
        assert_eq!(
            response.status,
            200,
            "the second pass must keep serving at step {step}: {}",
            response.text()
        );
        let body: serde_json::Value = match response.json("/step") {
            Ok(body) => body,
            Err(error) => panic!("{error}"),
        };
        assert_eq!(
            &market(&body),
            expected,
            "step {step} changed when the walk was resumed rather than served in one pass"
        );
    }
}

/// The same contract, for a v1 session.
///
/// v1 is frozen on rendered values and IronCondor consumes it, so its
/// reproduction matters as much as v2's.
#[test]
fn test_a_v1_session_reproduces_its_tape_under_the_same_seed() {
    let Some(client) = service() else {
        return;
    };

    let request = |seed: u64| {
        serde_json::json!({
            "symbol": "AAPL",
            "steps": HORIZON,
            "initial_price": 100.0,
            "days_to_expiration": 30.0,
            "volatility": 0.2,
            "risk_free_rate": 0.05,
            "dividend_yield": 0.01,
            "method": {"GeometricBrownian": {"dt": 0.004, "drift": 0.05, "volatility": 0.2}},
            "time_frame": "Day",
            "chain_size": 2,
            "strike_interval": 5.0,
            "seed": seed
        })
    };

    let create = |seed: u64| -> Option<String> {
        match client.post("/api/v1/chain", &request(seed)) {
            Ok(response) if response.status == 404 => {
                println!("SKIP: this deployment does not serve /api/v1/chain");
                None
            }
            Ok(response) => {
                assert_eq!(
                    response.status,
                    201,
                    "creating a v1 session must answer 201, got {}",
                    response.text()
                );
                let body: serde_json::Value = match response.json("/api/v1/chain") {
                    Ok(body) => body,
                    Err(error) => panic!("{error}"),
                };
                body.get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            }
            Err(error) => panic!("{error}"),
        }
    };

    let (Some(left), Some(right)) = (create(11), create(11)) else {
        return;
    };

    for step in 0..HORIZON {
        let mut markets = Vec::new();
        for id in [&left, &right] {
            let path = format!("/api/v1/chain/step?sessionid={id}");
            let response = match client.request("POST", &path, None) {
                Ok(response) => response,
                Err(error) => panic!("{error}"),
            };
            assert_eq!(
                response.status,
                200,
                "step {step} must serve, got {}",
                response.text()
            );
            let body: serde_json::Value = match response.json(&path) {
                Ok(body) => body,
                Err(error) => panic!("{error}"),
            };
            // A v1 snapshot carries a wall-clock timestamp and a
            // `session_info` block holding the session id and the cursor.
            // Both differ between two runs by construction and say nothing
            // about the market. Everything else must match, and does:
            // verified against a live service, where those were the ONLY
            // differences between two same-seed sessions, down to the last
            // digit of every quote, greek and implied volatility.
            let mut snapshot = body;
            if let Some(object) = snapshot.as_object_mut() {
                object.remove("timestamp");
                object.remove("session_info");
            }
            markets.push(snapshot);
        }
        assert_eq!(
            markets[0], markets[1],
            "step {step} differs between two v1 sessions created with the same seed"
        );
    }

    for id in [&left, &right] {
        let _ = client.delete(&format!("/api/v1/chain?sessionid={id}"));
    }
}
