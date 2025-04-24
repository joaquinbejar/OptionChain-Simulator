use optionstratlib::pos;
use optionstratlib::simulation::WalkType;
use optionstratlib::utils::TimeFrame;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::fmt;
use utoipa::ToSchema;

/// Represents address binding options for the server
#[derive(Debug, Clone, Copy, Default)]
pub enum ListenOn {
    /// Binds to all network interfaces (0.0.0.0)
    All,
    /// Binds only to localhost (127.0.0.1)
    #[default]
    Localhost,
}

impl ListenOn {
    /// Converts the enum variant to its corresponding IP address string
    pub fn as_str(&self) -> &'static str {
        match self {
            ListenOn::All => "0.0.0.0",
            ListenOn::Localhost => "127.0.0.1",
        }
    }
}

impl From<ListenOn> for String {
    fn from(listen_on: ListenOn) -> Self {
        listen_on.as_str().to_string()
    }
}

impl fmt::Display for ListenOn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, PartialOrd, ToSchema)]
pub enum ApiTimeFrame {
    /// 1-microsecond data.
    Microsecond,
    /// 1-millisecond data.
    Millisecond,
    /// 1-second data.
    Second,
    /// 1-minute data.
    Minute,
    /// 1-hour data.
    Hour,
    /// Daily data.
    Day,
    /// Weekly data.
    Week,
    /// Monthly data.
    Month,
    /// Quarterly data.
    Quarter,
    /// Yearly data.
    Year,
    /// Custom periods per year.
    Custom(f64),
}

impl From<TimeFrame> for ApiTimeFrame {
    fn from(value: TimeFrame) -> Self {
        match value {
            TimeFrame::Microsecond => ApiTimeFrame::Microsecond,
            TimeFrame::Millisecond => ApiTimeFrame::Millisecond,
            TimeFrame::Second => ApiTimeFrame::Second,
            TimeFrame::Minute => ApiTimeFrame::Minute,
            TimeFrame::Hour => ApiTimeFrame::Hour,
            TimeFrame::Day => ApiTimeFrame::Day,
            TimeFrame::Week => ApiTimeFrame::Week,
            TimeFrame::Month => ApiTimeFrame::Month,
            TimeFrame::Quarter => ApiTimeFrame::Quarter,
            TimeFrame::Year => ApiTimeFrame::Year,
            TimeFrame::Custom(value) => ApiTimeFrame::Custom(value.to_f64()),
        }
    }
}

