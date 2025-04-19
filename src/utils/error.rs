use std::error::Error;
use std::fmt;

/// `ChainError` is an enumeration that represents various kinds of errors that can occur within the context of the application.
///
/// This enum is marked with the `#[derive(Debug, Clone)]` attribute, enabling the implementation of the `Debug` and `Clone` traits.
///
/// # Usage
/// This enum can be used to differentiate between various error types and handle them appropriately in the application.
///
#[derive(Debug, Clone)]
pub enum ChainError {
    /// - `SessionError(String)`
    ///   Represents an error related to a session. Contains a `String` providing additional information about the session error.
    SessionError(String),
    /// - `StdError(String)`
    ///   Represents an error originating from standard library usage. Accepts a `String` for detailed error information.
    StdError(String),
    /// - `InvalidState(String)`
    ///   Indicates the occurrence of an invalid state during execution. Contains a `String` with details about the invalid state.
    InvalidState(String),
    /// - `Internal(String)`
    ///   Represents an internal error within the application. Additional information is provided via a `String`.
    Internal(String),
    /// - `NotFound(String)`
    ///   Denotes that a requested resource or item was not found. Includes a `String` detailing what was not found.
    NotFound(String),
    /// - `SimulatorError(String)`
    ///   Indicates an error specific to a simulation process. Provides a `String` for more context.
    SimulatorError(String),
    /// - `ClickHouseError(String)`
    ///   Represents errors related to operations with the ClickHouse database. Contains a `String` with error details.
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
