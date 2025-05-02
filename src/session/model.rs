use crate::api::CreateSessionRequest;
use crate::session::manager::DEFAULT_NAMESPACE;
use crate::utils::{ChainError, UuidGenerator};
pub use optionstratlib::simulation::WalkType as SimulationMethod;
use optionstratlib::utils::TimeFrame;
use optionstratlib::{Positive, pos};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::SystemTime;
use uuid::Uuid;

/// Represents the various states a session can be in.
///
/// This enum is used to track and manage the state of a session throughout its lifecycle.
/// It derives several traits for added utility:
/// - `Debug`: Allows for formatting the value using the `{:?}` formatter.
/// - `Clone`: Enables creating a duplicate of a session state.
/// - `Copy`: Permits duplication of values without explicitly calling `.clone()`.
/// - `Serialize` / `Deserialize`: Enables serialization and deserialization, typically for data storage
///   or communication purposes (requires support from the `serde` library).
/// - `PartialEq`: Provides equality comparison between two `SessionState` values.
///
/// ## Variants
///
/// - `Initialized`: Represents the initial state of a session when it is created.
/// - `InProgress`: Indicates that the session is currently active and ongoing.
/// - `Modified`: Signifies that changes have been made to the session since it began.
/// - `Reinitialized`: Represents that the session has been reset or started over after being modified or completed.
/// - `Completed`: Denotes that the session has successfully reached its end.
/// - `Error`: Indicates that an error occurred during the session.
///
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum SessionState {
    /// Represents the initialization state or process of a given system, application, or component.
    ///
    /// This can be used to indicate that the underlying functionality, resources,
    /// or metadata required for a program or module are being set up to operate correctly.
    ///
    Initialized,
    /// Represents the status "In Progress" for a task or operation.
    /// This status is typically used to indicate that a process or task
    /// has been started but is not yet complete.
    InProgress,
    /// This property or function is marked as `Modified` to indicate that it has been updated or altered
    /// from its original implementation. It serves as a marker for tracking changes in the codebase.
    Modified,
    ///
    /// The `Reinitialized` function or event typically signifies that an object, variable, or system
    /// component has been reset or initialized again to its default or initial state.
    ///
    /// Use Cases:
    /// - To reset the state of an object or application back to its starting point.
    /// - To reapply initial configurations or refresh components in a program.
    Reinitialized,
    /// This class indicates the completion status of a certain task or operation.
    ///
    /// The "Completed" class typically signifies that a specific action, process,
    /// or task has been successfully finished. It may be used as a marker class
    /// or structure to facilitate understanding of workflow completion in the
    /// application logic.
    Completed,
    /// Represents an error type in the application.
    ///
    /// The `Error` class or struct (assuming this is part of the code implementation)
    /// can be used to define and handle errors that occur within the application.
    /// Depending on its implementation, it may support features like error messages,
    /// error codes, or additional metadata to provide contextual information about
    /// the error.
    Error,
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionState::Initialized => write!(f, "Initialized"),
            SessionState::InProgress => write!(f, "In Progress"),
            SessionState::Modified => write!(f, "Modified"),
            SessionState::Reinitialized => write!(f, "Reinitialized"),
            SessionState::Completed => write!(f, "Completed"),
            SessionState::Error => write!(f, "Error"),
        }
    }
}

/// `SimulationParameters` is a struct that encapsulates the configuration parameters
/// required for simulating the behavior of a financial asset or instrument.
/// It includes details about the asset, simulation parameters, and optional refinements
/// for advanced scenarios.
///
/// # Notes
/// - This struct implements the `Debug`, `Clone`, `Serialize`, and `Deserialize` traits, allowing it to be easily logged, duplicated, and serialized/deserialized for storage and transmission.
///
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationParameters {
    /// - `symbol` (`String`): The name or ticker symbol of the asset being simulated.
    pub symbol: String,
    /// - `steps` (`usize`): The number of discrete time steps or intervals in the simulation process.
    pub steps: usize,
    /// - `initial_price` (`Positive`): The initial starting price of the asset. This must be a positive value.
    pub initial_price: Positive,
    /// - `days_to_expiration` (`Positive`): The number of days until the expiration of the asset or contract. This must be a positive value.
    pub days_to_expiration: Positive,
    /// - `volatility` (`Positive`): The expected volatility (standard deviation) of the asset's returns.
    pub volatility: Positive,
    /// - `risk_free_rate` (`Decimal`): The risk-free rate of return, typically represented as an annualized percentage.
    pub risk_free_rate: Decimal,
    /// - `dividend_yield` (`Positive`): The annualized dividend yield of the asset, expressed as a positive value.
    pub dividend_yield: Positive,
    /// - `method` (`SimulationMethod`): The simulation method or algorithm to be used, defining the behavior of the simulation process.
    pub method: SimulationMethod,
    /// - `time_frame` (`TimeFrame`): The time frame for the simulation intervals, such as daily, weekly, or hourly.
    pub time_frame: TimeFrame,
    /// - `chain_size` (`Option<usize>`): The optional size of the option chain being simulated. If `None`, this is not specified.
    pub chain_size: Option<usize>,
    /// - `strike_interval` (`Option<Positive>`): The optional interval between strike prices for options. If `None`, this is not specified.
    pub strike_interval: Option<Positive>,
    /// A public field representing an optional skew slope value.
    ///
    /// # Description
    /// The `skew_slope` field holds an optional `Decimal` value that indicates
    /// the slope or angle associated with a skew operation or calculation.
    /// If the value is `None`, it implies that the skew slope is not set or
    /// applicable in the current context.
    pub skew_slope: Option<Decimal>,
    /// - `smile_curve` (`Option<Decimal>`): An optional factor that adjusts the skew of the distribution. For example, it can be used to bias option pricing.
    pub smile_curve: Option<Decimal>,
    /// - `spread` (`Option<Positive>`): An optional parameter to specify the spread value. If `None`, no spread is applied.
    pub spread: Option<Positive>,
}

