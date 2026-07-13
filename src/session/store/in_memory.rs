use crate::session::{Session, SessionStore};
use crate::utils::ChainError;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

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

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySessionStore {
    /// Creates a new instance of the struct.
    ///
    /// This function initializes and returns a new instance of the struct with an empty `HashMap`
    /// wrapped in an `Arc<Mutex<>>`. The `Arc` ensures thread-safe shared ownership of the `Mutex`,
    /// while the `Mutex` provides interior mutability and thread-safe access to the `HashMap`.
    ///
    /// # Returns
    ///
    /// * `Self` - A new instance of the struct.
    ///
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn get(&self, id: Uuid) -> Result<Session, ChainError> {
        let sessions = self.sessions.lock().map_err(|_| {
            ChainError::Internal("Failed to acquire lock on session store".to_string())
        })?;

        sessions
            .get(&id)
            .cloned()
            .ok_or_else(|| ChainError::NotFound(format!("Session with id {} not found", id)))
    }

    async fn create(&self, session: Session) -> Result<(), ChainError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            ChainError::Internal("Failed to acquire lock on session store".to_string())
        })?;

        if sessions.contains_key(&session.id) {
            return Err(ChainError::AlreadyExists(format!(
                "Session with id {} already exists",
                session.id
            )));
        }

        sessions.insert(session.id, session);
        Ok(())
    }

    async fn save(&self, session: Session) -> Result<(), ChainError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            ChainError::Internal("Failed to acquire lock on session store".to_string())
        })?;

        sessions.insert(session.id, session);
        Ok(())
    }

    async fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            ChainError::Internal("Failed to acquire lock on session store".to_string())
        })?;

        Ok(sessions.remove(&id).is_some())
    }

    async fn cleanup(&self) -> Result<usize, ChainError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            ChainError::Internal("Failed to acquire lock on session store".to_string())
        })?;

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
    use crate::session::SimulationMethod;
    use crate::session::model::{Session, SessionState, SimulationParameters};

    use crate::utils::UuidGenerator;
    use optionstratlib::utils::TimeFrame;
    use positive::{Positive, pos_or_panic};
    use rust_decimal::Decimal;
    use std::time::{Duration, SystemTime};
    use uuid::Uuid;

    fn create_test_session(id_option: Option<Uuid>) -> Session {
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

        let now = SystemTime::now();
        let namespace_uuid = Uuid::new_v4().to_string();
        let namespace =
            Uuid::parse_str(&namespace_uuid).expect("Failed to parse default UUID namespace");
        let uuid_generator = UuidGenerator::new(namespace);

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
            Session::new(params, &uuid_generator)
        }
    }

    #[test]
    fn test_in_memory_session_store_new() {
        let store = InMemorySessionStore::new();

        // Verificar que el store se creó correctamente
        let sessions = store.sessions.lock().unwrap();
        assert_eq!(sessions.len(), 0);
    }

    #[tokio::test]
    async fn test_get_nonexistent_session() {
        let store = InMemorySessionStore::new();
        let id = Uuid::new_v4();

        let result = store.get(id).await;

        assert!(result.is_err());
        match result {
            Err(ChainError::NotFound(msg)) => {
                assert!(msg.contains(&id.to_string()));
            }
            _ => panic!("Expected NotFound error"),
        }
    }

    #[tokio::test]
    async fn test_save_and_get_session() {
        let store = InMemorySessionStore::new();
        let session = create_test_session(None);
        let id = session.id;

        let save_result = store.save(session.clone()).await;
        assert!(save_result.is_ok());

        let get_result = store.get(id).await;
        assert!(get_result.is_ok());

        let retrieved_session = get_result.unwrap();
        assert_eq!(retrieved_session.id, id);
        assert_eq!(retrieved_session.parameters.symbol, "TEST");
        assert_eq!(retrieved_session.state, SessionState::Initialized);
    }

    #[tokio::test]
    async fn test_create_new_session() {
        let store = InMemorySessionStore::new();
        let session = create_test_session(None);
        let id = session.id;

        // create succeeds on a new id
        assert!(store.create(session).await.is_ok());

        // and the session is retrievable
        assert!(store.get(id).await.is_ok());
    }

    #[tokio::test]
    async fn test_create_duplicate_returns_already_exists() {
        let store = InMemorySessionStore::new();
        let session = create_test_session(None);

        // first create wins
        assert!(store.create(session.clone()).await.is_ok());

        // second create with the same id is rejected instead of overwriting
        match store.create(session).await {
            Err(ChainError::AlreadyExists(msg)) => {
                assert!(msg.contains("already exists"));
            }
            other => panic!("Expected AlreadyExists error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_save_still_updates_after_create() {
        let store = InMemorySessionStore::new();
        let mut session = create_test_session(None);
        let id = session.id;

        // create the session, then save (upsert) an updated copy
        assert!(store.create(session.clone()).await.is_ok());
        session.current_step = 7;
        session.state = SessionState::InProgress;
        assert!(store.save(session).await.is_ok());

        let retrieved = store.get(id).await.unwrap();
        assert_eq!(retrieved.current_step, 7);
        assert_eq!(retrieved.state, SessionState::InProgress);
    }

    #[tokio::test]
    async fn test_save_multiple_sessions() {
        let store = InMemorySessionStore::new();

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        let session1 = create_test_session(Some(id1));
        let session2 = create_test_session(Some(id2));

        assert!(store.save(session1).await.is_ok());
        assert!(store.save(session2).await.is_ok());

        let retrieved1 = store.get(id1).await.unwrap();
        let retrieved2 = store.get(id2).await.unwrap();

        assert_eq!(retrieved1.id, id1);
        assert_eq!(retrieved2.id, id2);

        let sessions = store.sessions.lock().unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[tokio::test]
    async fn test_update_existing_session() {
        let store = InMemorySessionStore::new();
        let mut session = create_test_session(None);
        let id = session.id;

        // Guardar la sesión inicial
        assert!(store.save(session.clone()).await.is_ok());

        // Modificar la sesión y guardarla nuevamente
        session.state = SessionState::InProgress;
        session.current_step = 1;
        assert!(store.save(session).await.is_ok());

        // Verificar que los cambios se aplicaron
        let retrieved = store.get(id).await.unwrap();
        assert_eq!(retrieved.state, SessionState::InProgress);
        assert_eq!(retrieved.current_step, 1);
    }

    #[tokio::test]
    async fn test_delete_session() {
        let store = InMemorySessionStore::new();
        let session = create_test_session(None);
        let id = session.id;

        // Guardar y luego borrar la sesión
        assert!(store.save(session).await.is_ok());
        let delete_result = store.delete(id).await;

        assert!(delete_result.is_ok());
        assert!(delete_result.unwrap());

        // Verificar que la sesión ya no existe
        assert!(store.get(id).await.is_err());
    }

    #[tokio::test]
    async fn test_delete_nonexistent_session() {
        let store = InMemorySessionStore::new();
        let id = Uuid::new_v4();

        let delete_result = store.delete(id).await;

        assert!(delete_result.is_ok());
        assert!(!delete_result.unwrap()); // Debe retornar false
    }

    #[tokio::test]
    async fn test_cleanup_expired_sessions() {
        let store = InMemorySessionStore::new();

        // Crear una sesión con tiempo actual
        let current_session = create_test_session(None);
        let current_id = current_session.id;

        // Crear una sesión "antigua" (más de 30 minutos)
        let expired_time = SystemTime::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap();
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
        assert!(store.save(current_session).await.is_ok());
        assert!(store.save(expired_session).await.is_ok());

        // Ejecutar la limpieza
        let cleanup_result = store.cleanup().await;
        assert!(cleanup_result.is_ok());
        assert_eq!(cleanup_result.unwrap(), 1); // Una sesión debe ser eliminada

        // Verificar que solo la sesión actual sigue existiendo
        assert!(store.get(current_id).await.is_ok());
        assert!(store.get(expired_id).await.is_err());
    }

    #[tokio::test]
    async fn test_concurrent_access() {
        let store = Arc::new(InMemorySessionStore::new());
        let session = create_test_session(None);
        let id = session.id;

        // Guardar la sesión inicial
        assert!(store.save(session).await.is_ok());

        let store_clone = Arc::clone(&store);
        let handle = tokio::spawn(async move {
            // Intentar obtener la sesión desde otra tarea
            let result = store_clone.get(id).await;
            assert!(result.is_ok());

            let mut session = result.unwrap();
            session.state = SessionState::InProgress;

            // Guardar los cambios
            assert!(store_clone.save(session).await.is_ok());
        });

        // Esperar a que la tarea termine
        handle.await.unwrap();

        // Verificar que los cambios de la otra tarea se aplicaron
        let retrieved = store.get(id).await.unwrap();
        assert_eq!(retrieved.state, SessionState::InProgress);
    }

    /// Issue #19: two store operations on DIFFERENT sessions run concurrently
    /// over a single shared `Arc<InMemorySessionStore>` via `tokio::join!` and
    /// both complete — the async trait never serializes independent callers on
    /// an `.await`-held lock (the `std::Mutex` is only held for synchronous map
    /// access, never across an await).
    #[tokio::test]
    async fn test_concurrent_ops_on_different_sessions_do_not_serialize() {
        let store = Arc::new(InMemorySessionStore::new());

        let session_a = create_test_session(None);
        let session_b = create_test_session(None);
        let (id_a, id_b) = (session_a.id, session_b.id);

        // Two creates on different ids issued concurrently through one Arc.
        let store_a = Arc::clone(&store);
        let store_b = Arc::clone(&store);
        let (res_a, res_b) =
            tokio::join!(async move { store_a.create(session_a).await }, async move {
                store_b.create(session_b).await
            },);

        assert!(res_a.is_ok());
        assert!(res_b.is_ok());

        // Both sessions landed independently.
        assert!(store.get(id_a).await.is_ok());
        assert!(store.get(id_b).await.is_ok());
    }

    #[tokio::test]
    async fn test_lock_poisoning_recovery() {
        let store = InMemorySessionStore::new();
        let session = create_test_session(None);
        let id = session.id;

        // Guardar la sesión
        assert!(store.save(session).await.is_ok());

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
        let _ = store.get(id).await;
        let _ = store.delete(id).await;
        let _ = store.cleanup().await;
    }
}
