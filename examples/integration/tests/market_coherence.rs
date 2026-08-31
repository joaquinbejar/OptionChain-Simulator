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

/// The interval the simulated clock advances by, in seconds. The default the
/// service applies when a request names none.
const STEP_INTERVAL_SECONDS: u64 = 86_400;

/// Seconds from `from` to `to`, both RFC 3339 instants.
///
/// Parsed rather than compared as text: `>` on strings would accept a clock
/// that moved by the wrong amount, which is exactly what has to be caught.
fn seconds_between(from: &str, to: &str) -> i64 {
    epoch_seconds(to) - epoch_seconds(from)
}

/// One RFC 3339 instant, in seconds since the epoch.
fn epoch_seconds(instant: &str) -> i64 {
    let bytes = instant.as_bytes();
    let number = |start: usize, end: usize| -> i64 {
        instant
            .get(start..end)
            .and_then(|slice| slice.parse::<i64>().ok())
            .unwrap_or_else(|| panic!("{instant} is not an RFC 3339 instant"))
    };
    assert!(
        bytes.len() >= 20 && bytes.get(10) == Some(&b'T'),
        "{instant} is not an RFC 3339 instant"
    );

    let (year, month, day) = (number(0, 4), number(5, 7), number(8, 10));
    let (hour, minute, second) = (number(11, 13), number(14, 16), number(17, 19));

    // Days from the civil date, the standard algorithm.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    days * 86_400 + hour * 3_600 + minute * 60 + second
}

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
///
/// `implied_volatility`, `call` and `put` are REQUIRED by the public
/// response, so they are required here: an `Option` would let a deployment
/// stop quoting a side and keep this suite green. Only `gamma` and the greek
/// block are optional, and only because the greek level decides them.
#[derive(Debug, Deserialize)]
struct Contract {
    strike: f64,
    implied_volatility: f64,
    #[serde(default)]
    gamma: Option<f64>,
    call: Side,
    put: Side,
}

/// One side of a contract.
///
/// `bid`, `ask` and `mid` are nullable in the wire shape — a wing worth
/// nothing has no quote — but `delta` is the convenience field the response
/// always carries.
#[derive(Debug, Deserialize)]
struct Side {
    bid: Option<f64>,
    ask: Option<f64>,
    mid: Option<f64>,
    delta: f64,
    /// Present only at the `first` and `all` greek levels.
    #[serde(default)]
    greeks: Option<Greeks>,
}

