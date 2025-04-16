use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::session::model::Session;
use crate::utils::error::ChainError;

/// Interface for session storage implementations
pub trait SessionStore: Send + Sync {
    fn get(&self, id: Uuid) -> Result<Session, ChainError>;
    fn save(&self, session: Session) -> Result<(), ChainError>;
    fn delete(&self, id: Uuid) -> Result<bool, ChainError>;
    fn cleanup(&self) -> Result<usize, ChainError>;
}

/// In-memory implementation of SessionStore
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
        let sessions = self.sessions.lock().map_err(|_| {
            ChainError::Internal("Failed to acquire lock on session store")
        })?;

        sessions.get(&id).cloned().ok_or_else(|| {
            ChainError::NotFound(format!("Session with id {} not found", id))
        })
    }

    fn save(&self, session: Session) -> Result<(), ChainError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            ChainError::Internal("Failed to acquire lock on session store")
        })?;

        sessions.insert(session.id, session);
        Ok(())
    }

    fn delete(&self, id: Uuid) -> Result<bool, ChainError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            ChainError::Internal("Failed to acquire lock on session store")
        })?;

        Ok(sessions.remove(&id).is_some())
    }

    fn cleanup(&self) -> Result<usize, ChainError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            ChainError::Internal("Failed to acquire lock on session store")
        })?;

        // Find expired sessions (older than 30 minutes)
        let now = std::time::SystemTime::now();
        let expired_ids: Vec<Uuid> = sessions
            .iter()
            .filter_map(|(id, session)| {
                match now.duration_since(session.updated_at) {
                    Ok(duration) if duration.as_secs() > 1800 => Some(*id),
                    _ => None,
                }
            })
            .collect();

        // Remove expired sessions
        let count = expired_ids.len();
        for id in expired_ids {
            sessions.remove(&id);
        }

        Ok(count)
    }
}