impl fmt::Display for SimulationParameters {
    /// Serialize `SimulationParameters` to JSON string for Display.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Serialize to JSON, map any error to fmt::Error
        let json = serde_json::to_string(self).map_err(|_| fmt::Error)?;
        write!(f, "{}", json)
    }
}

impl From<CreateSessionRequest> for SimulationParameters {
    fn from(req: CreateSessionRequest) -> Self {
        Self {
            symbol: req.symbol,
            steps: req.steps,
            initial_price: pos!(req.initial_price),
            days_to_expiration: pos!(req.days_to_expiration),
            volatility: pos!(req.volatility),
            risk_free_rate: Decimal::try_from(req.risk_free_rate).unwrap_or_default(),
            dividend_yield: pos!(req.dividend_yield),
            method: req.method.into(),
            time_frame: req.time_frame.into(),
            chain_size: req.chain_size,
            strike_interval: req.strike_interval.map(|v| pos!(v)),
            skew_slope: req
                .skew_slope
                .map(|v| Decimal::try_from(v).unwrap_or_default()),
            smile_curve: req
                .smile_curve
                .map(|v| Decimal::try_from(v).unwrap_or_default()),
            spread: req.spread.map(|v| pos!(v)),
        }
    }
}

/// Represents a simulation session with its current state and parameters.
///
/// This struct holds information about a simulation session, including its
/// unique identifier, timestamps for creation and the last update, simulation
/// parameters, and progress details like the current step and total steps.
/// Additionally, it tracks the overall session state.
///
/// # Traits
///
/// * `Debug` - Allows for formatting the `Session` object using the `{:?}`
///   formatter for debugging purposes.
/// * `Clone` - Enables the `Session` object to be cloned, creating an identical
///   copy of the session.
/// * `Serialize, Deserialize` - Allows the `Session` object to be serialized
///   (converted to a format like JSON) and deserialized (converted back to the
///   struct) for storage or communication purposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// * `id` - A universally unique identifier (UUID) for the session.
    pub id: Uuid,
    /// * `created_at` - The timestamp indicating when the session was created.
    pub created_at: SystemTime,
    /// * `updated_at` - The timestamp indicating the last time the session was updated.
    pub updated_at: SystemTime,
    /// * `parameters` - The parameters associated with the simulation session,
    pub parameters: SimulationParameters,
    /// * `current_step` - The current step or iteration of the simulation session.
    pub current_step: usize,
    /// * `total_steps` - The total number of steps or iterations required for the
    pub total_steps: usize,
    /// * `state` - The current state of the session, represented by an instance
    pub state: SessionState,
}

impl Session {
    /// Creates and returns a new instance of the structure with the given simulation parameters
    /// and a UUID generator. This method initializes various fields of the structure, including
    /// a unique identifier, timestamps for creation and last update, and simulation state.
    ///
    /// # Parameters
    ///
    /// * `parameters` - A `SimulationParameters` object that defines the configuration for the simulation, including the total number of steps.
    /// * `uuid_generator` - A reference to a `UuidGenerator` instance used to generate a unique identifier for the new session.
    ///
    /// # Returns
    ///
    /// * `Self` - A new instance of the structure with initialized fields, including:
    ///   - `id`: A unique identifier generated by the `uuid_generator`.
    ///   - `created_at`: The time at which this instance is created, set to the current system time.
    ///   - `updated_at`: The time of the last update, initially set to the creation time.
    ///   - `current_step`: The starting step of the simulation, initialized to 0.
    ///   - `total_steps`: The number of steps to be executed, retrieved from `parameters`.
    ///   - `parameters`: The simulation parameters used to configure the session.
    ///   - `state`: The initial state of the session, set to `SessionState::Initialized`.
    ///
    pub fn new_with_generator(
        parameters: SimulationParameters,
        uuid_generator: &UuidGenerator,
    ) -> Self {
        let now = SystemTime::now();
        Self {
            id: uuid_generator.next(),
            created_at: now,
            updated_at: now,
            current_step: 0,
            total_steps: parameters.steps,
            parameters,
            state: SessionState::Initialized,
        }
    }

