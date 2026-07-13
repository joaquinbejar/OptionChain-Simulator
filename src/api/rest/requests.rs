use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
use crate::api::rest::patch::Patch;
use crate::session::SimulationParameters;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::fmt;
use utoipa::ToSchema;

/// Represents a request to create a new simulation session.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    /// - `symbol` (`String`): The name or ticker symbol of the asset being simulated.
    pub symbol: String,
    /// - `steps` (`usize`): The number of discrete time steps or intervals in the simulation process.
    pub steps: usize,
    /// - `initial_price` (`Positive`): The initial starting price of the asset. This must be a positive value.
    pub initial_price: f64,
    /// - `days_to_expiration` (`Positive`): The number of days until the expiration of the asset or contract. This must be a positive value.
    pub days_to_expiration: f64,
    /// - `volatility` (`Positive`): The expected volatility (standard deviation) of the asset's returns.
    pub volatility: f64,
    /// - `risk_free_rate` (`Decimal`): The risk-free rate of return, typically represented as an annualized percentage.
    pub risk_free_rate: f64,
    /// - `dividend_yield` (`Positive`): The annualized dividend yield of the asset, expressed as a positive value.
    pub dividend_yield: f64,
    /// - `method` (`SimulationMethod`): The simulation method or algorithm to be used, defining the behavior of the simulation process.
    pub method: ApiWalkType,
    /// - `time_frame` (`TimeFrame`): The time frame for the simulation intervals, such as daily, weekly, or hourly.
    pub time_frame: ApiTimeFrame,
    /// - `chain_size` (`Option<usize>`): The optional size of the option chain being simulated. If `None`, this is not specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_size: Option<usize>,
    /// - `strike_interval` (`Option<Positive>`): The optional interval between strike prices for options. If `None`, this is not specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strike_interval: Option<f64>,
    /// - `skew_slope` (`Option<Decimal>`): An optional factor that adjusts the slope of the volatility distribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skew_slope: Option<f64>,
    /// - `smile_curve` (`Option<Decimal>`): An optional factor that adjusts the skew of the volatility distribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smile_curve: Option<f64>,
    /// - `spread` (`Option<Positive>`): An optional parameter to specify the spread value. If `None`, no spread is applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread: Option<f64>,
    /// - `seed` (`Option<u64>`): An optional RNG seed that makes the session reproducible.
    ///   Two sessions created with identical parameters and the same seed produce the same
    ///   sequence of option chain snapshots. If `None`, a random seed is generated; the
    ///   effective seed is reported back in the session response so clients can record it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

impl From<SimulationParameters> for CreateSessionRequest {
    fn from(params: SimulationParameters) -> Self {
        Self {
            symbol: params.symbol,
            steps: params.steps,
            initial_price: params.initial_price.to_f64(),
            days_to_expiration: params.days_to_expiration.to_f64(),
            volatility: params.volatility.to_f64(),
            risk_free_rate: params.risk_free_rate.to_f64().unwrap_or(0.0),
            dividend_yield: params.dividend_yield.to_f64(),
            method: params.method.into(),
            time_frame: params.time_frame.into(),
            chain_size: params.chain_size,
            strike_interval: params.strike_interval.map(|p| p.to_f64()),
            skew_slope: params.skew_slope.map(|d| d.to_f64().unwrap_or(0.0)),
            smile_curve: params.smile_curve.map(|d| d.to_f64().unwrap_or(0.0)),
            spread: params.spread.map(|p| p.to_f64()),
            seed: params.seed,
        }
    }
}

impl Default for CreateSessionRequest {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            steps: 20,
            initial_price: 100.0,
            days_to_expiration: 30.0,
            volatility: 0.2,
            risk_free_rate: 0.0,
            dividend_yield: 0.0,
            method: ApiWalkType::Brownian {
                dt: 1.0 / 252.0,
                drift: 0.0,
                volatility: 0.2,
            },
            time_frame: ApiTimeFrame::Day,
            chain_size: Some(30),
            strike_interval: Some(1.0),
            skew_slope: None,
            smile_curve: None,
            spread: Some(0.01),
            seed: None,
        }
    }
}

