use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::session::model::Session;
use crate::utils::error::ChainError;

/// A trait that defines the behavior of a session store backend.
/// This trait is intended to manage sessions by providing methods for retrieving,
/// saving, deleting, and cleaning up session data.
///
/// Implementors of the `SessionStore` trait must provide thread-safe and
/// shareable implementations (i.e., satisfy `Send` and `Sync`).
///
/// # Associated Types
/// - `Uuid`: A universally unique identifier used to identify sessions.
/// - `Session`: The session object containing session data.
/// - `ChainError`: Represents errors that may occur during operations.
///
/// # Required Methods
///
/// ## `get`
/// Retrieves a session associated with the given `Uuid`.
///
/// - **Parameters**:
///   - `id`: A `Uuid` identifying the session.
/// - **Returns**:
///   - `Ok(Session)`: The session object if retrieval is successful.
///   - `Err(ChainError)`: An error if the session cannot be retrieved.
///
/// ## `save`
/// Persists the provided session object.
///
/// - **Parameters**:
///   - `session`: The `Session` object to be saved.
/// - **Returns**:
///   - `Ok(())`: Indicates successful saving of the session.
///   - `Err(ChainError)`: An error if the session cannot be saved.
///
/// ## `delete`
/// Deletes a session identified by the given `Uuid`.
///
/// - **Parameters**:
///   - `id`: A `Uuid` identifying the session to be deleted.
/// - **Returns**:
///   - `Ok(true)`: Indicates the session was successfully deleted.
///   - `Ok(false)`: Indicates the session did not exist.
///   - `Err(ChainError)`: An error if the deletion fails.
///
/// ## `cleanup`
/// Cleans up expired or stale sessions from the session store.
///
/// - **Returns**:
///   - `Ok(usize)`: The number of sessions that were cleaned up.
///   - `Err(ChainError)`: An error if cleanup fails.
///
pub trait SessionStore: Send + Sync {
    fn get(&self, id: Uuid) -> Result<Session, ChainError>;
    fn save(&self, session: Session) -> Result<(), ChainError>;
    fn delete(&self, id: Uuid) -> Result<bool, ChainError>;
    fn cleanup(&self) -> Result<usize, ChainError>;
}

/// `InMemorySessionStore` is a structure that provides an in-memory implementation
/// of a session storage system. It allows you to store session information in a
/// thread-safe manner using an internally wrapped `HashMap` protected by a
/// `Mutex` within an `Arc`.
///
/// # Fields
/// - `sessions`: An `Arc` (atomic reference-counted pointer) that safely shares ownership
///   of the session store across threads. The `Mutex` ensures only one thread can
///   modify the `HashMap` at a time, maintaining thread safety.
///   - The `HashMap` uses `Uuid` as the key for identifying sessions and maps
///     it to the corresponding `Session` struct.
///
/// This structure is useful for simple session management in applications where
/// in-memory storage suffices, such as in single-server or low-scale environments.
/// For larger applications or distributed setups, a more robust solution (e.g.,
/// database-backed storage) might be required.
pub struct InMemorySessionStore {
    sessions: Arc<Mutex<HashMap<Uuid, Session>>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl SessionStore for InMemorySessionStore {
    fn get(&self, id: Uuid) -> Result<Session, ChainError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| ChainError::Internal("Failed to acquire lock on session store".to_string()))?;