    /// Creates a new instance of the struct using the provided `SimulationParameters` and a reference
    /// to a `UuidGenerator`.
    ///
    /// This function delegates to `Self::new_with_generator` to initialize the struct with the given
    /// parameters and UUID generator.
    ///
    /// # Arguments
    ///
    /// * `parameters` - An instance of `SimulationParameters` that defines the configuration for the simulation.
    /// * `uuid_generator` - A reference to a `UuidGenerator`, which is used for generating unique identifiers required by the instance.
    ///
    /// # Returns
    ///
    /// A new instance of the struct.
    ///
    pub fn new(parameters: SimulationParameters, uuid_generator: &UuidGenerator) -> Self {
        Self::new_with_generator(parameters, uuid_generator)
    }

    /// Advances the session to the next step while updating its state and timestamp.
    ///
    /// # Behavior
    /// - If the current step has already reached or exceeded the total number of steps, returns a `ChainError`
    ///   with a `SessionError` containing the message `"Session has completed all steps"`.
    /// - Increments the `current_step` by 1 and updates `updated_at` to the current system time.
    /// - Updates the session state based on its progress:
    ///   - If the `current_step` equals `total_steps`, the session state is set to `SessionState::Completed`.
    ///   - If the session was in the `SessionState::Initialized` or `SessionState::Modified` state, it is
    ///     transitioned to the `SessionState::InProgress` state.
    ///
    /// # Returns
    /// - `Ok(())` if the operation is successful.
    /// - `Err(ChainError)` if the session has completed all steps and cannot advance further.
    ///
    /// # Errors
    /// - Returns a `ChainError::SessionError` if attempting to advance past the total number of steps.
    ///
    pub fn advance_step(&mut self) -> Result<(), ChainError> {
        if self.current_step >= self.total_steps {
            return Err(ChainError::SessionError(
                "Session has completed all steps".to_string(),
            ));
        }

        self.current_step += 1;
        self.updated_at = SystemTime::now();

        if self.current_step == self.total_steps {
            self.state = SessionState::Completed;
        } else if self.state == SessionState::Initialized || self.state == SessionState::Modified {
            self.state = SessionState::InProgress;
        }

        Ok(())
    }

    /// Updates the simulation parameters of the current session.
    ///
    /// # Parameters
    /// - `new_params`: The new `SimulationParameters` to replace the existing parameters.
    ///
    /// # Behavior
    /// - The `parameters` field of the session is updated with the provided `new_params`.
    /// - The `updated_at` field is set to the current system time, marking the time of modification.
    /// - The session's `state` is updated to `SessionState::Modified` to reflect the change.
    ///
    /// # Notes
    /// This method assumes that the caller has mutable access to the session object and is responsible for ensuring
    /// that the modified parameters are valid within the context of the simulation.
    ///
    /// # Panics
    /// This function does not explicitly handle errors; however, any issues such as obtaining the current system time
    /// (if `SystemTime::now()` fails) may panic depending on the environment.
    pub fn modify_parameters(&mut self, new_params: SimulationParameters) {
        self.parameters = new_params;
        self.updated_at = SystemTime::now();
        self.state = SessionState::Modified;
    }

    /// Reinitializes the simulation with new parameters.
    ///
    /// This method resets the simulation to its initial state by updating
    /// the parameters, resetting the current step count, setting the total
    /// number of steps, updating the timestamp to the current system time,
    /// and changing the session state to `Reinitialized`.
    ///
    /// # Parameters
    ///
    /// - `new_params`: A `SimulationParameters` object containing the new
    ///   configuration settings for the simulation.
    /// - `total_steps`: The total number of steps the simulation will run
    ///   after reinitialization.
    ///
    /// # Behavior
    ///
    /// - The `parameters` field is updated with the provided `new_params`.
    /// - The `current_step` counter is reset to zero.
    /// - The `total_steps` field is updated to reflect the new simulation
    ///   duration.
    /// - The `updated_at` field is set to the current system time to record
    ///   the timestamp of the reinitialization.
    /// - The `state` field is set to `SessionState::Reinitialized` to
    ///   indicate that the simulation has been reset.
    ///
    pub fn reinitialize(&mut self, new_params: SimulationParameters) {
        self.total_steps = new_params.steps;
        self.parameters = new_params;
        self.current_step = 0;
        self.updated_at = SystemTime::now();
        self.state = SessionState::Reinitialized;
    }

    /// Determines whether the current session is active.
    ///
    /// A session is considered active if its state is neither `Completed` nor `Error`.
    ///
    /// # Returns
    ///
    /// * `true` - If the session state is not `Completed` or `Error`.
    /// * `false` - If the session state is either `Completed` or `Error`.
    ///
    pub fn is_active(&self) -> bool {
        self.state != SessionState::Completed && self.state != SessionState::Error
    }
}

