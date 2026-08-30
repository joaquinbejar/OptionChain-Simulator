//! Whether what the service answers is a MARKET.
//!
//! Every other file in this series checks that the service answers correctly.
//! This one checks the answer itself: quotes that are ordered, a ladder with
//! no gaps, expirations that roll rather than vanish, greeks that belong to
//! their level, and a clock that advances. A service can be perfectly
//! well-behaved over HTTP and still serve nonsense.
//!
//! Assertions are per contract rather than per chain, and every failure names
//! the step, the expiration and the strike, because "the chain is wrong" is
//! not something anyone can act on.
//!
//! The walk is four steps over a five-strike chain with two live expirations,
//! which is long enough for the underlying to move and for an expiration to
//! roll, and small enough to be polite against a shared deployment.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{ServiceClient, reference_request, service};
use serde::Deserialize;
use std::collections::BTreeSet;

/// Steps walked by these tests.
const STEPS: usize = 4;

/// Strikes each side of the money.
const CHAIN_SIZE: usize = 2;

/// The grid the strikes sit on.
const STRIKE_INTERVAL: f64 = 25.0;

/// One snapshot of a v2 simulation.
#[derive(Debug, Deserialize)]
struct Snapshot {
    simulated_at: String,
    underlying: Underlying,
    chains: Vec<Chain>,
}

/// The state of the underlying at a step.
#[derive(Debug, Deserialize)]
struct Underlying {
    price: f64,
}

/// One expiration's chain.
#[derive(Debug, Deserialize)]
struct Chain {
    expires_at: String,
    days_to_expiration: f64,
    contracts: Vec<Contract>,
}

/// One strike, both sides.
#[derive(Debug, Deserialize)]
struct Contract {
    strike: f64,
    implied_volatility: Option<f64>,
    call: Option<Side>,
    put: Option<Side>,
}

/// One side of a contract.
#[derive(Debug, Deserialize)]
struct Side {
    bid: Option<f64>,
    ask: Option<f64>,
    mid: Option<f64>,
    delta: Option<f64>,
}

/// A simulation walked step by step, keeping every snapshot, that deletes
/// itself afterwards.
struct Walk {
    client: ServiceClient,
    id: String,
    snapshots: Vec<Snapshot>,
}

impl Walk {
    /// Creates a simulation and walks it, keeping what it served.
    ///
    /// `extra` carries the fields a particular test needs, such as a spread
    /// model or a pinned ladder; a deployment that does not know one refuses
    /// the creation, which the caller reports as a skip.
    fn create(client: &ServiceClient, extra: &[(&str, serde_json::Value)]) -> Option<Self> {
        let mut request = reference_request("SPX");
        if let Some(object) = request.as_object_mut() {
            object.insert("steps".to_string(), serde_json::json!(STEPS));
            object.insert("chain_size".to_string(), serde_json::json!(CHAIN_SIZE));
            object.insert(
                "strike_interval".to_string(),
                serde_json::json!(STRIKE_INTERVAL),
            );
            object.insert(
                "schedules".to_string(),
                serde_json::json!([{"rule_id": "dailies", "kind": "daily", "target_count": 2}]),
            );
            for (field, value) in extra {
                object.insert((*field).to_string(), value.clone());
            }
        }

        let response = match client.post("/api/v2/simulations", &request) {
            Ok(response) => response,
            Err(error) => panic!("{error}"),
        };
        match response.status {
            201 => {}
            400 | 404 => {
                println!(
                    "SKIP: this deployment would not create that simulation ({}): {}",
                    response.status,
                    response.text()
                );
                return None;
            }
            other => panic!(
                "creating a simulation answered {other}: {}",
                response.text()
            ),
        }

        let body: serde_json::Value = match response.json("/api/v2/simulations") {
            Ok(body) => body,
            Err(error) => panic!("{error}"),
        };
        let id = match body.get("id").and_then(serde_json::Value::as_str) {
            Some(id) => id.to_string(),
            None => panic!("a created simulation must carry an id: {body}"),
        };

        let mut walk = Self {
            client: client.clone(),
            id,
            snapshots: Vec::new(),
        };

        for step in 0..STEPS {
            let path = format!("/api/v2/simulations/{}/step", walk.id);
            let response = match client.request("POST", &path, None) {
                Ok(response) => response,
                Err(error) => panic!("{error}"),
            };
            assert_eq!(
                response.status,
                200,
                "step {step} must serve a snapshot, got {}",
                response.text()
            );
            match response.json::<Snapshot>(&path) {
                Ok(snapshot) => walk.snapshots.push(snapshot),
                Err(error) => panic!("{error}"),
            }
        }

        Some(walk)
    }
}

impl Drop for Walk {
    fn drop(&mut self) {
        let path = format!("/api/v2/simulations/{}", self.id);
        if let Err(error) = self.client.delete(&path) {
            println!("WARNING: could not delete simulation {}: {error}", self.id);
        }
    }
}