impl fmt::Display for CreateSessionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Serialize to JSON, map any error to fmt::Error
        let json = serde_json::to_string(self).map_err(|_| fmt::Error)?;
        write!(f, "{}", json)
    }
}

/// Represents a request to update an existing simulation session.
/// This is a partial update, so all fields are optional.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateSessionRequest {
    /// - `symbol` (`String`): The name or ticker symbol of the asset being simulated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// - `steps` (`usize`): The number of discrete time steps or intervals in the simulation process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<usize>,
    /// - `initial_price` (`Positive`): The initial starting price of the asset. This must be a positive value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_price: Option<f64>,
    /// - `days_to_expiration` (`Positive`): The number of days until the expiration of the asset or contract. This must be a positive value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_to_expiration: Option<f64>,
    /// - `volatility` (`Positive`): The expected volatility (standard deviation) of the asset's returns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volatility: Option<f64>,
    /// - `risk_free_rate` (`Decimal`): The risk-free rate of return, typically represented as an annualized percentage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_free_rate: Option<f64>,
    /// - `dividend_yield` (`Positive`): The annualized dividend yield of the asset, expressed as a positive value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dividend_yield: Option<f64>,
    /// - `method` (`SimulationMethod`): The simulation method or algorithm to be used, defining the behavior of the simulation process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<ApiWalkType>,
    /// - `time_frame` (`TimeFrame`): The time frame for the simulation intervals, such as daily, weekly, or hourly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_frame: Option<ApiTimeFrame>,
    /// - `chain_size` (`Option<usize>`): The size of the option chain being simulated.
    ///   Tri-state: **absent** keeps the current value, **null** clears it, a **value** replaces it.
    #[serde(default, skip_serializing_if = "Patch::is_absent")]
    #[schema(value_type = Option<usize>)]
    pub chain_size: Patch<usize>,
    /// - `strike_interval` (`Option<Positive>`): The interval between strike prices for options.
    ///   Tri-state: **absent** keeps the current value, **null** clears it, a **value** replaces it.
    #[serde(default, skip_serializing_if = "Patch::is_absent")]
    #[schema(value_type = Option<f64>)]
    pub strike_interval: Patch<f64>,
    /// - `skew_slope` (`Option<Decimal>`): A factor that adjusts the slope of the volatility distribution.
    ///   Tri-state: **absent** keeps the current value, **null** clears it, a **value** replaces it.
    #[serde(default, skip_serializing_if = "Patch::is_absent")]
    #[schema(value_type = Option<f64>)]
    pub skew_slope: Patch<f64>,
    /// - `smile_curve` (`Option<Decimal>`): A factor that adjusts the skew of the volatility distribution.
    ///   Tri-state: **absent** keeps the current value, **null** clears it, a **value** replaces it.
    #[serde(default, skip_serializing_if = "Patch::is_absent")]
    #[schema(value_type = Option<f64>)]
    pub smile_curve: Patch<f64>,
    /// - `spread` (`Option<Positive>`): The bid-ask spread factor.
    ///   Tri-state: **absent** keeps the current value, **null** clears it, a **value** replaces it.
    #[serde(default, skip_serializing_if = "Patch::is_absent")]
    #[schema(value_type = Option<f64>)]
    pub spread: Patch<f64>,
    /// - `seed` (`Option<u64>`): The RNG seed driving the session's stochastic walk.
    ///   Tri-state: **absent** keeps the current seed, a **value** replaces it, and **null**
    ///   re-seeds the session with a fresh random seed. The effective seed is never cleared —
    ///   `SimulationParameters.seed` stays `Some` so the session remains reproducible and the
    ///   seed is always reported back in the session response.
    #[serde(default, skip_serializing_if = "Patch::is_absent")]
    #[schema(value_type = Option<u64>)]
    pub seed: Patch<u64>,
}

impl fmt::Display for UpdateSessionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Serialize to JSON, map any error to fmt::Error
        let json = serde_json::to_string(self).map_err(|_| fmt::Error)?;
        write!(f, "{}", json)
    }
}