impl Default for Session {
    fn default() -> Self {
        let namespace =
            Uuid::parse_str(DEFAULT_NAMESPACE).expect("Failed to parse default UUID namespace");
        let uuid_generator = UuidGenerator::new(namespace);

        // Parámetros de simulación predeterminados
        let default_params = SimulationParameters {
            symbol: "AAPL".to_string(),
            steps: 20,
            initial_price: pos!(100.0),
            days_to_expiration: pos!(30.0),
            volatility: pos!(0.2),
            risk_free_rate: Decimal::new(3, 2),
            dividend_yield: Positive::ZERO,
            method: SimulationMethod::GeometricBrownian {
                dt: pos!(1.0 / 252.0),
                drift: Decimal::ZERO,
                volatility: pos!(0.2),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(30),
            strike_interval: Some(pos!(5.0)),
            skew_slope: Some(dec!(-0.2)),
            smile_curve: Some(dec!(0.4)),
            spread: Some(pos!(0.02)),
        };

        Self {
            id: uuid_generator.next(),
            created_at: SystemTime::now(),
            updated_at: SystemTime::now(),
            parameters: default_params,
            current_step: 0,
            total_steps: 20,
            state: SessionState::Initialized,
        }
    }
}
impl fmt::Display for Session {
    /// Serialize `Session` to JSON string for Display.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Same approach: convert to JSON string
        let json = serde_json::to_string(self).map_err(|_| fmt::Error)?;
        write!(f, "{}", json)
    }
}

#[cfg(test)]
mod tests_simulation_parameters_serialization {
    use super::*;
    use crate::session::SimulationParameters;
    use optionstratlib::pos;
    use rust_decimal_macros::dec;
    use serde_json::{Value, from_str, to_string};

    #[test]
    fn test_simulation_parameters_serialization() {
        // Create a sample SimulationParameters with all fields populated
        let params = SimulationParameters {
            symbol: "AAPL".to_string(),
            steps: 30,
            initial_price: pos!(150.75),
            days_to_expiration: pos!(45.0),
            volatility: pos!(0.25),
            risk_free_rate: dec!(0.04),
            dividend_yield: pos!(0.015),
            method: SimulationMethod::GeometricBrownian {
                dt: pos!(0.0027),
                drift: dec!(0.05),
                volatility: pos!(0.25),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(15),
            strike_interval: Some(pos!(5.0)),
            skew_slope: Some(dec!(-0.2)),
            smile_curve: Some(dec!(0.5)),
            spread: Some(pos!(0.02)),
        };

        // Serialize to JSON
        let json = to_string(&params).unwrap();

        // Verify JSON contains all expected fields
        let value: Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["symbol"], "AAPL");
        assert_eq!(value["steps"], 30);
        assert_eq!(value["initial_price"], 150.75);
        assert_eq!(value["days_to_expiration"], 45.0);
        assert_eq!(value["volatility"], 0.25);
        assert_eq!(value["risk_free_rate"], "0.04");
        assert_eq!(value["dividend_yield"], 0.015);
        assert_eq!(value["time_frame"], "Day");
        assert_eq!(value["chain_size"], 15);
        assert_eq!(value["strike_interval"], 5.0);
        assert_eq!(value["smile_curve"], "0.5");
        assert_eq!(value["spread"], 0.02);

        // Check the method field specifically
        assert!(value["method"].is_object());
        assert!(
            value["method"]
                .as_object()
                .unwrap()
                .contains_key("GeometricBrownian")
        );
        assert_eq!(value["method"]["GeometricBrownian"]["dt"], 0.0027);
        assert_eq!(value["method"]["GeometricBrownian"]["drift"], "0.05");
        assert_eq!(value["method"]["GeometricBrownian"]["volatility"], 0.25);

        // Deserialize back to verify round-trip
        let deserialized: SimulationParameters = from_str(&json).unwrap();

        // Check a few key fields to verify successful deserialization
        assert_eq!(deserialized.symbol, "AAPL");
        assert_eq!(deserialized.initial_price, pos!(150.75));
        assert_eq!(deserialized.chain_size, Some(15));

        // For method, we need to match the enum variant
        match deserialized.method {
            SimulationMethod::GeometricBrownian {
                dt,
                drift,
                volatility,
            } => {
                assert_eq!(dt, pos!(0.0027));
                assert_eq!(drift, dec!(0.05));
                assert_eq!(volatility, pos!(0.25));
            }
            _ => panic!("Wrong simulation method variant deserialized"),
        }
    }

