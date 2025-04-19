use crate::utils::{ChainError, UuidGenerator};
use optionstratlib::Positive;
pub use optionstratlib::simulation::WalkType as SimulationMethod;
use optionstratlib::utils::TimeFrame;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
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

    /// Creates a new instance of the simulation using the specified parameters.
    ///
    /// This function initializes the simulation by creating a default UUID namespace,
    /// which is required for compatibility purposes, and a UUID generator using this
    /// namespace. It then delegates the creation process to the `new_with_generator`
    /// function using the provided parameters and the generated UUID generator.
    ///
    /// # Arguments
    /// * `parameters` - A `SimulationParameters` struct that contains the configuration
    ///   settings necessary to start the simulation.
    ///
    /// # Returns
    /// * `Self` - A new instance of the simulation object.
    ///
    /// # Panics
    /// This method will panic if the hardcoded default UUID namespace cannot be parsed.
    /// This is highly unlikely as the namespace string is valid and hardcoded.
    ///
    pub fn new(parameters: SimulationParameters) -> Self {
        // Create a default namespace for compatibility
        let namespace = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8")
            .expect("Failed to parse default UUID namespace");
        let generator = UuidGenerator::new(namespace);
        Self::new_with_generator(parameters, &generator)
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
    pub fn reinitialize(&mut self, new_params: SimulationParameters, total_steps: usize) {
        self.parameters = new_params;
        self.current_step = 0;
        self.total_steps = total_steps;
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
