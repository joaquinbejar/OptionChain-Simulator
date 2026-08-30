//! A checked-in tape for every walk kernel, so a change in the values they
//! produce cannot land quietly.
//!
//! Every other reproducibility test in this crate compares two runs of the
//! SAME build: they prove the walker is deterministic, and they are
//! structurally blind to a kernel that starts returning different numbers.
//! The optionstratlib 0.21 re-sync was exactly that class of change, and
//! nothing in CI would have noticed if it had moved a price.
//!
//! This module closes that hole. It walks all nine [`WalkType`] variants plus
//! the four `*_with_vol` variants at a fixed seed and a fixed size, and
//! compares the result against `fixtures/walk_kernels.json`.
//!
//! # Regenerating the fixture
//!
//! Regenerating is a DELIBERATE, TAPE-BREAKING act, never a way to make a red
//! test green. Every tape any consumer has recorded under a given seed becomes
//! incomparable with the ones this build produces: IronCondor gates a
//! milestone on the seed-reproducibility contract, so a regeneration has to be
//! announced, and the snapshot generation
//! (`infrastructure::clickhouse::snapshots::record::CURRENT_SNAPSHOT_GENERATION`)
//! bumped so a stored tape cannot be mistaken for a comparable one.
//!
//! When it really is intended — an upstream kernel fix this crate deliberately
//! takes — run:
//!
//! ```text
//! cargo test --lib domain::golden_tape::regenerate -- --ignored --nocapture
//! ```
//!
//! and read the diff on the fixture: it is the exact list of what moved.

use crate::domain::Walker;
use optionstratlib::chains::OptionChain;
use optionstratlib::simulation::{WalkParams, WalkType, WalkTypeAble};
use positive::{Positive, pos_or_panic};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::BTreeMap;

/// The seed every kernel in the fixture is walked under.
const FIXTURE_SEED: u64 = 7;

/// How many steps each fixture path carries.
const FIXTURE_SIZE: usize = 30;

/// The checked-in expectation.
const FIXTURE: &str = include_str!("fixtures/walk_kernels.json");

/// Where `regenerate` writes, relative to the crate root.
const FIXTURE_PATH: &str = "src/domain/fixtures/walk_kernels.json";

/// Builds walk parameters over a real chain, so the kernels run exactly as
/// they do in a simulation rather than against a synthetic Ystep.
fn params_with(walk_type: WalkType) -> WalkParams<Positive, OptionChain> {
    use optionstratlib::ExpirationDate;
    use optionstratlib::chains::OptionChainBuildParams;
    use optionstratlib::chains::utils::OptionDataPriceParams;
    use optionstratlib::simulation::steps::{Step, Xstep, Ystep};
    use optionstratlib::utils::TimeFrame;

    let days = pos_or_panic!(30.0);
    let symbol = "TEST".to_string();
    let price_params = OptionDataPriceParams::new(
        Some(Box::new(pos_or_panic!(100.0))),
        Some(ExpirationDate::Days(days)),
        Some(Decimal::ZERO),
        Some(Positive::ZERO),
        Some(symbol.clone()),
    );
    let build_params = OptionChainBuildParams::new(
        symbol,
        Some(Positive::ONE),
        10,
        Some(pos_or_panic!(5.0)),
        dec!(-0.2),
        dec!(0.5),
        pos_or_panic!(0.01),
        2,
        price_params,
        pos_or_panic!(0.2),
    );

    let chain = match OptionChain::build_chain(&build_params) {
        Ok(chain) => chain,
        Err(error) => panic!("the fixture chain must build: {error}"),
    };

    WalkParams {
        size: FIXTURE_SIZE,
        init_step: Step {
            x: Xstep::new(Positive::ONE, TimeFrame::Day, ExpirationDate::Days(days)),
            y: Ystep::new(0, chain),
        },
        walk_type,
        walker: Box::new(Walker::new_with_seed(FIXTURE_SEED)),
    }
}

/// Every walk type the API exposes, with the parameters the fixture pins.
fn every_walk_type() -> Vec<(&'static str, WalkType)> {
    let dt = pos_or_panic!(1.0 / 252.0);
    vec![
        (
            "brownian",
            WalkType::Brownian {
                dt,
                drift: dec!(0.05),
                volatility: pos_or_panic!(0.2),
            },
        ),
        (
            "geometric_brownian",
            WalkType::GeometricBrownian {
                dt,
                drift: dec!(0.05),
                volatility: pos_or_panic!(0.2),
            },
        ),
        (
            "log_returns",
            WalkType::LogReturns {
                dt,
                expected_return: dec!(0.05),
                volatility: pos_or_panic!(0.2),
                autocorrelation: Some(dec!(0.3)),
            },
        ),
        (
            "mean_reverting",
            WalkType::MeanReverting {
                dt,
                volatility: pos_or_panic!(0.2),
                speed: pos_or_panic!(1.5),
                mean: pos_or_panic!(100.0),
            },
        ),
        (
            "jump_diffusion",
            WalkType::JumpDiffusion {
                dt,
                drift: dec!(0.05),
                volatility: pos_or_panic!(0.2),
                intensity: pos_or_panic!(5.0),
                jump_mean: dec!(0.1),
                jump_volatility: pos_or_panic!(0.1),
            },
        ),
        (
            "garch",
            WalkType::Garch {
                dt,
                drift: dec!(0.05),
                volatility: pos_or_panic!(0.2),
                alpha: pos_or_panic!(0.1),
                beta: pos_or_panic!(0.8),
            },
        ),
        (
            "heston",
            WalkType::Heston {
                dt,
                drift: dec!(0.05),
                volatility: pos_or_panic!(0.2),
                kappa: pos_or_panic!(1.5),
                theta: pos_or_panic!(0.04),
                xi: pos_or_panic!(0.3),
                rho: dec!(-0.7),
            },
        ),
        (
            "custom",
            WalkType::Custom {
                dt,
                drift: dec!(0.05),
                volatility: pos_or_panic!(0.2),
                vov: pos_or_panic!(0.3),
                vol_speed: pos_or_panic!(1.5),
                vol_mean: pos_or_panic!(0.2),
            },
        ),
        (
            "telegraph",
            WalkType::Telegraph {
                dt,
                drift: dec!(0.05),
                volatility: pos_or_panic!(0.2),
                lambda_up: pos_or_panic!(2.0),
                lambda_down: pos_or_panic!(3.0),
                vol_multiplier_up: Some(pos_or_panic!(1.2)),
                vol_multiplier_down: Some(pos_or_panic!(0.8)),
            },
        ),
    ]
}

