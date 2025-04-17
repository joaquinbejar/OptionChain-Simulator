use crate::utils::UuidGenerator;
use optionstratlib::Positive;
pub use optionstratlib::simulation::WalkType as SimulationMethod;
use optionstratlib::utils::TimeFrame;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use uuid::Uuid;

/// Possible states a simulation session can be in
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum SessionState {
    Initialized,
    InProgress,
    Modified,
    Reinitialized,
    Completed,
    Error,
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
    /// - `skew_factor` (`Option<Decimal>`): An optional factor that adjusts the skew of the distribution. For example, it can be used to bias option pricing.
    pub skew_factor: Option<Decimal>,
    /// - `spread` (`Option<Positive>`): An optional parameter to specify the spread value. If `None`, no spread is applied.
    pub spread: Option<Positive>,
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
    /// Creates a new Session using the provided UuidGenerator for ID generation
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

    /// Legacy method maintained for backward compatibility
    pub fn new(parameters: SimulationParameters) -> Self {
        // Create a default namespace for compatibility
        let namespace = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8")
            .expect("Failed to parse default UUID namespace");
        let generator = UuidGenerator::new(namespace);
        Self::new_with_generator(parameters, &generator)
    }

    pub fn advance_step(&mut self) -> Result<(), String> {
        if self.current_step >= self.total_steps {
            return Err("Session has completed all steps".to_string());
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

    pub fn modify_parameters(&mut self, new_params: SimulationParameters) {
        self.parameters = new_params;
        self.updated_at = SystemTime::now();
        self.state = SessionState::Modified;
    }

    pub fn reinitialize(&mut self, new_params: SimulationParameters, total_steps: usize) {
        self.parameters = new_params;
        self.current_step = 0;
        self.total_steps = total_steps;
        self.updated_at = SystemTime::now();
        self.state = SessionState::Reinitialized;
    }

    pub fn is_active(&self) -> bool {
        self.state != SessionState::Completed && self.state != SessionState::Error
    }
}