impl From<ApiTimeFrame> for TimeFrame {
    fn from(value: ApiTimeFrame) -> Self {
        match value {
            ApiTimeFrame::Microsecond => TimeFrame::Microsecond,
            ApiTimeFrame::Millisecond => TimeFrame::Millisecond,
            ApiTimeFrame::Second => TimeFrame::Second,
            ApiTimeFrame::Minute => TimeFrame::Minute,
            ApiTimeFrame::Hour => TimeFrame::Hour,
            ApiTimeFrame::Day => TimeFrame::Day,
            ApiTimeFrame::Week => TimeFrame::Week,
            ApiTimeFrame::Month => TimeFrame::Month,
            ApiTimeFrame::Quarter => TimeFrame::Quarter,
            ApiTimeFrame::Year => TimeFrame::Year,
            ApiTimeFrame::Custom(value) => TimeFrame::Custom(pos!(value)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub enum ApiWalkType {
    /// Standard Brownian motion (normal increments)
    Brownian {
        /// Time step size (fraction of year: daily=1/365, weekly=1/52, etc.)
        dt: f64,
        /// Drift parameter (expected return or growth rate)
        drift: f64,
        /// Volatility parameter (annualized standard deviation)
        volatility: f64,
    },

    /// Geometric Brownian motion (log-normal increments)
    GeometricBrownian {
        /// Time step size (fraction of year: daily=1/365, weekly=1/52, etc.)
        dt: f64,
        /// Drift parameter (expected return or growth rate)
        drift: f64,
        /// Volatility parameter (annualized standard deviation)
        volatility: f64,
    },

    /// Log-Returns model (simulates directly log-returns instead of prices)
    LogReturns {
        /// Time step size (fraction of year: daily=1/365, weekly=1/52, etc.)
        dt: f64,
        /// Expected return (mean of log returns)
        expected_return: f64,
        /// Volatility parameter (annualized standard deviation of log returns)
        volatility: f64,
        /// Optional autocorrelation parameter (-1 to 1)
        autocorrelation: Option<f64>,
    },

    /// Mean-reverting process (Ornstein-Uhlenbeck)
    MeanReverting {
        /// Time step size (fraction of year: daily=1/365, weekly=1/52, etc.)
        dt: f64,
        /// Volatility parameter (annualized standard deviation)
        volatility: f64,
        /// Mean reversion speed (rate at which process reverts to mean)
        speed: f64,
        /// Long-term mean (equilibrium level)
        mean: f64,
    },

    /// Jump diffusion process (normal increments with occasional jumps)
    JumpDiffusion {
        /// Time step size (fraction of year: daily=1/365, weekly=1/52, etc.)
        dt: f64,
        /// Drift parameter (expected return of continuous part)
        drift: f64,
        /// Volatility parameter (annualized standard deviation of continuous part)
        volatility: f64,
        /// Jump intensity (annual frequency of jumps)
        intensity: f64,
        /// Jump size mean (average jump magnitude)
        jump_mean: f64,
        /// Jump size volatility (standard deviation of jump size)
        jump_volatility: f64,
    },

    /// GARCH process (time-varying volatility)
    Garch {
        /// Time step size (fraction of year: daily=1/365, weekly=1/52, etc.)
        dt: f64,
        /// Drift parameter (expected return)
        drift: f64,
        /// Initial volatility parameter (starting volatility level)
        volatility: f64,
        /// GARCH alpha parameter (impact of past observations)
        alpha: f64,
        /// GARCH beta parameter (persistence of volatility)
        beta: f64,
        /// Long-term variance (unconditional variance)
        omega: f64,
    },

    /// Heston model (stochastic volatility)
    Heston {
        /// Time step size (fraction of year: daily=1/365, weekly=1/52, etc.)
        dt: f64,
        /// Drift parameter (expected return)
        drift: f64,
        /// Initial volatility parameter (starting volatility level)
        volatility: f64,
        /// Mean reversion speed of volatility
        kappa: f64,
        /// Long-term variance (equilibrium level of variance)
        theta: f64,
        /// Volatility of volatility (standard deviation of variance process)
        xi: f64,
        /// Correlation between price and volatility processes
        rho: f64,
    },

    /// Custom process defined by a function
    Custom {
        /// Time step size (fraction of year: daily=1/365, weekly=1/52, etc.)
        dt: f64,
        /// Drift parameter (expected change)
        drift: f64,
        /// Volatility parameter (may be interpreted differently based on custom implementation)
        volatility: f64,
        /// Volatility of Volatility parameter (annualized standard deviation)
        vov: f64,
        /// Mean reversion speed (rate at which process reverts to mean)
        vol_speed: f64,
        /// Long-term mean (equilibrium level)
        vol_mean: f64,
    },

    /// Represents historical price data for a given timeframe.
    ///
    /// This encapsulates the historical price data, including the timeframe
    /// over which the data was collected and a vector of positive price values.
    /// It is typically used to store and process historical market data for
    /// financial analysis and simulation purposes.
    ///
    /// # Fields
    ///
    /// * `timeframe`: The `TimeFrame` over which the historical data is relevant.
    /// * `prices`: A `Vec` of `Positive` values representing the historical prices.
    Historical {
        /// The timeframe of the historical data.
        timeframe: ApiTimeFrame,
        /// The vector of positive price values.
        prices: Vec<f64>,
        /// Represents an optional `symbol` as a `String`.
        ///
        /// This field can store the symbol related to an object, entity, or data structure.
        /// If no symbol is provided, the value will be `None`.
        ///
        symbol: Option<String>,
    },
}

impl From<WalkType> for ApiWalkType {
    fn from(value: WalkType) -> Self {
        match value {
            WalkType::Brownian {
                dt,
                drift,
                volatility,
            } => ApiWalkType::Brownian {
                dt: dt.to_f64(),
                drift: drift.to_f64().unwrap_or(0.0),
                volatility: volatility.to_f64(),
            },
            WalkType::GeometricBrownian {
                dt,
                drift,
                volatility,
            } => ApiWalkType::GeometricBrownian {
                dt: dt.to_f64(),
                drift: drift.to_f64().unwrap_or(0.0),
                volatility: volatility.to_f64(),
            },
            WalkType::LogReturns {
                dt,
                expected_return,
                volatility,
                autocorrelation,
            } => ApiWalkType::LogReturns {
                dt: dt.to_f64(),
                expected_return: expected_return.to_f64().unwrap_or(0.0),
                volatility: volatility.to_f64(),
                autocorrelation: autocorrelation.map(|ac| ac.to_f64().unwrap_or(0.0)),
            },
            WalkType::MeanReverting {
                dt,
                volatility,
                speed,
                mean,
            } => ApiWalkType::MeanReverting {
                dt: dt.to_f64(),
                volatility: volatility.to_f64(),
                speed: speed.to_f64(),
                mean: mean.to_f64(),
            },
            WalkType::JumpDiffusion {
                dt,
                drift,
                volatility,
                intensity,
                jump_mean,
                jump_volatility,
            } => ApiWalkType::JumpDiffusion {
                dt: dt.to_f64(),
                drift: drift.to_f64().unwrap_or(0.0),
                volatility: volatility.to_f64(),
                intensity: intensity.to_f64(),
                jump_mean: jump_mean.to_f64().unwrap_or(0.0),
                jump_volatility: jump_volatility.to_f64(),
            },
            WalkType::Garch {
                dt,
                drift,
                volatility,
                alpha,
                beta,
                omega,
            } => ApiWalkType::Garch {
                dt: dt.to_f64(),
                drift: drift.to_f64().unwrap_or(0.0),
                volatility: volatility.to_f64(),
                alpha: alpha.to_f64(),
                beta: beta.to_f64(),
                omega: omega.to_f64(),
            },
            WalkType::Heston {
                dt,
                drift,
                volatility,
                kappa,
                theta,
                xi,
                rho,
            } => ApiWalkType::Heston {
                dt: dt.to_f64(),
                drift: drift.to_f64().unwrap_or(0.0),
                volatility: volatility.to_f64(),
                kappa: kappa.to_f64(),
                theta: theta.to_f64(),
                xi: xi.to_f64(),
                rho: rho.to_f64().unwrap_or(0.0),
            },
            WalkType::Custom {
                dt,
                drift,
                volatility,
                vov,
                vol_speed,
                vol_mean,
            } => ApiWalkType::Custom {
                dt: dt.to_f64(),
                drift: drift.to_f64().unwrap_or(0.0),
                volatility: volatility.to_f64(),
                vov: vov.to_f64(),
                vol_speed: vol_speed.to_f64(),
                vol_mean: vol_mean.to_f64(),
            },
            WalkType::Historical {
                timeframe,
                prices,
                symbol,
            } => ApiWalkType::Historical {
                timeframe: timeframe.into(),
                prices: prices.iter().map(|p| p.to_f64()).collect(),
                symbol,
            },
        }
    }
}

impl From<ApiWalkType> for WalkType {
    fn from(value: ApiWalkType) -> Self {
        match value {
            ApiWalkType::Brownian {
                dt,
                drift,
                volatility,
            } => WalkType::Brownian {
                dt: pos!(dt),
                drift: Decimal::try_from(drift).unwrap_or_default(),
                volatility: pos!(volatility),
            },
            ApiWalkType::GeometricBrownian {
                dt,
                drift,
                volatility,
            } => WalkType::GeometricBrownian {
                dt: pos!(dt),
                drift: Decimal::try_from(drift).unwrap_or_default(),
                volatility: pos!(volatility),
            },
            ApiWalkType::LogReturns {
                dt,
                expected_return,
                volatility,
                autocorrelation,
            } => WalkType::LogReturns {
                dt: pos!(dt),
                expected_return: Decimal::try_from(expected_return).unwrap_or_default(),
                volatility: pos!(volatility),
                autocorrelation: autocorrelation
                    .map(|ac| Decimal::try_from(ac).unwrap_or_default()),
            },
            ApiWalkType::MeanReverting {
                dt,
                volatility,
                speed,
                mean,
            } => WalkType::MeanReverting {
                dt: pos!(dt),
                volatility: pos!(volatility),
                speed: pos!(speed),
                mean: pos!(mean),
            },
            ApiWalkType::JumpDiffusion {
                dt,
                drift,
                volatility,
                intensity,
                jump_mean,
                jump_volatility,
            } => WalkType::JumpDiffusion {
                dt: pos!(dt),
                drift: Decimal::try_from(drift).unwrap_or_default(),
                volatility: pos!(volatility),
                intensity: pos!(intensity),
                jump_mean: Decimal::try_from(jump_mean).unwrap_or_default(),
                jump_volatility: pos!(jump_volatility),
            },
            ApiWalkType::Garch {
                dt,
                drift,
                volatility,
                alpha,
                beta,
                omega,
            } => WalkType::Garch {
                dt: pos!(dt),
                drift: Decimal::try_from(drift).unwrap_or_default(),
                volatility: pos!(volatility),
                alpha: pos!(alpha),
                beta: pos!(beta),
                omega: pos!(omega),
            },
            ApiWalkType::Heston {
                dt,
                drift,
                volatility,
                kappa,
                theta,
                xi,
                rho,
            } => WalkType::Heston {
                dt: pos!(dt),
                drift: Decimal::try_from(drift).unwrap_or_default(),
                volatility: pos!(volatility),
                kappa: pos!(kappa),
                theta: pos!(theta),
                xi: pos!(xi),
                rho: Decimal::try_from(rho).unwrap_or_default(),
            },
            ApiWalkType::Custom {
                dt,
                drift,
                volatility,
                vov,
                vol_speed,
                vol_mean,
            } => WalkType::Custom {
                dt: pos!(dt),
                drift: Decimal::try_from(drift).unwrap_or_default(),
                volatility: pos!(volatility),
                vov: pos!(vov),
                vol_speed: pos!(vol_speed),
                vol_mean: pos!(vol_mean),
            },
            ApiWalkType::Historical {
                timeframe,
                prices,
                symbol,
            } => WalkType::Historical {
                timeframe: timeframe.into(),
                prices: prices.into_iter().map(|p| pos!(p)).collect(),
                symbol,
            },
        }
    }
}

impl fmt::Display for ApiWalkType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Serialize to a JSON string
        let json_str = serde_json::to_string(self).map_err(|_| fmt::Error)?;

        write!(f, "{}", json_str)
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SessionId {
    #[serde(rename = "sessionid")]
    pub(crate) session_id: String,
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.session_id)
    }
}

/// Test suite for ApiTimeFrame conversions and serialization
#[cfg(test)]
mod api_timeframe_tests {
    use super::*;
    use serde_json::{from_str, to_string};

    /// Test conversion from TimeFrame to ApiTimeFrame
    #[test]
    fn test_timeframe_to_api_timeframe_conversion() {
        let test_cases = vec![
            (TimeFrame::Microsecond, ApiTimeFrame::Microsecond),
            (TimeFrame::Millisecond, ApiTimeFrame::Millisecond),
            (TimeFrame::Second, ApiTimeFrame::Second),
            (TimeFrame::Minute, ApiTimeFrame::Minute),
            (TimeFrame::Hour, ApiTimeFrame::Hour),
            (TimeFrame::Day, ApiTimeFrame::Day),
            (TimeFrame::Week, ApiTimeFrame::Week),
            (TimeFrame::Month, ApiTimeFrame::Month),
            (TimeFrame::Quarter, ApiTimeFrame::Quarter),
            (TimeFrame::Year, ApiTimeFrame::Year),
            (TimeFrame::Custom(pos!(2.0)), ApiTimeFrame::Custom(2.0)),
        ];

        for (input, expected) in test_cases {
            let result: ApiTimeFrame = input.into();
            assert_eq!(result, expected, "Conversion failed for {:?}", input);
        }
    }

    /// Test conversion from ApiTimeFrame to TimeFrame
    #[test]
    fn test_api_timeframe_to_timeframe_conversion() {
        let test_cases = vec![
            (ApiTimeFrame::Microsecond, TimeFrame::Microsecond),
            (ApiTimeFrame::Millisecond, TimeFrame::Millisecond),
            (ApiTimeFrame::Second, TimeFrame::Second),
            (ApiTimeFrame::Minute, TimeFrame::Minute),
            (ApiTimeFrame::Hour, TimeFrame::Hour),
            (ApiTimeFrame::Day, TimeFrame::Day),
            (ApiTimeFrame::Week, TimeFrame::Week),
            (ApiTimeFrame::Month, TimeFrame::Month),
            (ApiTimeFrame::Quarter, TimeFrame::Quarter),
            (ApiTimeFrame::Year, TimeFrame::Year),
            (ApiTimeFrame::Custom(2.0), TimeFrame::Custom(pos!(2.0))),
        ];

        for (input, expected) in test_cases {
            let result: TimeFrame = input.into();
            assert_eq!(result, expected, "Conversion failed for {:?}", input);
        }
    }

    /// Test serialization and deserialization of ApiTimeFrame
    #[test]
    fn test_api_timeframe_serialization() {
        let test_cases = vec![
            ApiTimeFrame::Day,
            ApiTimeFrame::Hour,
            ApiTimeFrame::Custom(1.5),
        ];

        for timeframe in test_cases {
            let serialized = to_string(&timeframe).expect("Failed to serialize");
            let deserialized: ApiTimeFrame = from_str(&serialized).expect("Failed to deserialize");
            assert_eq!(timeframe, deserialized);
        }
    }
}

/// Test suite for ApiWalkType conversions and serialization
#[cfg(test)]
mod api_walktype_tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::{from_str, to_string};

    /// Helper function to create test walk types
    fn create_test_walk_types() -> Vec<WalkType> {
        vec![
            WalkType::Brownian {
                dt: pos!(0.004),
                drift: dec!(0.05),
                volatility: pos!(0.25),
            },
            WalkType::GeometricBrownian {
                dt: pos!(0.004),
                drift: dec!(0.05),
                volatility: pos!(0.25),
            },
            WalkType::LogReturns {
                dt: pos!(0.004),
                expected_return: dec!(0.02),
                volatility: pos!(0.25),
                autocorrelation: Some(dec!(0.1)),
            },
            WalkType::MeanReverting {
                dt: pos!(0.004),
                volatility: pos!(0.25),
                speed: pos!(0.5),
                mean: pos!(100.0),
            },
            WalkType::JumpDiffusion {
                dt: pos!(0.004),
                drift: dec!(0.05),
                volatility: pos!(0.25),
                intensity: pos!(0.1),
                jump_mean: dec!(0.02),
                jump_volatility: pos!(0.15),
            },
            WalkType::Historical {
                timeframe: TimeFrame::Day,
                prices: vec![pos!(100.0), pos!(101.0), pos!(102.0)],
                symbol: Some("AAPL".to_string()),
            },
        ]
    }

    /// Test conversion from WalkType to ApiWalkType
    #[test]
    fn test_walktype_to_api_walktype_conversion() {
        for walk_type in create_test_walk_types() {
            let api_walk_type: ApiWalkType = walk_type.clone().into();
            let converted_back: WalkType = api_walk_type.into();

            // Ensure the converted back type matches the original
            assert_eq!(
                walk_type, converted_back,
                "Conversion failed for {:?}",
                walk_type
            );
        }
    }

    /// Test serialization and deserialization of ApiWalkType
    #[test]
    fn test_api_walktype_serialization() {
        let test_cases = create_test_walk_types()
            .into_iter()
            .map(|wt| wt.into())
            .collect::<Vec<ApiWalkType>>();

        for walk_type in test_cases {
            let serialized = to_string(&walk_type).expect("Failed to serialize ApiWalkType");

            let deserialized: ApiWalkType =
                from_str(&serialized).expect("Failed to deserialize ApiWalkType");

            assert_eq!(
                walk_type.to_string(),
                deserialized.to_string(),
                "Serialization/deserialization failed"
            );
        }
    }

    /// Test different variations of walk types with edge cases
    #[test]
    fn test_walktype_edge_cases() {
        // Test conversion of walk types with extreme/default values
        let edge_cases = vec![
            WalkType::Brownian {
                dt: pos!(0.001),
                drift: dec!(0.0),
                volatility: pos!(0.0),
            },
            WalkType::LogReturns {
                dt: pos!(1.0 / 252.0), // Trading day fraction
                expected_return: dec!(0.0),
                volatility: pos!(0.5),
                autocorrelation: None,
            },
        ];

        for walk_type in edge_cases {
            let api_walk_type: ApiWalkType = walk_type.clone().into();
            let converted_back: WalkType = api_walk_type.into();

            assert_eq!(
                walk_type, converted_back,
                "Edge case conversion failed for {:?}",
                walk_type
            );
        }
    }
}

/// Test suite for SessionId
#[cfg(test)]
mod session_id_tests {
    use super::*;
    use serde_json::{from_str, to_string};