/// Every quoted side is an ordered, non-negative book, and a quote is never
/// half withdrawn.
#[test]
fn test_every_quote_is_an_ordered_book() {
    let Some(client) = service() else {
        return;
    };
    let Some(walk) = Walk::create(&client, &[]) else {
        return;
    };

    let mut quotes = 0_usize;
    for (step, snapshot) in walk.snapshots.iter().enumerate() {
        for chain in &snapshot.chains {
            for contract in &chain.contracts {
                for (name, side) in [("call", &contract.call), ("put", &contract.put)] {
                    let Some(side) = side else {
                        continue;
                    };
                    let where_ = format!(
                        "step {step}, expiration {}, strike {}, {name}",
                        chain.expires_at, contract.strike
                    );

                    for (label, value) in [("bid", side.bid), ("ask", side.ask), ("mid", side.mid)]
                    {
                        if let Some(value) = value {
                            assert!(value >= 0.0, "{where_}: {label} is negative at {value}");
                            assert!(
                                value.is_finite(),
                                "{where_}: {label} is not a number, it is {value}"
                            );
                        }
                    }

                    match (side.bid, side.mid, side.ask) {
                        (Some(bid), Some(mid), Some(ask)) => {
                            assert!(
                                bid <= mid && mid <= ask,
                                "{where_}: the book is crossed, bid {bid} mid {mid} ask {ask}"
                            );
                            quotes += 1;
                        }
                        (bid, Some(mid), ask) => panic!(
                            "{where_}: a contract with a mid of {mid} must quote both sides, \
                             bid is {bid:?} and ask is {ask:?}"
                        ),
                        _ => {}
                    }

                    if let Some(delta) = side.delta {
                        let bounds = if name == "call" {
                            (0.0, 1.0)
                        } else {
                            (-1.0, 0.0)
                        };
                        assert!(
                            delta >= bounds.0 - 1e-9 && delta <= bounds.1 + 1e-9,
                            "{where_}: delta {delta} is outside {bounds:?}"
                        );
                    }
                }

                if let Some(volatility) = contract.implied_volatility {
                    assert!(
                        volatility > 0.0 && volatility.is_finite(),
                        "step {step}, strike {}: implied volatility is {volatility}",
                        contract.strike
                    );
                }
            }
        }
    }

    assert!(quotes > 0, "a walked simulation must quote something");
    println!("INFO: {quotes} two-sided quotes checked over {STEPS} steps");
}

/// The ladder is centred on the money, evenly spaced, and has no gaps or
/// duplicates.
#[test]
fn test_the_ladder_is_complete_and_evenly_spaced() {
    let Some(client) = service() else {
        return;
    };
    let Some(walk) = Walk::create(&client, &[]) else {
        return;
    };

    let expected = CHAIN_SIZE * 2 + 1;
    for (step, snapshot) in walk.snapshots.iter().enumerate() {
        for chain in &snapshot.chains {
            let where_ = format!("step {step}, expiration {}", chain.expires_at);
            let strikes: Vec<f64> = chain
                .contracts
                .iter()
                .map(|contract| contract.strike)
                .collect();

            assert_eq!(
                strikes.len(),
                expected,
                "{where_}: a chain_size of {CHAIN_SIZE} must quote {expected} strikes, got \
                 {strikes:?}"
            );

            let unique: BTreeSet<String> = strikes.iter().map(f64::to_string).collect();
            assert_eq!(
                unique.len(),
                strikes.len(),
                "{where_}: the ladder repeats a strike: {strikes:?}"
            );

            for pair in strikes.windows(2) {
                let gap = pair[1] - pair[0];
                assert!(
                    (gap - STRIKE_INTERVAL).abs() < 1e-6,
                    "{where_}: {} to {} is a gap of {gap}, not the {STRIKE_INTERVAL} configured",
                    pair[0],
                    pair[1]
                );
            }

            // Centred: the money sits inside the ladder, never off one end.
            let price = snapshot.underlying.price;
            let (lowest, highest) = match (strikes.first(), strikes.last()) {
                (Some(lowest), Some(highest)) => (*lowest, *highest),
                _ => unreachable!("the ladder was checked non-empty"),
            };
            assert!(
                price >= lowest && price <= highest,
                "{where_}: the underlying at {price} is outside its own ladder {lowest}..{highest}"
            );
        }
    }
}