/// The greek block a snapshot carries when one is asked for.
///
/// `delta` and `gamma` are absent at the `first` level and present at `all`,
/// which is not an accident: at `first` they already sit on the side and on
/// the contract, so repeating them would be two places to disagree. Both are
/// asserted below rather than merely tolerated.
#[derive(Debug, Clone, Deserialize)]
struct Greeks {
    #[serde(default)]
    delta: Option<f64>,
    #[serde(default)]
    gamma: Option<f64>,
    theta: f64,
    vega: f64,
    rho: f64,
    rho_d: f64,
    /// The second-order set, present only at `all`.
    #[serde(default)]
    alpha: Option<f64>,
    #[serde(default)]
    vanna: Option<f64>,
    #[serde(default)]
    vomma: Option<f64>,
    #[serde(default)]
    veta: Option<f64>,
    #[serde(default)]
    charm: Option<f64>,
    #[serde(default)]
    color: Option<f64>,
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
        Self::create_walking(client, extra, STEPS)
    }

    /// The same, stopping one step short, so `/snapshot` still has a step to
    /// peek at.
    fn create_partial(client: &ServiceClient) -> Option<Self> {
        Self::create_walking(client, &[], STEPS - 1)
    }

    /// Creates a simulation and walks it `walk_steps` times.
    fn create_walking(
        client: &ServiceClient,
        extra: &[(&str, serde_json::Value)],
        walk_steps: usize,
    ) -> Option<Self> {
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

        for step in 0..walk_steps {
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

                    let bounds = if name == "call" {
                        (0.0, 1.0)
                    } else {
                        (-1.0, 0.0)
                    };
                    assert!(
                        side.delta >= bounds.0 - 1e-9 && side.delta <= bounds.1 + 1e-9,
                        "{where_}: delta {} is outside {bounds:?}",
                        side.delta
                    );
                }

                assert!(
                    contract.implied_volatility > 0.0 && contract.implied_volatility.is_finite(),
                    "step {step}, strike {}: implied volatility is {}",
                    contract.strike,
                    contract.implied_volatility
                );
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

            // Centred, which is a stronger statement than "the spot is
            // somewhere inside": the middle strike must be the grid point
            // nearest the spot, with exactly CHAIN_SIZE strikes each side. A
            // ladder shifted toward either edge still contains the spot.
            let price = snapshot.underlying.price;
            let expected_centre = (price / STRIKE_INTERVAL).round() * STRIKE_INTERVAL;
            let middle = match strikes.get(CHAIN_SIZE) {
                Some(middle) => *middle,
                None => unreachable!("the ladder width was asserted above"),
            };
            assert!(
                (middle - expected_centre).abs() < 1e-6,
                "{where_}: the ladder is centred on {middle} while the grid point nearest the \
                 spot of {price} is {expected_centre}"
            );

            let below = strikes.iter().filter(|strike| **strike < middle).count();
            let above = strikes.iter().filter(|strike| **strike > middle).count();
            assert_eq!(
                (below, above),
                (CHAIN_SIZE, CHAIN_SIZE),
                "{where_}: the ladder carries {below} strikes below the money and {above} above, \
                 where a chain_size of {CHAIN_SIZE} means {CHAIN_SIZE} each side"
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
            // The clock advances by the CONFIGURED interval, not merely
            // forwards: a step that moved by something else is a different
            // simulation from the one the parameters describe.
            let moved = seconds_between(&previous.simulated_at, &snapshot.simulated_at);
            assert_eq!(
                moved, STEP_INTERVAL_SECONDS as i64,
                "step {step}: the clock moved {moved} seconds, where the simulation is \
                 configured for {STEP_INTERVAL_SECONDS}, from {} to {}",
                previous.simulated_at, snapshot.simulated_at
            );

            // An expiration that has not been reached must STILL be quoted.
            // Treating its disappearance as acceptable is what let the
            // previous version of this test pass while a chain vanished.
            for chain in &previous.chains {
                let expired = seconds_between(&snapshot.simulated_at, &chain.expires_at) <= 0;
                let later = snapshot
                    .chains
                    .iter()
                    .find(|later| later.expires_at == chain.expires_at);

                match (expired, later) {
                    (false, None) => panic!(
                        "step {step}: expiration {} vanished while it is still live at {}",
                        chain.expires_at, snapshot.simulated_at
                    ),
                    (false, Some(later)) => assert!(
                        later.days_to_expiration < chain.days_to_expiration,
                        "step {step}: expiration {} is {} days away, no closer than the {} it \
                         was a step ago",
                        chain.expires_at,
                        later.days_to_expiration,
                        chain.days_to_expiration
                    ),
                    // Reached its expiry: it may roll out of the chain.
                    (true, _) => {}
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

/// Each greek level carries exactly the set it documents, and the same
/// numbers reach the snapshot, its convenience field and the export.
///
/// Three renderings of one quantity is three chances to disagree: `delta` sits
/// on the side as a convenience, inside the greek block, and as a column in
/// the export. A consumer that reads one and reasons about another needs them
/// to be the same number.
#[test]
fn test_the_greek_levels_carry_their_documented_sets_and_agree() {
    let Some(client) = service() else {
        return;
    };
    // One step short of the end, so the snapshot endpoint still has a step to
    // peek at, and every served step is in the export.
    let Some(walk) = Walk::create_partial(&client) else {
        return;
    };

    let snapshot_at = |level: &str| -> Option<Snapshot> {
        let path = format!("/api/v2/simulations/{}/snapshot?greeks={level}", walk.id);
        match client.get(&path) {
            Ok(response) if response.status == 400 => {
                println!("SKIP: this deployment does not know the {level} greek level");
                None
            }
            Ok(response) => {
                assert_eq!(
                    response.status,
                    200,
                    "the {level} greek level must serve, got {}",
                    response.text()
                );
                match response.json(&path) {
                    Ok(snapshot) => Some(snapshot),
                    Err(error) => panic!("{error}"),
                }
            }
            Err(error) => panic!("{error}"),
        }
    };

    let (Some(none), Some(first), Some(all)) = (
        snapshot_at("none"),
        snapshot_at("first"),
        snapshot_at("all"),
    ) else {
        return;
    };

    let side_of = |snapshot: &Snapshot| -> Side {
        let chain = snapshot
            .chains
            .first()
            .unwrap_or_else(|| panic!("a snapshot must quote an expiration"));
        let contract = chain
            .contracts
            .first()
            .unwrap_or_else(|| panic!("a chain must quote a strike"));
        Side {
            bid: contract.call.bid,
            ask: contract.call.ask,
            mid: contract.call.mid,
            delta: contract.call.delta,
            greeks: contract.call.greeks.clone(),
        }
    };

    // `none` carries no greek block at all; `first` carries the first-order
    // set; `all` adds the second-order one. Anything else is a level that no
    // longer means what it says.
    let none_side = side_of(&none);
    assert!(
        none_side.greeks.is_none(),
        "the none level must carry no greek block, it carried {:?}",
        none_side.greeks
    );

    let first_side = side_of(&first);
    let first_greeks = first_side
        .greeks
        .as_ref()
        .unwrap_or_else(|| panic!("the first level must carry a greek block"));
    assert!(
        first_greeks.alpha.is_none()
            && first_greeks.vanna.is_none()
            && first_greeks.vomma.is_none()
            && first_greeks.veta.is_none()
            && first_greeks.charm.is_none()
            && first_greeks.color.is_none(),
        "the first level must stop at the first-order set, it carried second-order values"
    );
    for (name, value) in [
        ("theta", first_greeks.theta),
        ("vega", first_greeks.vega),
        ("rho", first_greeks.rho),
        ("rho_d", first_greeks.rho_d),
    ] {
        assert!(
            value.is_finite(),
            "the first level must carry a usable {name}, it carried {value}"
        );
    }
    // At this level delta and gamma are NOT repeated inside the block: they
    // already sit on the side and on the contract, and two copies is two
    // places to disagree.
    assert!(
        first_greeks.delta.is_none() && first_greeks.gamma.is_none(),
        "the first level repeats delta or gamma inside the greek block, where the side and the \
         contract already carry them"
    );

    let all_side = side_of(&all);
    let all_greeks = all_side
        .greeks
        .as_ref()
        .unwrap_or_else(|| panic!("the all level must carry a greek block"));
    for (name, value) in [
        ("alpha", all_greeks.alpha),
        ("vanna", all_greeks.vanna),
        ("vomma", all_greeks.vomma),
        ("veta", all_greeks.veta),
        ("charm", all_greeks.charm),
        ("color", all_greeks.color),
    ] {
        assert!(
            value.is_some_and(f64::is_finite),
            "the all level must carry {name}, it carried {value:?}"
        );
    }

    // At `all` the block does repeat them, so the two renderings must agree.
    let block_delta = all_greeks
        .delta
        .unwrap_or_else(|| panic!("the all level must carry delta inside the block"));
    let block_gamma = all_greeks
        .gamma
        .unwrap_or_else(|| panic!("the all level must carry gamma inside the block"));
    assert!(
        (all_side.delta - block_delta).abs() < 1e-12,
        "the side's delta {} and the greek block's {block_delta} disagree at the all level",
        all_side.delta
    );
    let contract_gamma = all
        .chains
        .first()
        .and_then(|chain| chain.contracts.first())
        .and_then(|contract| contract.gamma)
        .unwrap_or_else(|| panic!("the all level must carry gamma on the contract"));
    assert!(
        (contract_gamma - block_gamma).abs() < 1e-12,
        "the contract's gamma {contract_gamma} and the greek block's {block_gamma} disagree"
    );
    assert!(
        (none_side.delta - first_side.delta).abs() < 1e-12,
        "asking for greeks changed the delta itself, {} became {}",
        none_side.delta,
        first_side.delta
    );

    // And the export renders the same number for the same contract. The
    // comparison uses the last SERVED step, since the peeked one is not in
    // the export yet.
    let path = format!(
        "/api/v2/simulations/{}/export?dataset=option_chains&format=csv&greeks=all",
        walk.id
    );
    let export = match client.get(&path) {
        Ok(response) => response,
        Err(error) => panic!("{error}"),
    };
    assert_eq!(export.status, 200, "{}", export.text());
    let text = export.text();
    let mut lines = text.split("\r\n").filter(|line| !line.is_empty());
    let header: Vec<&str> = match lines.next() {
        Some(header) => header.split(',').collect(),
        None => panic!("an export must carry a header"),
    };
    for column in ["strike", "call_delta", "put_delta", "gamma"] {
        assert!(
            header.contains(&column),
            "the all-level export must carry {column}: {header:?}"
        );
    }

    let served = walk
        .snapshots
        .last()
        .unwrap_or_else(|| panic!("the walk must have served a step"));
    let served_contract = served
        .chains
        .first()
        .and_then(|chain| chain.contracts.first())
        .unwrap_or_else(|| panic!("a served snapshot must quote a strike"));

    let strike_column = header.iter().position(|column| *column == "strike");
    let delta_column = header.iter().position(|column| *column == "call_delta");
    let step_column = header.iter().position(|column| *column == "step");
    if let (Some(strike_at), Some(delta_at), Some(step_at)) =
        (strike_column, delta_column, step_column)
    {
        let wanted = served_contract.strike;
        let last_step = (walk.snapshots.len() - 1).to_string();
        let matching = lines
            .map(|line| line.split(',').map(str::to_string).collect::<Vec<String>>())
            .find(|row| {
                row.get(step_at).map(String::as_str) == Some(last_step.as_str())
                    && row
                        .get(strike_at)
                        .and_then(|value| value.parse::<f64>().ok())
                        .is_some_and(|strike| (strike - wanted).abs() < 1e-9)
            });
        match matching {
            Some(row) => {
                let exported = row
                    .get(delta_at)
                    .and_then(|value| value.parse::<f64>().ok())
                    .unwrap_or_else(|| panic!("call_delta must be a number: {row:?}"));
                assert!(
                    (exported - served_contract.call.delta).abs() < 1e-9,
                    "strike {wanted} at step {last_step} has a call delta of {} in the served \
                     snapshot and {exported} in the export",
                    served_contract.call.delta
                );
            }
            None => panic!("the export must carry strike {wanted} at step {last_step}"),
        }
    }
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
/// The two contracts are chosen by their MID, not by the spread being
/// compared: selecting them by the quantity under test would make the
/// assertion true by construction, and a constant spread would pass it.
///
/// Skipped on a deployment that predates the model, which refuses the fields
/// rather than ignoring them.
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

    // Every two-sided quote in the walk, as (mid, relative spread).
    let mut quotes: Vec<(f64, f64, String)> = Vec::new();
    for (step, snapshot) in walk.snapshots.iter().enumerate() {
        for chain in &snapshot.chains {
            for contract in &chain.contracts {
                for (name, side) in [("call", &contract.call), ("put", &contract.put)] {
                    let (Some(bid), Some(mid), Some(ask)) = (side.bid, side.mid, side.ask) else {
                        continue;
                    };
                    if mid <= 0.0 {
                        continue;
                    }
                    quotes.push((
                        mid,
                        (ask - bid) / mid,
                        format!("step {step} strike {} {name}", contract.strike),
                    ));
                }
            }
        }
    }

    assert!(
        quotes.len() >= 2,
        "the walk must quote at least two contracts to compare, it quoted {}",
        quotes.len()
    );

    // Cheapest and dearest by mid, chosen before any spread is looked at.
    quotes.sort_by(|left, right| left.0.total_cmp(&right.0));
    let (cheap_mid, cheap_spread, cheap_where) = quotes[0].clone();
    let (dear_mid, dear_spread, dear_where) = match quotes.last() {
        Some(quote) => quote.clone(),
        None => unreachable!("quotes was checked non-empty"),
    };

    assert!(
        dear_mid > cheap_mid,
        "the walk must quote two different prices to compare, everything sat at {cheap_mid}"
    );

    // The model is a floor plus a proportion of the mid, so the ABSOLUTE
    // spread grows with the mid while the RELATIVE one shrinks. Both are
    // asserted, because either alone can be satisfied by a constant.
    assert!(
        cheap_spread > dear_spread,
        "a cheap contract must spread relatively wider: {cheap_where} at a mid of {cheap_mid} \
         spreads {cheap_spread}, {dear_where} at {dear_mid} spreads {dear_spread}"
    );
    assert!(
        cheap_spread * cheap_mid <= dear_spread * dear_mid + 1e-9,
        "the absolute spread must not shrink as the mid grows: {cheap_where} spreads {} \
         absolutely, {dear_where} spreads {}",
        cheap_spread * cheap_mid,
        dear_spread * dear_mid
    );

    println!(
        "INFO: relative spread {dear_spread:.4} at a mid of {dear_mid:.2} against \
         {cheap_spread:.4} at {cheap_mid:.2}"
    );
}