/// Renders one value exactly, as a decimal string: a float would round away
/// precisely the digits a kernel change is most likely to move.
fn render(values: &[Positive]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

/// Walks every kernel under [`FIXTURE_SEED`] and returns what it produced,
/// keyed the way the fixture is keyed.
fn walk_everything() -> BTreeMap<String, Vec<String>> {
    let mut produced = BTreeMap::new();

    for (name, walk_type) in every_walk_type() {
        let params = params_with(walk_type.clone());
        let walker = Walker::new_with_seed(FIXTURE_SEED);

        let prices = match &walk_type {
            WalkType::Brownian { .. } => walker.brownian(&params),
            WalkType::GeometricBrownian { .. } => walker.geometric_brownian(&params),
            WalkType::LogReturns { .. } => walker.log_returns(&params),
            WalkType::MeanReverting { .. } => walker.mean_reverting(&params),
            WalkType::JumpDiffusion { .. } => walker.jump_diffusion(&params),
            WalkType::Garch { .. } => walker.garch(&params),
            WalkType::Heston { .. } => walker.heston(&params),
            WalkType::Custom { .. } => walker.custom(&params),
            WalkType::Telegraph { .. } => walker.telegraph(&params),
            other => panic!("the fixture must cover every walk type, missing {other:?}"),
        };
        match prices {
            Ok(prices) => produced.insert(name.to_string(), render(&prices)),
            Err(error) => panic!("{name} must walk: {error}"),
        };

        // The `*_with_vol` variants are what the generators actually call, so
        // they carry their own entries: prices and the volatility path that
        // drove them.
        let walker = Walker::new_with_seed(FIXTURE_SEED);
        let with_vol = match &walk_type {
            WalkType::Garch { .. } => Some(walker.garch_with_vol(&params)),
            WalkType::Heston { .. } => Some(walker.heston_with_vol(&params)),
            WalkType::Custom { .. } => Some(walker.custom_with_vol(&params)),
            WalkType::Telegraph { .. } => Some(walker.telegraph_with_vol(&params)),
            _ => None,
        };
        if let Some(path) = with_vol {
            match path {
                Ok(path) => {
                    produced.insert(format!("{name}_with_vol.prices"), render(&path.prices));
                    let vols = match path.vols {
                        Some(vols) => render(&vols),
                        None => Vec::new(),
                    };
                    produced.insert(format!("{name}_with_vol.vols"), vols);
                }
                Err(error) => panic!("{name}_with_vol must walk: {error}"),
            }
        }
    }

    produced
}

/// Every kernel still produces the values it produced when the fixture was
/// written.
///
/// A failure here is NOT a flaky test and never a reason to regenerate: it
/// means a walk path moved, so every seeded tape recorded before this build
/// disagrees with the ones it produces. See the module documentation.
#[test]
fn test_every_walk_kernel_matches_the_committed_tape() {
    let expected: BTreeMap<String, Vec<String>> = match serde_json::from_str(FIXTURE) {
        Ok(expected) => expected,
        Err(error) => panic!("the committed fixture must parse: {error}"),
    };
    let produced = walk_everything();

    for (name, expected_path) in &expected {
        match produced.get(name) {
            Some(path) if path == expected_path => {}
            Some(path) => {
                let step = path
                    .iter()
                    .zip(expected_path)
                    .position(|(produced, expected)| produced != expected);
                panic!(
                    "the {name} kernel no longer produces the committed tape, first difference \
                     at step {step:?}: produced {path:?}, committed {expected_path:?}"
                );
            }
            None => panic!("the {name} kernel produced nothing; the fixture expects a path"),
        }
    }

    for name in produced.keys() {
        assert!(
            expected.contains_key(name),
            "{name} is walked but not committed; regenerate the fixture deliberately"
        );
    }
}

/// Rewrites the fixture. Ignored by default: see the module documentation for
/// what regenerating means for anyone holding a recorded tape.
#[test]
#[ignore = "regenerating the walk fixture is a deliberate, tape-breaking act"]
fn regenerate_the_walk_kernel_fixture() {
    let produced = walk_everything();
    let json = match serde_json::to_string_pretty(&produced) {
        Ok(json) => json,
        Err(error) => panic!("the fixture must serialize: {error}"),
    };
    match std::fs::write(FIXTURE_PATH, format!("{json}\n")) {
        Ok(()) => println!("wrote {} entries to {FIXTURE_PATH}", produced.len()),
        Err(error) => panic!("the fixture must be writable at {FIXTURE_PATH}: {error}"),
    }
}