    #[test]
    fn test_simulation_parameters_with_optional_fields_none() {
        // Create params with None for optional fields
        let params = SimulationParameters {
            symbol: "SPY".to_string(),
            steps: 20,
            initial_price: pos!(420.50),
            days_to_expiration: pos!(30.0),
            volatility: pos!(0.18),
            risk_free_rate: dec!(0.035),
            dividend_yield: pos!(0.01),
            method: SimulationMethod::Brownian {
                dt: pos!(0.0027),
                drift: dec!(0.0),
                volatility: pos!(0.18),
            },
            time_frame: TimeFrame::Day,
            chain_size: None,
            strike_interval: None,
            skew_slope: None,
            smile_curve: None,
            spread: None,
        };

        // Serialize to JSON
        let json = to_string(&params).unwrap();

        // Parse JSON to verify structure
        let value: Value = serde_json::from_str(&json).unwrap();

        // Check that optional fields are null or missing
        assert!(value.get("chain_size").is_none() || value["chain_size"].is_null());
        assert!(value.get("strike_interval").is_none() || value["strike_interval"].is_null());
        assert!(value.get("smile_curve").is_none() || value["smile_curve"].is_null());
        assert!(value.get("spread").is_none() || value["spread"].is_null());

        // Deserialize back and verify
        let deserialized: SimulationParameters = from_str(&json).unwrap();
        assert_eq!(deserialized.chain_size, None);
        assert_eq!(deserialized.strike_interval, None);
        assert_eq!(deserialized.smile_curve, None);
        assert_eq!(deserialized.spread, None);
    }

    #[test]
    fn test_deserialization_from_json_string() {
        // Create a JSON string directly
        let json = r#"{
            "symbol": "TSLA",
            "steps": 50,
            "initial_price": 240.35,
            "days_to_expiration": 60,
            "volatility": 0.35,
            "risk_free_rate": "0.045",
            "dividend_yield": 0,
            "method": {
                "Brownian": {
                    "dt": 0.0027,
                    "drift": "0.02",
                    "volatility": 0.35
                }
            },
            "time_frame": "Day",
            "chain_size": 20,
            "strike_interval": 10.0,
            "smile_curve": "0.001",
            "spread": 0.025
        }"#;

        // Deserialize
        let params: SimulationParameters = from_str(json).unwrap();

        // Verify fields
        assert_eq!(params.symbol, "TSLA");
        assert_eq!(params.steps, 50);
        assert_eq!(params.initial_price, pos!(240.35));
        assert_eq!(params.days_to_expiration, pos!(60.0));
        assert_eq!(params.volatility, pos!(0.35));
        assert_eq!(params.risk_free_rate, dec!(0.045));
        assert_eq!(params.dividend_yield, pos!(0.0));
        assert_eq!(params.time_frame, TimeFrame::Day);
        assert_eq!(params.chain_size, Some(20));
        assert_eq!(params.strike_interval, Some(pos!(10.0)));
        assert_eq!(params.smile_curve, Some(dec!(0.001)));
        assert_eq!(params.spread, Some(pos!(0.025)));

