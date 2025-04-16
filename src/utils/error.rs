use std::error::Error;
use std::fmt;

#[derive(Debug, Clone)]
pub enum ChainError<'a> {
    SessionError(&'a str),
    StdError(String),
    InvalidState(&'a str),
    Internal(&'a str),
    NotFound(String),
}

impl<'a> From<&'a str> for ChainError<'a> {
    fn from(msg: &'a str) -> Self {
        ChainError::StdError(msg.to_string())
    }
}

impl From<String> for ChainError<'_> {
    fn from(msg: String) -> Self {
        ChainError::StdError(msg)
    }
}



impl fmt::Display for ChainError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChainError::SessionError(msg) => write!(f, "Session Error: {}", msg),
            ChainError::StdError(msg) => write!(f, "Std Error: {}", msg),
            ChainError::InvalidState(msg) => write!(f, "Invalid State: {}", msg),
            ChainError::Internal(msg) => write!(f, "Internal Error: {}", msg),
            ChainError::NotFound(msg) => write!(f, "Not Found: {}", msg),
        }
        
    }
}

impl Error for ChainError<'_> {}