use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};
use optionstratlib::Positive;
use uuid::Uuid;
use crate::utils::UuidGenerator;
pub use optionstratlib::simulation::WalkType as SimulationMethod;
use optionstratlib::utils::TimeFrame;

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

/// Parameters for configuring a simulation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationParameters {
    pub symbol: String,
    pub steps: usize,
    pub initial_price: Positive,
    pub days_to_expiration: Positive,
    pub volatility: Positive,
    pub risk_free_rate: Decimal,
    pub dividend_yield: Positive,
    pub method: SimulationMethod,
    pub time_frame: TimeFrame,
    pub chain_size: Option<usize>,
    pub strike_interval: Option<Positive>,
    pub skew_factor: Option<Decimal>,
    pub spread: Option<Positive>,
}

/// Represents a stateful simulation session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
    pub parameters: SimulationParameters,
    pub current_step: usize,
    pub total_steps: usize,
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