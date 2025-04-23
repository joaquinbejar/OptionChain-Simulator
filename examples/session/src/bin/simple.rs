use optionstratlib::utils::time::convert_time_frame;
use optionstratlib::utils::{TimeFrame, setup_logger};
use optionstratlib::{Positive, pos, spos};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tracing::{error, info};
use uuid::Uuid;

use optionchain_simulator::session::{InMemorySessionStore, SimulationMethod};
use optionchain_simulator::session::{Session, SessionManager, SessionState, SimulationParameters};
use optionchain_simulator::utils::error::ChainError;

/// Example demonstrating the usage of SessionManager and Session for option chain simulation
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logger();

    info!("Starting OptionChain-Simulator example");

    // Create an in-memory session store
    let store = Arc::new(InMemorySessionStore::new());

    // Initialize the session manager
    let session_manager = SessionManager::new(store.clone());

    // Define simulation parameters
    let params = create_simulation_parameters();

    // Create a new simulation session
    info!("Creating a new simulation session");
    let session_result = session_manager.create_session(params);

    match session_result {
        Ok(session) => {
            info!(
                session_id = %session.id,
                "Session created successfully with initial state: {:?}",
                session.state
            );

            // Run the session through its lifecycle
            if let Err(e) = run_session_lifecycle(&session_manager, session.id).await {
                error!("Error during session lifecycle: {}", e);
            }

            // Demonstrate error handling with a non-existent session
            info!("Attempting to access a non-existent session");
            let random_uuid = Uuid::new_v4();
            match session_manager.get_next_step(random_uuid).await {
                Ok(_) => info!("Unexpectedly found a random session"),
                Err(e) => error!("Expected error occurred: {}", e),
            }

            // Cleanup expired sessions
            match session_manager.cleanup_sessions() {
                Ok(count) => info!("Cleaned up {} expired sessions", count),
                Err(e) => error!("Error cleaning up sessions: {}", e),
            }
        }
        Err(e) => {
            error!("Failed to create session: {}", e);
        }
    }

    info!("OptionChain-Simulator example completed");
    Ok(())
}

/// Creates simulation parameters for testing
fn create_simulation_parameters() -> SimulationParameters {
    let volatility = pos!(0.2);
    let time_frame = TimeFrame::Minute;
    let dt = convert_time_frame(Positive::ONE, &time_frame, &TimeFrame::Day);
    SimulationParameters {
        symbol: "CL".to_string(),
        steps: 30,
        initial_price: pos!(1000.0),
        days_to_expiration: pos!(30.0),
        volatility: pos!(0.2),
        risk_free_rate: Decimal::ZERO,
        dividend_yield: Positive::ZERO,
        method: SimulationMethod::GeometricBrownian {
            dt,
            drift: Decimal::ZERO,
            volatility,
        },
        time_frame,
        chain_size: Some(30),
        strike_interval: Some(Positive::ONE),
        skew_factor: None,
        spread: spos!(0.02),
    }
}

/// Runs a session through its complete lifecycle
async fn run_session_lifecycle(
    session_manager: &SessionManager,
    session_id: Uuid,
) -> Result<(), ChainError> {
    // Step 1: Get initial option chain
    info!(session_id = %session_id, "Advancing session to first step");
    let (session, chain) = session_manager.get_next_step(session_id).await?;

    info!(
        session_id = %session_id,
        current_step = session.current_step,
        state = ?session.state,
        "Retrieved first option chain"
    );

    // Step 2: Advance a few more steps
    for i in 0..3 {
        info!(session_id = %session_id, step = i+2, "Advancing to next step");
        match session_manager.get_next_step(session_id).await {
            Ok((session, _chain)) => {
                info!(
                    session_id = %session_id,
                    current_step = session.current_step,
                    state = ?session.state,
                    "Advanced simulation successfully"
                );
            }
            Err(e) => {
                error!(session_id = %session_id, "Error advancing simulation: {}", e);
                return Err(e);
            }
        }
    }

    // Step 3: Modify session parameters
    info!(session_id = %session_id, "Modifying session parameters");
    let mut modified_params = create_simulation_parameters();

    // Increase volatility
    let volatility = pos!(0.3); // Increased from 0.2 to 0.3
    let time_frame = TimeFrame::Minute;
    let dt = convert_time_frame(Positive::ONE, &time_frame, &TimeFrame::Day);

    modified_params.volatility = volatility;
    modified_params.method = SimulationMethod::GeometricBrownian {
        dt,
        drift: dec!(0.05),
        volatility,
    };

    match session_manager.update_session(session_id, modified_params) {
        Ok(modified_session) => {
            info!(
                session_id = %session_id,
                state = ?modified_session.state,
                "Session parameters modified"
            );
        }
        Err(e) => {
            error!(session_id = %session_id, "Error modifying session: {}", e);
            return Err(e);
        }
    }

    // Step 4: Advance with modified parameters
    for i in 0..2 {
        info!(session_id = %session_id, step = i+1, "Advancing with modified parameters");
        match session_manager.get_next_step(session_id).await {
            Ok((session, _chain)) => {
                info!(
                    session_id = %session_id,
                    current_step = session.current_step,
                    state = ?session.state,
                    "Advanced simulation with modified parameters"
                );
            }
            Err(e) => {
                error!(session_id = %session_id, "Error advancing simulation: {}", e);
                return Err(e);
            }
        }
    }

    // Step 7: Delete the session
    info!(session_id = %session_id, "Deleting session");
    match session_manager.delete_session(session_id) {
        Ok(deleted) => {
            info!(session_id = %session_id, deleted = deleted, "Session deletion result");
        }
        Err(e) => {
            error!(session_id = %session_id, "Error deleting session: {}", e);
            return Err(e);
        }
    }

    Ok(())
}
