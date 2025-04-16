use std::sync::Arc;
use optionstratlib::chains::OptionChain;
use uuid::Uuid;
use crate::domain::Simulator;
use crate::session::model::{Session, SimulationParameters};
use crate::session::state_handler::StateProgressionHandler;
use crate::session::store::SessionStore;
use crate::utils::error::ChainError;

/// Manages the lifecycle of simulation sessions
pub struct SessionManager {
    store: Arc<dyn SessionStore>,
    state_handler: StateProgressionHandler,
    simulator: Simulator,
}

impl SessionManager {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            state_handler: StateProgressionHandler::new(),
            simulator: Simulator::new(),
        }
    }

    pub fn create_session(&self, params: SimulationParameters, total_steps: u32) -> Result<Session, ChainError> {
        let session = Session::new(params, total_steps);
        self.store.save(session.clone())?;
        Ok(session)
    }

    pub fn get_next_step(&self, id: Uuid) -> Result<(Session, OptionChain), ChainError> {
        let mut session = self.store.get(id)?;

        // Advance session state
        self.state_handler.advance_state(&mut session)?;

        // Generate option chain for current step
        let chain = self.simulator.simulate_next_step(&session)
            .map_err(|e| ChainError::SimulatorError(format!("Simulation error: {}", e)))?;

        // Save updated session
        self.store.save(session.clone())?;

        Ok((session, chain))
    }

    pub fn update_session(&self, id: Uuid, params: SimulationParameters) -> Result<Session, ChainError> {
        let mut session = self.store.get(id)?;

        // Update parameters
        session.modify_parameters(params);

        // Save updated session
        self.store.save(session.clone())?;

        Ok(session)
    }

    pub fn reinitialize_session(&self, id: Uuid, params: SimulationParameters, total_steps: u32) -> Result<Session, ChainError> {
        let mut session = self.store.get(id)?;

        // Reinitialize session
        session.reinitialize(params, total_steps);

        // Reset progression
        self.state_handler.reset_progression(&mut session)?;

        // Save updated session
        self.store.save(session.clone())?;

        Ok(session)
    }

    pub fn delete_session(&self, id: Uuid) -> Result<bool, ChainError> {
        self.store.delete(id)
    }

    pub fn cleanup_sessions(&self) -> Result<usize, ChainError> {
        self.store.cleanup()
    }
}