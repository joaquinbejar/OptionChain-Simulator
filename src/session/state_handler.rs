use crate::session::{Session, SessionState};
use crate::utils::error::ChainError;

/// Handles state transitions for simulation sessions
pub struct StateProgressionHandler;

impl StateProgressionHandler {
    pub fn new() -> Self {
        Self
    }

    /// Advances the session one step under serve-then-advance semantics.
    ///
    /// The `Reinitialized` branch mirrors `Initialized`/`Modified`: the snapshot at
    /// cursor 0 has already been served (the simulator rebuilt the walk because it
    /// observed the pre-advance `Reinitialized` state), so this transitions the
    /// session out of `Reinitialized` into `InProgress` and advances the cursor. That
    /// leaves the persisted state as `InProgress`, so the NEXT advance serves the
    /// cached walk instead of triggering another eviction/rebuild — the fix for a
    /// reinitialized session getting stuck at step 0 forever (issue #4).
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionState, SimulationMethod, SimulationParameters};
    use crate::utils::UuidGenerator;
    use optionstratlib::utils::TimeFrame;
    use positive::{Positive, pos_or_panic};
    use rust_decimal::Decimal;
    use uuid::Uuid;

    /// Helper function to create a test session
    fn create_test_session(initial_state: SessionState) -> Session {
        let params = SimulationParameters {
            symbol: "TEST".to_string(),
            steps: 10,
            initial_price: pos_or_panic!(100.0),
            days_to_expiration: pos_or_panic!(30.0),
            volatility: pos_or_panic!(0.2),
            risk_free_rate: Decimal::ZERO,
            dividend_yield: Positive::ZERO,
            method: SimulationMethod::GeometricBrownian {
                dt: pos_or_panic!(1.0),
                drift: Decimal::ZERO,
                volatility: pos_or_panic!(0.2),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(5),
            strike_interval: Some(pos_or_panic!(5.0)),
            skew_slope: None,
            smile_curve: None,
            spread: None,
            seed: None,
        };

        let namespace = Uuid::new_v4().to_string();
        let namespace_uuid = Uuid::parse_str(&namespace).expect("Failed to parse UUID");
        let uuid_generator = UuidGenerator::new(namespace_uuid);

        let mut session = Session::new(params, &uuid_generator);
        session.state = initial_state;
        session
    }

    #[test]
    fn test_advance_state_initialized() {
        let handler = StateProgressionHandler::new();
        let mut session = create_test_session(SessionState::Initialized);

        let result = handler.advance_state(&mut session);

        assert!(result.is_ok());
        assert_eq!(session.state, SessionState::InProgress);
        assert_eq!(session.current_step, 1);
    }

    #[test]
    fn test_advance_state_in_progress() {
        let handler = StateProgressionHandler::new();
        let mut session = create_test_session(SessionState::InProgress);
        session.current_step = 5; // Set to a mid-point step

        let result = handler.advance_state(&mut session);

        assert!(result.is_ok());
        assert_eq!(session.state, SessionState::InProgress);
        assert_eq!(session.current_step, 6);
    }

    #[test]
    fn test_advance_state_modified() {
        let handler = StateProgressionHandler::new();
        let mut session = create_test_session(SessionState::Modified);

        let result = handler.advance_state(&mut session);

        assert!(result.is_ok());
        assert_eq!(session.state, SessionState::InProgress);
        assert_eq!(session.current_step, 1);
    }

    #[test]
    fn test_advance_state_reinitialized() {
        // Issue #4: a reinitialized session must not stick at step 0. After the
        // snapshot at cursor 0 is served, advancing transitions the session out of
        // `Reinitialized` into `InProgress` and moves the cursor to 1, so the next
        // advance serves the cached walk instead of rebuilding it forever.
        let handler = StateProgressionHandler::new();
        let mut session = create_test_session(SessionState::Reinitialized);

        let result = handler.advance_state(&mut session);

        assert!(result.is_ok());
        assert_eq!(session.state, SessionState::InProgress);
        assert_eq!(session.current_step, 1);
    }

    #[test]
    fn test_advance_state_completed() {
        let handler = StateProgressionHandler::new();
        let mut session = create_test_session(SessionState::Completed);

        let result = handler.advance_state(&mut session);

        assert!(result.is_err());
        match result {
            Err(ChainError::InvalidState(msg)) => {
                assert_eq!(msg, "Session has completed all steps");
            }
            _ => panic!("Expected InvalidState error"),
        }
    }

    #[test]
    fn test_advance_state_error() {
        let handler = StateProgressionHandler::new();
        let mut session = create_test_session(SessionState::Error);

        let result = handler.advance_state(&mut session);

        assert!(result.is_err());
        match result {
            Err(ChainError::InvalidState(msg)) => {
                assert_eq!(msg, "Session is in error state");
            }
            _ => panic!("Expected InvalidState error"),
        }
    }
}