#[cfg(test)]
mod tests_create_session_request {
    use super::*;
    use serde_json::{from_str, to_string};

    #[test]
    fn test_create_session_request_default() {
        let default_req = CreateSessionRequest::default();

        assert_eq!(default_req.symbol, "");
        assert_eq!(default_req.initial_price, 100.0);
        assert_eq!(default_req.volatility, 0.2);
        assert_eq!(default_req.risk_free_rate, 0.0);
        assert_eq!(default_req.days_to_expiration, 30.0);
        assert_eq!(default_req.dividend_yield, 0.0);
        assert_eq!(default_req.steps, 20);
        assert_eq!(default_req.time_frame, ApiTimeFrame::Day);
        assert_eq!(default_req.chain_size, Some(30));
        assert_eq!(default_req.strike_interval, Some(1.0));
        assert_eq!(default_req.smile_curve, None);
        assert_eq!(default_req.spread, Some(0.01));

        // Check method field
        match default_req.method {
            ApiWalkType::Brownian {
                dt,
                drift,
                volatility,
            } => {
                assert!((dt - 1.0 / 252.0).abs() < 1e-6);
                assert_eq!(drift, 0.0);
                assert_eq!(volatility, 0.2);
            }
            _ => panic!("Expected default method to be Brownian"),
        }
    }

    #[test]
    fn test_create_session_request_serialization() {
        let req = CreateSessionRequest {
            symbol: "AAPL".to_string(),
            initial_price: 185.5,
            volatility: 0.25,
            risk_free_rate: 0.04,
            days_to_expiration: 45.0,
            method: ApiWalkType::GeometricBrownian {
                dt: 0.004,
                drift: 0.05,
                volatility: 0.25,
            },
            steps: 30,
            time_frame: ApiTimeFrame::Day,
            dividend_yield: 0.005,
            skew_slope: Some(-0.2),
            smile_curve: Some(0.5),
            spread: Some(0.02),
            seed: None,
            chain_size: Some(15),
            strike_interval: Some(5.0),
        };

        let json = to_string(&req).unwrap();

        // Check that the JSON contains expected fields
        assert!(json.contains("\"symbol\":\"AAPL\""));
        assert!(json.contains("\"initial_price\":185.5"));
        assert!(json.contains("\"volatility\":0.25"));
        assert!(json.contains("\"risk_free_rate\":0.04"));
        assert!(json.contains("\"days_to_expiration\":45.0"));
        assert!(json.contains("\"steps\":30"));
        assert!(json.contains("\"dividend_yield\":0.005"));
        assert!(json.contains("\"smile_curve\":0.5"));
        assert!(json.contains("\"spread\":0.02"));
        assert!(json.contains("\"chain_size\":15"));
        assert!(json.contains("\"strike_interval\":5.0"));

        // Check method field
        assert!(json.contains("\"GeometricBrownian\""));
        assert!(json.contains("\"dt\":0.004"));
        assert!(json.contains("\"drift\":0.05"));

        // Check time_frame field
        assert!(json.contains("\"time_frame\":\"Day\""));
    }

    #[test]
    fn test_create_session_request_deserialization() {
        let json = r#"{
            "symbol": "TSLA",
            "initial_price": 250.0,
            "volatility": 0.4,
            "risk_free_rate": 0.035,
            "days_to_expiration": 60.0,
            "method": {
                "Brownian": {
                    "dt": 0.0027,
                    "drift": 0.02,
                    "volatility": 0.4
                }
            },
            "steps": 25,
            "time_frame": "Week",
            "dividend_yield": 0.0,
            "smile_curve": 0.001,
            "spread": 0.015,
            "chain_size": 20,
            "strike_interval": 10.0
        }"#;

        let req: CreateSessionRequest = from_str(json).unwrap();

        assert_eq!(req.symbol, "TSLA");
        assert_eq!(req.initial_price, 250.0);
        assert_eq!(req.volatility, 0.4);
        assert_eq!(req.risk_free_rate, 0.035);
        assert_eq!(req.days_to_expiration, 60.0);
        assert_eq!(req.steps, 25);
        assert_eq!(req.time_frame, ApiTimeFrame::Week);
        assert_eq!(req.dividend_yield, 0.0);
        assert_eq!(req.smile_curve, Some(0.001));
        assert_eq!(req.spread, Some(0.015));
        assert_eq!(req.chain_size, Some(20));
        assert_eq!(req.strike_interval, Some(10.0));