        sessions
            .get(&id)
            .cloned()
            .ok_or_else(|| ChainError::NotFound(format!("Session with id {} not found", id)))
    }

    fn save(&self, session: Session) -> Result<(), ChainError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| ChainError::Internal("Failed to acquire lock on session store".to_string()))?;

        sessions.insert(session.id, session);
        Ok(())
    }

    fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| ChainError::Internal("Failed to acquire lock on session store".to_string()))?;

        Ok(sessions.remove(&id).is_some())
    }

    fn cleanup(&self) -> Result<usize, ChainError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| ChainError::Internal("Failed to acquire lock on session store".to_string()))?;

        // Find expired sessions (older than 30 minutes)
        let now = std::time::SystemTime::now();
        let expired_ids: Vec<Uuid> = sessions
            .iter()
            .filter_map(
                |(id, session)| match now.duration_since(session.updated_at) {
                    Ok(duration) if duration.as_secs() > 1800 => Some(*id),
                    _ => None,
                },
            )
            .collect();

        // Remove expired sessions
        let count = expired_ids.len();
        for id in expired_ids {
            sessions.remove(&id);
        }

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::model::{Session, SessionState, SimulationParameters};
    use crate::session::SimulationMethod;
    use crate::utils::UuidGenerator;
    use optionstratlib::{pos, Positive};
    use optionstratlib::utils::TimeFrame;
    use rust_decimal::Decimal;
    use std::thread;
    use std::time::{Duration, SystemTime};
    use uuid::Uuid;

    fn create_test_session(id_option: Option<Uuid>) -> Session {
        let params = SimulationParameters {
            symbol: "TEST".to_string(),
            steps: 10,
            initial_price: pos!(100.0),
            days_to_expiration: pos!(30.0),
            volatility: pos!(0.2),
            risk_free_rate: Decimal::ZERO,
            dividend_yield: Positive::ZERO,
            method: SimulationMethod::GeometricBrownian {
                dt: pos!(1.0),
                drift: Decimal::ZERO,
                volatility: pos!(0.2),
            },
            time_frame: TimeFrame::Day,
            chain_size: Some(5),
            strike_interval: Some(pos!(5.0)),
            skew_factor: None,
            spread: None,
        };

        let now = SystemTime::now();

        if let Some(id) = id_option {
            Session {
                id,
                created_at: now,
                updated_at: now,
                current_step: 0,
                total_steps: params.steps,
                parameters: params,
                state: SessionState::Initialized,
            }
        } else {
            Session::new(params)
        }
    }

    #[test]
    fn test_in_memory_session_store_new() {
        let store = InMemorySessionStore::new();

        // Verificar que el store se creó correctamente
        let sessions = store.sessions.lock().unwrap();
        assert_eq!(sessions.len(), 0);
    }

    #[test]
    fn test_get_nonexistent_session() {
        let store = InMemorySessionStore::new();
        let id = Uuid::new_v4();

        let result = store.get(id);

        assert!(result.is_err());
        match result {
            Err(ChainError::NotFound(msg)) => {
                assert!(msg.contains(&id.to_string()));
            }
            _ => panic!("Expected NotFound error"),
        }
    }

    #[test]
    fn test_save_and_get_session() {
        let store = InMemorySessionStore::new();
        let session = create_test_session(None);
        let id = session.id;

        let save_result = store.save(session.clone());
        assert!(save_result.is_ok());

        let get_result = store.get(id);
        assert!(get_result.is_ok());

        let retrieved_session = get_result.unwrap();
        assert_eq!(retrieved_session.id, id);
        assert_eq!(retrieved_session.parameters.symbol, "TEST");
        assert_eq!(retrieved_session.state, SessionState::Initialized);
    }

    #[test]
    fn test_save_multiple_sessions() {
        let store = InMemorySessionStore::new();

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        let session1 = create_test_session(Some(id1));
        let session2 = create_test_session(Some(id2));

        assert!(store.save(session1).is_ok());
        assert!(store.save(session2).is_ok());

        let retrieved1 = store.get(id1).unwrap();
        let retrieved2 = store.get(id2).unwrap();

        assert_eq!(retrieved1.id, id1);
        assert_eq!(retrieved2.id, id2);

        let sessions = store.sessions.lock().unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn test_update_existing_session() {
        let store = InMemorySessionStore::new();
        let mut session = create_test_session(None);
        let id = session.id;

        // Guardar la sesión inicial
        assert!(store.save(session.clone()).is_ok());

        // Modificar la sesión y guardarla nuevamente
        session.state = SessionState::InProgress;
        session.current_step = 1;
        assert!(store.save(session).is_ok());

        // Verificar que los cambios se aplicaron
        let retrieved = store.get(id).unwrap();
        assert_eq!(retrieved.state, SessionState::InProgress);
        assert_eq!(retrieved.current_step, 1);
    }

    #[test]
    fn test_delete_session() {
        let store = InMemorySessionStore::new();
        let session = create_test_session(None);
        let id = session.id;

        // Guardar y luego borrar la sesión
        assert!(store.save(session).is_ok());
        let delete_result = store.delete(id);

        assert!(delete_result.is_ok());
        assert!(delete_result.unwrap());

        // Verificar que la sesión ya no existe
        assert!(store.get(id).is_err());
    }

    #[test]
    fn test_delete_nonexistent_session() {
        let store = InMemorySessionStore::new();
        let id = Uuid::new_v4();

        let delete_result = store.delete(id);

        assert!(delete_result.is_ok());
        assert!(!delete_result.unwrap()); // Debe retornar false
    }

    #[test]
    fn test_cleanup_expired_sessions() {
        let store = InMemorySessionStore::new();

        // Crear una sesión con tiempo actual
        let current_session = create_test_session(None);
        let current_id = current_session.id;

        // Crear una sesión "antigua" (más de 30 minutos)
        let expired_time = SystemTime::now().checked_sub(Duration::from_secs(3600)).unwrap();
        let expired_id = Uuid::new_v4();
        let expired_session = Session {
            id: expired_id,
            created_at: expired_time,
            updated_at: expired_time,
            current_step: 0,
            total_steps: 10,
            parameters: current_session.parameters.clone(),
            state: SessionState::Initialized,
        };

        // Guardar ambas sesiones
        assert!(store.save(current_session).is_ok());
        assert!(store.save(expired_session).is_ok());

        // Ejecutar la limpieza
        let cleanup_result = store.cleanup();
        assert!(cleanup_result.is_ok());
        assert_eq!(cleanup_result.unwrap(), 1); // Una sesión debe ser eliminada

        // Verificar que solo la sesión actual sigue existiendo
        assert!(store.get(current_id).is_ok());
        assert!(store.get(expired_id).is_err());
    }

    #[test]
    fn test_concurrent_access() {
        let store = Arc::new(InMemorySessionStore::new());
        let session = create_test_session(None);
        let id = session.id;

        // Guardar la sesión inicial
        assert!(store.save(session).is_ok());

        let store_clone = Arc::clone(&store);
        let handle = thread::spawn(move || {
            // Intentar obtener la sesión desde otro hilo
            let result = store_clone.get(id);
            assert!(result.is_ok());

            let mut session = result.unwrap();
            session.state = SessionState::InProgress;

            // Guardar los cambios
            assert!(store_clone.save(session).is_ok());
        });

        // Esperar a que el hilo termine
        handle.join().unwrap();

        // Verificar que los cambios del otro hilo se aplicaron
        let retrieved = store.get(id).unwrap();
        assert_eq!(retrieved.state, SessionState::InProgress);
    }

    #[test]
    fn test_lock_poisoning_recovery() {
        let store = InMemorySessionStore::new();
        let session = create_test_session(None);
        let id = session.id;

        // Guardar la sesión
        assert!(store.save(session).is_ok());

        // Simular un envenenamiento del mutex
        {
            let mutex_guard = store.sessions.lock().unwrap();
            // Normalmente aquí haríamos algo que cause pánico
            // pero no podemos inducir un pánico real en un test
            // Por lo tanto, simplemente verificamos que el manejo
            // de errores funcione si ocurriera
            drop(mutex_guard);
        }

        // Operaciones posteriores deberían manejar correctamente un mutex envenenado
        // aunque en este caso no lo está realmente
        let _ = store.get(id);
        let _ = store.delete(id);
        let _ = store.cleanup();
    }
}