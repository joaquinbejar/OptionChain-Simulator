use std::error::Error;
use std::fmt;

#[derive(Debug, Clone)]
pub enum ChainError {
    SessionError(String),
    StdError(String),
    InvalidState(String),
    Internal(String),
    NotFound(String),
    SimulatorError(String),
    ClickHouseError(String),
}

impl<'a> From<&'a str> for ChainError {
    fn from(msg: &'a str) -> Self {
        ChainError::StdError(msg.to_string())
    }
}

impl From<String> for ChainError {
    fn from(msg: String) -> Self {
        ChainError::StdError(msg)
    }
}

impl From<std::io::Error> for ChainError {
    fn from(err: std::io::Error) -> Self {
        ChainError::StdError(err.to_string())
    }   
}

impl From<Box<dyn Error>> for ChainError {
     fn from(err: Box<dyn Error>) -> Self {
         ChainError::StdError(err.to_string())   
     }
}

impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChainError::SessionError(msg) => write!(f, "Session Error: {}", msg),
            ChainError::StdError(msg) => write!(f, "Std Error: {}", msg),
            ChainError::InvalidState(msg) => write!(f, "Invalid State: {}", msg),
            ChainError::Internal(msg) => write!(f, "Internal Error: {}", msg),
            ChainError::NotFound(msg) => write!(f, "Not Found: {}", msg),
            ChainError::SimulatorError(msg) => write!(f, "Simulator Error: {}", msg),
            ChainError::ClickHouseError(msg) => write!(f, "ClickHouse Error: {}", msg),
        }
        
    }
}

impl Error for ChainError {}