        // Check method field
        match req.method {
            ApiWalkType::Brownian {
                dt,
                drift,
                volatility,
            } => {
                assert_eq!(dt, 0.0027);
                assert_eq!(drift, 0.02);
                assert_eq!(volatility, 0.4);
            }
            _ => panic!("Expected method to be Brownian"),
        }
    }

    #[test]
    fn test_partial_updates_create_session_request() {
        // Test with partial JSON (missing fields should use defaults)
        let json = r#"{
            "symbol": "AAPL",
            "steps": 30,
            "initial_price": 150.0,
            "volatility": 0.2,
            "risk_free_rate": 0.03,
            "dividend_yield": 0.005,
            "days_to_expiration": 30.0,
            "method": {
              "GeometricBrownian": {
                "dt": 0.004,
                "drift": 0.05,
                "volatility": 0.25
              }
            },
            "time_frame": "Day"
        }"#;

        let req: CreateSessionRequest = from_str(json).unwrap();

        // Provided fields
        assert_eq!(req.symbol, "AAPL");
        assert_eq!(req.initial_price, 150.0);
        assert_eq!(req.volatility, 0.2);
        assert_eq!(req.risk_free_rate, 0.03);
        assert_eq!(req.days_to_expiration, 30.0);

        // Default fields
        assert_eq!(req.steps, 30); // Default value
        assert_eq!(req.time_frame, ApiTimeFrame::Day); // Default value
        assert_eq!(req.dividend_yield, 0.005); // Default value
        assert_eq!(req.smile_curve, None); // Default value
        assert_eq!(req.spread, None); // Default value

        // Method field should be default Brownian
        match req.method {
            ApiWalkType::GeometricBrownian {
                dt,
                drift,
                volatility,
            } => {
                assert!((dt - 0.004).abs() < 1e-6);
                assert_eq!(drift, 0.05);
                assert_eq!(volatility, 0.25);
            }
            _ => panic!("Expected default method to be Brownian"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use optionstratlib::{chains::OptionChain, simulation::WalkType};
    use positive::{Positive, pos_or_panic};
    use rust_decimal::Decimal;
    use uuid::Uuid;

    // Session Management Tests
    mod session_tests {
        use super::*;
        use crate::session::{
            InMemorySessionStore, SessionManager, SessionState, SimulationParameters,
        };
        use optionstratlib::utils::TimeFrame;
        use std::sync::Arc;

        #[tokio::test]
        async fn test_session_creation() {
            let store = Arc::new(InMemorySessionStore::new());
            let session_manager = SessionManager::new(store);

            let params = SimulationParameters {
                symbol: "AAPL".to_string(),
                steps: 10,
                initial_price: pos_or_panic!(100.0),
                days_to_expiration: pos_or_panic!(30.0),
                volatility: pos_or_panic!(0.2),
                risk_free_rate: Decimal::ZERO,
                dividend_yield: Positive::ZERO,
                method: WalkType::Brownian {
                    dt: pos_or_panic!(1.0 / 252.0),
                    drift: Decimal::ZERO,
                    volatility: pos_or_panic!(0.2),
                },
                time_frame: TimeFrame::Day,
                chain_size: Some(15),
                strike_interval: Some(pos_or_panic!(1.0)),
                skew_slope: None,
                smile_curve: None,
                spread: Some(pos_or_panic!(0.02)),
                seed: None,
            };

            let session = session_manager
                .create_session(params.clone())
                .expect("Session creation failed");

            assert_eq!(session.parameters.symbol, "AAPL");
            assert_eq!(session.state, SessionState::Initialized);
            assert_eq!(session.current_step, 0);
            assert_eq!(session.total_steps, params.steps);
        }

        #[tokio::test]
        async fn test_session_advancement() {
            let store = Arc::new(InMemorySessionStore::new());
            let session_manager = SessionManager::new(store);

            let params = SimulationParameters {
                symbol: "AAPL".to_string(),
                steps: 5,
                initial_price: pos_or_panic!(100.0),
                days_to_expiration: pos_or_panic!(30.0),
                volatility: pos_or_panic!(0.2),
                risk_free_rate: Decimal::ZERO,
                dividend_yield: Positive::ZERO,
                method: WalkType::Brownian {
                    dt: pos_or_panic!(1.0 / 252.0),
                    drift: Decimal::ZERO,
                    volatility: pos_or_panic!(0.2),
                },
                time_frame: TimeFrame::Day,
                chain_size: Some(15),
                strike_interval: Some(pos_or_panic!(1.0)),
                skew_slope: None,
                smile_curve: None,
                spread: Some(pos_or_panic!(0.02)),
                seed: None,
            };

            let session = session_manager
                .create_session(params)
                .expect("Session creation failed");

            // Advance through steps
            for step in 0..4 {
                let (advanced_session, _chain) = session_manager
                    .get_next_step(session.id)
                    .await
                    .expect("Step advancement failed");

                assert_eq!(advanced_session.current_step, step + 1);
                assert_eq!(advanced_session.state, SessionState::InProgress);
            }
        }
    }

    // Simulation Method Tests
    mod simulation_method_tests {
        use super::*;
        use crate::domain::Simulator;
        use crate::session::Session;
        use crate::utils::UuidGenerator;
        use optionstratlib::utils::{Len, TimeFrame};

        #[tokio::test]
        async fn test_geometric_brownian_simulation() {
            let simulator = Simulator::new();

            let params = SimulationParameters {
                symbol: "AAPL".to_string(),
                steps: 10,
                initial_price: pos_or_panic!(100.0),
                days_to_expiration: pos_or_panic!(30.0),
                volatility: pos_or_panic!(0.2),
                risk_free_rate: Decimal::new(3, 2), // 3%
                dividend_yield: Positive::ZERO,
                method: WalkType::GeometricBrownian {
                    dt: pos_or_panic!(1.0 / 252.0),
                    drift: Decimal::new(5, 2), // 5%
                    volatility: pos_or_panic!(0.2),
                },
                time_frame: TimeFrame::Day,
                chain_size: Some(15),
                strike_interval: Some(pos_or_panic!(1.0)),
                skew_slope: None,
                smile_curve: None,
                spread: Some(pos_or_panic!(0.02)),
                seed: None,
            };

            let session = Session::new(params, &UuidGenerator::new(Uuid::new_v4()));

            let option_chain = simulator
                .simulate_next_step(&session)
                .await
                .expect("Simulation step failed");

            assert_eq!(option_chain.symbol, "AAPL");
            assert!(option_chain.len() > 0);
            assert!(option_chain.underlying_price > Positive::ZERO);
        }
    }

    // Option Chain Generation Tests
    mod option_chain_tests {
        use super::*;
        use optionstratlib::ExpirationDate;
        use optionstratlib::chains::OptionChainBuildParams;
        use optionstratlib::chains::utils::OptionDataPriceParams;
        use positive::spos;
        use rust_decimal_macros::dec;

        #[test]
        fn test_option_chain_generation() {
            let initial_price = pos_or_panic!(100.0);
            let expiration = ExpirationDate::Days(pos_or_panic!(30.0));

            let chain_params = OptionChainBuildParams::new(
                "AAPL".to_string(),
                Some(pos_or_panic!(1000.0)), // Volume
                15,                          // Chain size
                spos!(1.0),                  // Strike interval
                dec!(-0.2),                  // Skew slope
                Decimal::new(5, 1),          // Skew curve
                pos_or_panic!(0.02),         // Spread
                2,                           // Decimal places
                OptionDataPriceParams::new(
                    Some(Box::new(initial_price)),
                    Some(expiration),
                    Some(Decimal::ZERO),  // Risk-free rate
                    Some(Positive::ZERO), // Dividend yield
                    Some("AAPL".to_string()),
                ),
                pos_or_panic!(0.2), // Implied volatility
            );

            let option_chain =
                OptionChain::build_chain(&chain_params).expect("Failed to build option chain");

            assert_eq!(option_chain.symbol, "AAPL");
            assert_eq!(option_chain.underlying_price, initial_price);
        }
    }

    // Error Handling Tests
    mod error_handling_tests {
        use super::*;
        use crate::session::{InMemorySessionStore, Session, SessionManager};
        use crate::utils::{ChainError, UuidGenerator};
        use optionstratlib::utils::TimeFrame;
        use std::sync::Arc;

        #[tokio::test]
        async fn test_invalid_session_advancement() {
            let store = Arc::new(InMemorySessionStore::new());
            let session_manager = SessionManager::new(store);

            // Use a non-existent UUID
            let non_existent_id = Uuid::new_v4();

            let result = session_manager.get_next_step(non_existent_id).await;

            assert!(matches!(result, Err(ChainError::NotFound(_))));
        }

        #[test]
        fn test_invalid_simulation_parameters() {
            let invalid_params = SimulationParameters {
                symbol: "".to_string(),            // Invalid: empty symbol
                steps: 0,                          // Invalid: zero steps
                initial_price: pos_or_panic!(0.0), // Invalid: zero initial price
                days_to_expiration: Default::default(),
                volatility: Default::default(),
                risk_free_rate: Default::default(),
                dividend_yield: Default::default(),
                method: WalkType::Brownian {
                    dt: Default::default(),
                    drift: Default::default(),
                    volatility: Default::default(),
                },
                time_frame: TimeFrame::Microsecond,
                chain_size: None,
                strike_interval: None,
                skew_slope: None,
                smile_curve: None,
                spread: None,
                seed: None,
            };

            let result = Session::new(invalid_params, &UuidGenerator::new(Uuid::new_v4()));

            // Depending on your validation logic, this might panic or return an error
            assert!(result.parameters.symbol.is_empty());
            assert_eq!(result.parameters.steps, 0);
        }
    }

    // Infrastructure Tests
    mod infrastructure_tests {
        use crate::infrastructure::{ClickHouseConfig, RedisConfig};

        #[test]
        fn test_redis_configuration() {
            let config = RedisConfig::default();

            assert!(!config.host.is_empty());
            assert_ne!(config.port, 0);
        }

        #[test]
        fn test_clickhouse_configuration() {
            let config = ClickHouseConfig::default();

            assert!(!config.host.is_empty());
            assert_ne!(config.port, 0);
            assert!(!config.username.is_empty());
        }
    }

    // API Request Validation Tests
    mod api_request_tests {

        use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
        use crate::api::rest::requests::{CreateSessionRequest, UpdateSessionRequest};

        #[test]
        fn test_create_session_request_validation() {
            let req = CreateSessionRequest {
                symbol: "AAPL".to_string(),
                initial_price: 185.5,
                volatility: 0.25,
                risk_free_rate: 0.04,
                days_to_expiration: 45.0,
                method: ApiWalkType::GeometricBrownian {
                    dt: 0.004,
                    drift: 0.05,
                    volatility: 0.25,
                },
                steps: 30,
                time_frame: ApiTimeFrame::Day,
                dividend_yield: 0.005,
                ..Default::default()
            };

            // Validate required fields
            assert_eq!(req.symbol, "AAPL");
            assert_eq!(req.initial_price, 185.5);
            assert_eq!(req.volatility, 0.25);
        }

        #[test]
        fn test_update_session_request_partial_update() {
            use crate::api::rest::patch::Patch;

            let update_req = UpdateSessionRequest {
                symbol: Some("GOOGL".to_string()),
                steps: None,
                initial_price: None,
                days_to_expiration: None,
                volatility: Some(0.3),
                risk_free_rate: None,
                dividend_yield: None,
                method: None,
                time_frame: None,
                chain_size: Patch::Absent,
                strike_interval: Patch::Absent,
                skew_slope: Patch::Absent,
                smile_curve: Patch::Absent,
                spread: Patch::Absent,
                seed: Patch::Absent,
            };

            assert_eq!(update_req.symbol, Some("GOOGL".to_string()));
            assert_eq!(update_req.volatility, Some(0.3));
            assert!(update_req.initial_price.is_none());
        }
    }
}

#[cfg(test)]
mod tests_update_session_request_tristate {
    use super::*;
    use crate::api::rest::patch::Patch;
    use serde_json::{from_str, json, to_value};

    #[test]
    fn test_deserialize_absent_optional_fields_are_absent() {
        // A body that touches only a required field leaves every optional
        // (tri-state) field Absent — meaning "keep the current value".
        let json = r#"{ "volatility": 0.3 }"#;
        let req: UpdateSessionRequest = from_str(json).expect("partial body deserializes");

        assert_eq!(req.volatility, Some(0.3));
        assert_eq!(req.chain_size, Patch::Absent);
        assert_eq!(req.strike_interval, Patch::Absent);
        assert_eq!(req.skew_slope, Patch::Absent);
        assert_eq!(req.smile_curve, Patch::Absent);
        assert_eq!(req.spread, Patch::Absent);
        assert_eq!(req.seed, Patch::Absent);
    }

    #[test]
    fn test_deserialize_explicit_null_optional_fields_are_null() {
        // Explicit JSON null on each optional field is the "clear" signal.
        let json = r#"{
            "chain_size": null,
            "strike_interval": null,
            "skew_slope": null,
            "smile_curve": null,
            "spread": null,
            "seed": null
        }"#;
        let req: UpdateSessionRequest = from_str(json).expect("null body deserializes");

        assert_eq!(req.chain_size, Patch::Null);
        assert_eq!(req.strike_interval, Patch::Null);
        assert_eq!(req.skew_slope, Patch::Null);
        assert_eq!(req.smile_curve, Patch::Null);
        assert_eq!(req.spread, Patch::Null);
        assert_eq!(req.seed, Patch::Null);
    }

    #[test]
    fn test_deserialize_valued_optional_fields_are_value() {
        // A present value is the "replace" signal, including the new skew_slope.
        let json = r#"{
            "chain_size": 25,
            "strike_interval": 5.0,
            "skew_slope": -0.15,
            "smile_curve": 0.4,
            "spread": 0.02,
            "seed": 99
        }"#;
        let req: UpdateSessionRequest = from_str(json).expect("valued body deserializes");

        assert_eq!(req.chain_size, Patch::Value(25));
        assert_eq!(req.strike_interval, Patch::Value(5.0));
        assert_eq!(req.skew_slope, Patch::Value(-0.15));
        assert_eq!(req.smile_curve, Patch::Value(0.4));
        assert_eq!(req.spread, Patch::Value(0.02));
        assert_eq!(req.seed, Patch::Value(99));
    }

    #[test]
    fn test_serialize_absent_fields_are_omitted() {
        let req = UpdateSessionRequest {
            symbol: None,
            steps: None,
            initial_price: None,
            days_to_expiration: None,
            volatility: None,
            risk_free_rate: None,
            dividend_yield: None,
            method: None,
            time_frame: None,
            chain_size: Patch::Absent,
            strike_interval: Patch::Absent,
            skew_slope: Patch::Absent,
            smile_curve: Patch::Absent,
            spread: Patch::Absent,
            seed: Patch::Absent,
        };

        let value = to_value(&req).expect("serializes");
        assert_eq!(value, json!({}));
    }

    #[test]
    fn test_serialize_null_and_value_fields() {
        let req = UpdateSessionRequest {
            symbol: None,
            steps: None,
            initial_price: None,
            days_to_expiration: None,
            volatility: None,
            risk_free_rate: None,
            dividend_yield: None,
            method: None,
            time_frame: None,
            chain_size: Patch::Value(25),
            strike_interval: Patch::Absent,
            skew_slope: Patch::Null,
            smile_curve: Patch::Absent,
            spread: Patch::Absent,
            seed: Patch::Null,
        };

        let value = to_value(&req).expect("serializes");
        assert_eq!(
            value,
            json!({ "chain_size": 25, "skew_slope": null, "seed": null })
        );
    }
}