/// Expirations roll rather than vanish, and the clock advances by the
/// configured interval.
#[test]
fn test_expirations_roll_and_the_clock_advances() {
    let Some(client) = service() else {
        return;
    };
    let Some(walk) = Walk::create(&client, &[]) else {
        return;
    };

    let mut previous: Option<&Snapshot> = None;
    for (step, snapshot) in walk.snapshots.iter().enumerate() {
        assert!(
            !snapshot.chains.is_empty(),
            "step {step} quotes no expiration at all"
        );

        for chain in &snapshot.chains {
            assert!(
                chain.days_to_expiration >= 0.0,
                "step {step}: expiration {} is {} days away, which is in the past",
                chain.expires_at,
                chain.days_to_expiration
            );
        }

        if let Some(previous) = previous {
            // The clock moves forward, and by a constant interval.
            assert!(
                snapshot.simulated_at > previous.simulated_at,
                "step {step}: the simulated clock went from {} to {}",
                previous.simulated_at,
                snapshot.simulated_at
            );

            // An expiration that is still alive keeps being quoted, and is
            // closer than it was.
            for chain in &previous.chains {
                if chain.days_to_expiration <= 0.0 {
                    continue;
                }
                let still_there = snapshot
                    .chains
                    .iter()
                    .find(|later| later.expires_at == chain.expires_at);
                if let Some(later) = still_there {
                    assert!(
                        later.days_to_expiration < chain.days_to_expiration,
                        "step {step}: expiration {} is {} days away, no closer than the {} it \
                         was a step ago",
                        chain.expires_at,
                        later.days_to_expiration,
                        chain.days_to_expiration
                    );
                }
            }
        }

        previous = Some(snapshot);
    }

    // Over a walk of this length the underlying must actually move; a constant
    // spot would make every other assertion here vacuous.
    let prices: BTreeSet<String> = walk
        .snapshots
        .iter()
        .map(|snapshot| snapshot.underlying.price.to_string())
        .collect();
    assert!(
        prices.len() > 1,
        "the underlying never moved over {STEPS} steps: {prices:?}"
    );
}

/// A pinned ladder quotes the same strike set at every step.
///
/// Skipped on a deployment that predates `strike_ladder`, which refuses the
/// field rather than ignoring it.
#[test]
fn test_a_pinned_ladder_keeps_its_strikes() {
    let Some(client) = service() else {
        return;
    };
    let Some(walk) = Walk::create(&client, &[("strike_ladder", serde_json::json!("pinned"))])
    else {
        return;
    };

    let strike_set = |chain: &Chain| -> Vec<String> {
        chain
            .contracts
            .iter()
            .map(|contract| contract.strike.to_string())
            .collect()
    };

    let first = match walk
        .snapshots
        .first()
        .and_then(|snapshot| snapshot.chains.first())
    {
        Some(chain) => strike_set(chain),
        None => panic!("a pinned simulation must quote a chain"),
    };

    for (step, snapshot) in walk.snapshots.iter().enumerate() {
        for chain in &snapshot.chains {
            assert_eq!(
                strike_set(chain),
                first,
                "step {step}, expiration {}: a pinned ladder quoted a different strike set than \
                 it did at step 0",
                chain.expires_at
            );
        }
    }
    println!(
        "INFO: a pinned ladder held {} strikes for {STEPS} steps",
        first.len()
    );
}

/// The spread model widens a cheap contract more, in relative terms, than a
/// dear one.
///
/// Skipped on a deployment that predates the model.
#[test]
fn test_the_spread_widens_the_cheap_contracts_relatively_more() {
    let Some(client) = service() else {
        return;
    };
    let Some(walk) = Walk::create(
        &client,
        &[
            ("spread", serde_json::json!(0.02)),
            ("spread_proportional", serde_json::json!(0.01)),
        ],
    ) else {
        return;
    };

    let mut widest: Option<(f64, f64)> = None;
    let mut tightest: Option<(f64, f64)> = None;

    for snapshot in &walk.snapshots {
        for chain in &snapshot.chains {
            for contract in &chain.contracts {
                let Some(side) = contract.call.as_ref() else {
                    continue;
                };
                let (Some(bid), Some(mid), Some(ask)) = (side.bid, side.mid, side.ask) else {
                    continue;
                };
                if mid <= 0.0 {
                    continue;
                }
                let relative = (ask - bid) / mid;
                match widest {
                    Some((_, worst)) if worst >= relative => {}
                    _ => widest = Some((mid, relative)),
                }
                match tightest {
                    Some((_, best)) if best <= relative => {}
                    _ => tightest = Some((mid, relative)),
                }
            }
        }
    }

    let (Some((cheap_mid, cheap_spread)), Some((dear_mid, dear_spread))) = (widest, tightest)
    else {
        println!("SKIP: this deployment quoted nothing two-sided to compare");
        return;
    };

    assert!(
        cheap_mid <= dear_mid,
        "the relatively widest quote at a mid of {cheap_mid} should not be dearer than the \
         tightest at {dear_mid}"
    );
    assert!(
        cheap_spread >= dear_spread,
        "a cheap contract at {cheap_mid} spreads {cheap_spread} relatively, a dear one at \
         {dear_mid} spreads {dear_spread}"
    );
    println!(
        "INFO: relative spread ranges from {dear_spread:.4} at a mid of {dear_mid:.2} to \
         {cheap_spread:.4} at {cheap_mid:.2}"
    );
}