    #[test]
    fn test_session_id_creation_and_display() {
        let session_id_str = "6af613b6-569c-5c22-9c37-2ed93f31d3af";
        let session_id = SessionId {
            session_id: session_id_str.to_string(),
        };

        // Test string conversion
        assert_eq!(format!("{}", session_id), session_id_str);

        // Test serialization and deserialization
        let serialized = to_string(&session_id).expect("Failed to serialize");
        assert!(serialized.contains(session_id_str));

        let deserialized: SessionId = from_str(&serialized).expect("Failed to deserialize");
        assert_eq!(deserialized.session_id, session_id_str);
    }

    #[test]
    fn test_session_id_rename_attribute() {
        // Test that the sessionid attribute is correctly renamed during serialization
        let session_id_str = "test-session-id";
        let session_id = SessionId {
            session_id: session_id_str.to_string(),
        };

        let serialized = to_string(&session_id).expect("Failed to serialize");
        assert!(serialized.contains("\"sessionid\":\"test-session-id\""));
    }

    #[test]
    fn test_display_implementations() {
        let test_cases = vec![
            (
                ApiWalkType::Brownian {
                    dt: 0.004,
                    drift: 0.05,
                    volatility: 0.25,
                },
                r#"{"Brownian":{"dt":0.004,"drift":0.05,"volatility":0.25}}"#,
            ),
            (
                ApiWalkType::Historical {
                    timeframe: ApiTimeFrame::Day,
                    prices: vec![100.0, 101.0, 102.0],
                    symbol: Some("AAPL".to_string()),
                },
                r#"{"Historical":{"timeframe":"Day","prices":[100.0,101.0,102.0],"symbol":"AAPL"}}"#,
            ),
            (
                ApiWalkType::LogReturns {
                    dt: 0.004,
                    expected_return: 0.02,
                    volatility: 0.25,
                    autocorrelation: None,
                },
                r#"{"LogReturns":{"dt":0.004,"expected_return":0.02,"volatility":0.25,"autocorrelation":null}}"#,
            ),
        ];

        for (walk_type, expected) in test_cases {
            assert_eq!(
                walk_type.to_string(),
                expected,
                "Display implementation failed for {:?}",
                walk_type
            );
        }
    }

    #[test]
    fn test_roundtrip_serialization() {
        let test_cases = vec![
            ApiWalkType::Brownian {
                dt: 0.004,
                drift: 0.05,
                volatility: 0.25,
            },
            ApiWalkType::Historical {
                timeframe: ApiTimeFrame::Day,
                prices: vec![100.0, 101.0, 102.0],
                symbol: Some("AAPL".to_string()),
            },
            ApiWalkType::LogReturns {
                dt: 0.004,
                expected_return: 0.02,
                volatility: 0.25,
                autocorrelation: None,
            },
        ];

        for walk_type in test_cases {
            // Convert to string (JSON)
            let json_str = walk_type.to_string();

            // Deserialize back to ApiWalkType
            let deserialized: ApiWalkType =
                serde_json::from_str(&json_str).expect("Failed to deserialize");

            // Ensure the deserialized version matches the original
            assert_eq!(
                walk_type, deserialized,
                "Roundtrip serialization failed for {:?}",
                walk_type
            );
        }
    }
}
