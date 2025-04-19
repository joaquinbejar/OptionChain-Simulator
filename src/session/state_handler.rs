use crate::session::{Session, SessionState};
use crate::utils::error::ChainError;

/// Handles state transitions for simulation sessions
pub struct StateProgressionHandler;

impl StateProgressionHandler {
    pub fn new() -> Self {
        Self
    }

    pub fn advance_state(&self, session: &mut Session) -> Result<(), ChainError> {
        match session.state {
            SessionState::Initialized => {
                session.state = SessionState::InProgress;
                session.advance_step()?;
                Ok(())
            }
            SessionState::InProgress => {
                session.advance_step()?;
                Ok(())
            }
            SessionState::Modified => {
                session.state = SessionState::InProgress;
                session.advance_step()?;
                Ok(())
            }
            SessionState::Reinitialized => {
                session.state = SessionState::InProgress;
                session.advance_step()?;
                Ok(())
            }
            SessionState::Completed => Err(ChainError::InvalidState(
                "Session has completed all steps".to_string(),
            )),
            SessionState::Error => Err(ChainError::InvalidState(
                "Session is in error state".to_string(),
            )),
        }
    }

    pub fn reset_progression(&self, session: &mut Session) -> Result<(), ChainError> {
        session.current_step = 0;
        session.state = SessionState::Reinitialized;
        Ok(())
    }
}