        // Check method variant
        match params.method {
            SimulationMethod::Brownian {
                dt,
                drift,
                volatility,
            } => {
                assert_eq!(dt, pos!(0.0027));
                assert_eq!(drift, dec!(0.02));
                assert_eq!(volatility, pos!(0.35));
            }
            _ => panic!("Wrong simulation method variant deserialized"),
        }
    }

    #[test]
    fn test_different_simulation_methods() {
        // Test serialization/deserialization with different simulation methods

        // Test with MeanReverting method
        let params_mr = SimulationParameters {
            symbol: "GLD".to_string(),
            steps: 40,
            initial_price: pos!(1950.0),
            days_to_expiration: pos!(90.0),
            volatility: pos!(0.15),
            risk_free_rate: dec!(0.04),
            dividend_yield: pos!(0.0),
            method: SimulationMethod::MeanReverting {
                dt: pos!(0.0027),
                volatility: pos!(0.15),
                speed: pos!(0.5),
                mean: pos!(2000.0),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(25),
            strike_interval: Some(pos!(25.0)),
            skew_slope: None,
            smile_curve: None,
            spread: Some(pos!(0.01)),
        };

        let json_mr = to_string(&params_mr).unwrap();
        let deserialized_mr: SimulationParameters = from_str(&json_mr).unwrap();

        match deserialized_mr.method {
            SimulationMethod::MeanReverting {
                dt,
                volatility,
                speed,
                mean,
            } => {
                assert_eq!(dt, pos!(0.0027));
                assert_eq!(volatility, pos!(0.15));
                assert_eq!(speed, pos!(0.5));
                assert_eq!(mean, pos!(2000.0));
            }
            _ => panic!("Wrong simulation method variant deserialized"),
        }

        // Test with Historical method
        let params_hist = SimulationParameters {
            symbol: "OIL".to_string(),
            steps: 30,
            initial_price: pos!(75.0),
            days_to_expiration: pos!(30.0),
            volatility: pos!(0.25),
            risk_free_rate: dec!(0.035),
            dividend_yield: pos!(0.0),
            method: SimulationMethod::Historical {
                timeframe: TimeFrame::Day,
                prices: vec![pos!(75.0), pos!(76.2), pos!(74.8), pos!(77.5), pos!(78.1)],
                symbol: None,
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(15),
            strike_interval: Some(pos!(5.0)),
            skew_slope: None,
            smile_curve: None,
            spread: None,
        };

        let json_hist = to_string(&params_hist).unwrap();
        let deserialized_hist: SimulationParameters = from_str(&json_hist).unwrap();

        match deserialized_hist.method {
            SimulationMethod::Historical {
                timeframe, prices, ..
            } => {
                assert_eq!(timeframe, TimeFrame::Day);
                assert_eq!(prices.len(), 5);
                assert_eq!(prices[0], pos!(75.0));
                assert_eq!(prices[4], pos!(78.1));
            }
            _ => panic!("Wrong simulation method variant deserialized"),
        }
    }

    #[test]
    fn test_timeframe_serialization() {
        // Test different TimeFrame values
        let timeframes = vec![
            (TimeFrame::Minute, "Minute"),
            (TimeFrame::Hour, "Hour"),
            (TimeFrame::Day, "Day"),
            (TimeFrame::Week, "Week"),
            (TimeFrame::Month, "Month"),
        ];

        for (tf, expected_str) in timeframes {
            let params = SimulationParameters {
                symbol: "TEST".to_string(),
                steps: 10,
                initial_price: pos!(100.0),
                days_to_expiration: pos!(30.0),
                volatility: pos!(0.2),
                risk_free_rate: dec!(0.03),
                dividend_yield: pos!(0.01),
                method: SimulationMethod::GeometricBrownian {
                    dt: pos!(0.0027),
                    drift: dec!(0.0),
                    volatility: pos!(0.2),
                },
                time_frame: tf,
                chain_size: None,
                strike_interval: None,
                skew_slope: None,
                smile_curve: None,
                spread: None,
            };

            let json = to_string(&params).unwrap();
            let value: Value = serde_json::from_str(&json).unwrap();

            assert_eq!(value["time_frame"].as_str().unwrap(), expected_str);

            let deserialized: SimulationParameters = from_str(&json).unwrap();
            assert_eq!(deserialized.time_frame, tf);
        }
    }

    #[test]
    fn test_negative_values() {
        // Test with some negative decimal values
        let params = SimulationParameters {
            symbol: "INDEX".to_string(),
            steps: 25,
            initial_price: pos!(1000.0),
            days_to_expiration: pos!(30.0),
            volatility: pos!(0.2),
            risk_free_rate: dec!(-0.01), // Negative rate
            dividend_yield: pos!(0.02),
            method: SimulationMethod::JumpDiffusion {
                dt: pos!(0.0027),
                drift: dec!(-0.02), // Negative drift
                volatility: pos!(0.2),
                intensity: pos!(2.0),
                jump_mean: dec!(-0.05), // Negative jump mean
                jump_volatility: pos!(0.1),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(10),
            strike_interval: Some(pos!(10.0)),
            skew_slope: Some(dec!(-0.2)),
            smile_curve: Some(dec!(-0.5)), // Negative skew
            spread: Some(pos!(0.015)),
        };

        let json = to_string(&params).unwrap();

        // Check specific negative values
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["risk_free_rate"], "-0.01");
        assert_eq!(value["method"]["JumpDiffusion"]["drift"], "-0.02");
        assert_eq!(value["method"]["JumpDiffusion"]["jump_mean"], "-0.05");
        assert_eq!(value["smile_curve"], "-0.5");

        // Deserialize and verify
        let deserialized: SimulationParameters = from_str(&json).unwrap();
        assert_eq!(deserialized.risk_free_rate, dec!(-0.01));
        assert_eq!(deserialized.smile_curve, Some(dec!(-0.5)));

        match deserialized.method {
            SimulationMethod::JumpDiffusion {
                drift, jump_mean, ..
            } => {
                assert_eq!(drift, dec!(-0.02));
                assert_eq!(jump_mean, dec!(-0.05));
            }
            _ => panic!("Wrong simulation method variant deserialized"),
        }
    }
}

#[cfg(test)]
mod tests_session_state {
    use super::*;
    use serde_json;

    #[test]
    fn test_session_state_display() {
        assert_eq!(SessionState::Initialized.to_string(), "Initialized");
        assert_eq!(SessionState::InProgress.to_string(), "In Progress");
        assert_eq!(SessionState::Modified.to_string(), "Modified");
        assert_eq!(SessionState::Reinitialized.to_string(), "Reinitialized");
        assert_eq!(SessionState::Completed.to_string(), "Completed");
        assert_eq!(SessionState::Error.to_string(), "Error");
    }

    #[test]
    fn test_session_state_serialization() {
        let state = SessionState::Initialized;
        let serialized = serde_json::to_string(&state).unwrap();
        assert_eq!(serialized, "\"Initialized\"");

        let state = SessionState::InProgress;
        let serialized = serde_json::to_string(&state).unwrap();
        assert_eq!(serialized, "\"InProgress\"");

        let state = SessionState::Modified;
        let serialized = serde_json::to_string(&state).unwrap();
        assert_eq!(serialized, "\"Modified\"");

        let state = SessionState::Reinitialized;
        let serialized = serde_json::to_string(&state).unwrap();
        assert_eq!(serialized, "\"Reinitialized\"");

        let state = SessionState::Completed;
        let serialized = serde_json::to_string(&state).unwrap();
        assert_eq!(serialized, "\"Completed\"");

        let state = SessionState::Error;
        let serialized = serde_json::to_string(&state).unwrap();
        assert_eq!(serialized, "\"Error\"");
    }

    #[test]
    fn test_session_state_deserialization() {
        let deserialized: SessionState = serde_json::from_str("\"Initialized\"").unwrap();
        assert_eq!(deserialized, SessionState::Initialized);

        let deserialized: SessionState = serde_json::from_str("\"InProgress\"").unwrap();
        assert_eq!(deserialized, SessionState::InProgress);

        let deserialized: SessionState = serde_json::from_str("\"Modified\"").unwrap();
        assert_eq!(deserialized, SessionState::Modified);

        let deserialized: SessionState = serde_json::from_str("\"Reinitialized\"").unwrap();
        assert_eq!(deserialized, SessionState::Reinitialized);

        let deserialized: SessionState = serde_json::from_str("\"Completed\"").unwrap();
        assert_eq!(deserialized, SessionState::Completed);

        let deserialized: SessionState = serde_json::from_str("\"Error\"").unwrap();
        assert_eq!(deserialized, SessionState::Error);
    }

    #[test]
    fn test_session_state_equality() {
        assert_eq!(SessionState::Initialized, SessionState::Initialized);
        assert_ne!(SessionState::Initialized, SessionState::InProgress);
        assert_ne!(SessionState::Error, SessionState::Completed);
    }
}

#[cfg(test)]
mod tests_session {
    use super::*;
    use optionstratlib::spos;
    use rust_decimal_macros::dec;
    use std::time::Duration;
    use uuid::Uuid;

    // Helper function to create a default SimulationParameters for tests
    fn create_test_parameters() -> SimulationParameters {
        SimulationParameters {
            symbol: "TEST".to_string(),
            steps: 10,
            initial_price: pos!(100.0),
            days_to_expiration: pos!(30.0),
            volatility: pos!(0.25),
            risk_free_rate: Decimal::new(5, 2), // 0.05
            dividend_yield: pos!(0.02),         // 0.02
            method: SimulationMethod::Brownian {
                dt: pos!(0.0027),
                drift: dec!(0.0),
                volatility: pos!(0.25),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(10),
            strike_interval: spos!(5.0),
            skew_slope: None,
            smile_curve: Some(Decimal::new(1, 1)), // 0.1
            spread: spos!(0.2),                    // 0.2
        }
    }

    #[test]
    fn test_new_with_generator() {
        // Arrange
        let params = create_test_parameters();
        let uuid_gen = UuidGenerator::new(Uuid::new_v4());
        // Act
        let session = Session::new_with_generator(params.clone(), &uuid_gen);

        // Assert
        assert_eq!(session.current_step, 0);
        assert_eq!(session.total_steps, params.steps);
        assert_eq!(session.parameters.symbol, params.symbol);
        assert_eq!(session.state, SessionState::Initialized);

        // The created_at and updated_at should be very close to each other
        let time_diff = session
            .updated_at
            .duration_since(session.created_at)
            .unwrap_or(Duration::from_secs(1));
        assert!(time_diff.as_millis() < 100); // They should be within 100ms of each other
    }

    #[test]
    fn test_new_delegates_to_new_with_generator() {
        // Arrange
        let params = create_test_parameters();
        let uuid_gen = UuidGenerator::new(Uuid::new_v4());

        // Act
        let session = Session::new(params.clone(), &uuid_gen);

        // Assert
        assert_eq!(session.current_step, 0);
        assert_eq!(session.total_steps, params.steps);
        assert_eq!(session.state, SessionState::Initialized);
    }

    #[test]
    fn test_advance_step_from_initialized() {
        // Arrange
        let params = create_test_parameters();
        let uuid_gen = UuidGenerator::new(Uuid::new_v4());
        let mut session = Session::new_with_generator(params, &uuid_gen);
        let original_time = session.updated_at;

        // Introduce a small delay to ensure timestamp changes
        std::thread::sleep(Duration::from_millis(10));

        // Act
        let result = session.advance_step();

        // Assert
        assert!(result.is_ok());
        assert_eq!(session.current_step, 1);
        assert_eq!(session.state, SessionState::InProgress);
        assert!(session.updated_at > original_time);
    }

    #[test]
    fn test_advance_step_from_modified() {
        // Arrange
        let params = create_test_parameters();
        let uuid_gen = UuidGenerator::new(Uuid::new_v4());
        let mut session = Session::new_with_generator(params.clone(), &uuid_gen);
        session.state = SessionState::Modified;

        // Act
        let result = session.advance_step();

        // Assert
        assert!(result.is_ok());
        assert_eq!(session.current_step, 1);
        assert_eq!(session.state, SessionState::InProgress);
    }

    #[test]
    fn test_advance_step_in_progress() {
        // Arrange
        let params = create_test_parameters();
        let uuid_gen = UuidGenerator::new(Uuid::new_v4());
        let mut session = Session::new_with_generator(params.clone(), &uuid_gen);
        session.state = SessionState::InProgress;
        session.current_step = 1;

        // Act
        let result = session.advance_step();

        // Assert
        assert!(result.is_ok());
        assert_eq!(session.current_step, 2);
        assert_eq!(session.state, SessionState::InProgress);
    }

    #[test]
    fn test_advance_step_to_completion() {
        // Arrange
        let params = create_test_parameters();
        let uuid_gen = UuidGenerator::new(Uuid::new_v4());
        let mut session = Session::new_with_generator(params.clone(), &uuid_gen);
        session.state = SessionState::InProgress;
        session.current_step = session.total_steps - 1;

        // Act
        let result = session.advance_step();

        // Assert
        assert!(result.is_ok());
        assert_eq!(session.current_step, session.total_steps);
        assert_eq!(session.state, SessionState::Completed);
    }

    #[test]
    fn test_advance_step_already_completed() {
        // Arrange
        let params = create_test_parameters();
        let uuid_gen = UuidGenerator::new(Uuid::new_v4());
        let mut session = Session::new_with_generator(params.clone(), &uuid_gen);
        session.current_step = session.total_steps;
        session.state = SessionState::Completed;

        // Act
        let result = session.advance_step();

        // Assert
        assert!(result.is_err());
        match result {
            Err(ChainError::SessionError(msg)) => {
                assert_eq!(msg, "Session has completed all steps");
            }
            _ => panic!("Expected SessionError but got {:?}", result),
        }
    }

    #[test]
    fn test_modify_parameters() {
        // Arrange
        let original_params = create_test_parameters();
        let uuid_gen = UuidGenerator::new(Uuid::new_v4());
        let mut session = Session::new_with_generator(original_params, &uuid_gen);
        let original_time = session.updated_at;

        // Create new parameters with different values
        let mut new_params = create_test_parameters();
        new_params.symbol = "MODIFIED".to_string();
        new_params.steps = 20;

        std::thread::sleep(Duration::from_millis(10));

        // Act
        session.modify_parameters(new_params.clone());

        // Assert
        assert_eq!(session.parameters.symbol, "MODIFIED");
        assert_eq!(session.parameters.steps, 20);
        assert_eq!(session.state, SessionState::Modified);
        assert!(session.updated_at > original_time);
    }

    #[test]
    fn test_reinitialize() {
        // Arrange
        let original_params = create_test_parameters();
        let uuid_gen = UuidGenerator::new(Uuid::new_v4());
        let mut session = Session::new_with_generator(original_params, &uuid_gen);

        // Advance the session
        session.current_step = 5;
        session.state = SessionState::InProgress;

        let original_time = session.updated_at;

        // Create new parameters with different values
        let mut new_params = create_test_parameters();
        new_params.symbol = "REINITIALIZED".to_string();
        new_params.steps = 15;

        std::thread::sleep(Duration::from_millis(10));

        // Act
        session.reinitialize(new_params.clone());

        // Assert
        assert_eq!(session.parameters.symbol, "REINITIALIZED");
        assert_eq!(session.current_step, 0);
        assert_eq!(session.total_steps, 15);
        assert_eq!(session.state, SessionState::Reinitialized);
        assert!(session.updated_at > original_time);
    }

    #[test]
    fn test_is_active() {
        let params = create_test_parameters();
        let uuid_gen = UuidGenerator::new(Uuid::new_v4());

        // Test different states
        let mut session = Session::new_with_generator(params.clone(), &uuid_gen);

        // Initialized state
        session.state = SessionState::Initialized;
        assert!(session.is_active());

        // InProgress state
        session.state = SessionState::InProgress;
        assert!(session.is_active());

        // Modified state
        session.state = SessionState::Modified;
        assert!(session.is_active());

        // Reinitialized state
        session.state = SessionState::Reinitialized;
        assert!(session.is_active());

        // Completed state
        session.state = SessionState::Completed;
        assert!(!session.is_active());

        // Error state
        session.state = SessionState::Error;
        assert!(!session.is_active());
    }

    #[test]
    fn test_display_implementation() {
        // Arrange
        let params = create_test_parameters();
        let uuid_gen = UuidGenerator::new(Uuid::new_v4());
        let session = Session::new_with_generator(params, &uuid_gen);

        // Act
        let display_string = format!("{}", session);

        // Assert
        // The display implementation should create a valid JSON string
        let parsed_result = serde_json::from_str::<Session>(&display_string);
        assert!(parsed_result.is_ok());

        let parsed_session = parsed_result.unwrap();
        assert_eq!(parsed_session.id, session.id);
        assert_eq!(parsed_session.current_step, session.current_step);
        assert_eq!(parsed_session.total_steps, session.total_steps);
        assert_eq!(parsed_session.state, session.state);
    }
}
