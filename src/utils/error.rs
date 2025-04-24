use std::error::Error;
use std::fmt;
use tokio::task::JoinError;

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
    /// - `NotEnoughData(String)`
    ///   Represents errors when there isn't enough data to fulfill the requested steps.
    NotEnoughData(String),
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

impl From<clickhouse::error::Error> for ChainError {
    fn from(err: clickhouse::error::Error) -> Self {
        ChainError::ClickHouseError(err.to_string())
    }
}

impl From<JoinError> for ChainError {
    fn from(err: JoinError) -> Self {
        ChainError::ClickHouseError(err.to_string())
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
            ChainError::NotEnoughData(msg) => write!(f, "Not Enough Data: {}", msg),
        }
    }
}

impl Error for ChainError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::io;

    // Test conversion from &str
    #[test]
    fn test_from_str() {
        let error: ChainError = "test error".into();
        assert!(matches!(error, ChainError::StdError(msg) if msg == "test error"));
    }

    // Test conversion from String
    #[test]
    fn test_from_string() {
        let error_msg = "test error".to_string();
        let error: ChainError = error_msg.clone().into();
        assert!(matches!(error, ChainError::StdError(msg) if msg == error_msg));
    }

    // Test conversion from std::io::Error
    #[test]
    fn test_from_io_error() {
        let io_error = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let error: ChainError = io_error.into();
        assert!(matches!(error, ChainError::StdError(msg) if msg == "file not found"));
    }

    // Test conversion from Box<dyn Error>
    #[test]
    fn test_from_boxed_error() {
        let boxed_error: Box<dyn Error> =
            Box::new(io::Error::new(io::ErrorKind::Other, "generic error"));
        let error: ChainError = boxed_error.into();
        assert!(matches!(error, ChainError::StdError(msg) if msg == "generic error"));
    }

    // Test conversion from clickhouse::error::Error
    #[test]
    fn test_from_clickhouse_error() {
        // Note: You'll need to mock a clickhouse::error::Error for this test
        // This is a simplified example
        #[derive(Debug)]
        struct MockClickHouseError;

        impl std::fmt::Display for MockClickHouseError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "mock clickhouse error")
            }
        }

        impl std::error::Error for MockClickHouseError {}

        impl From<MockClickHouseError> for ChainError {
            fn from(_err: MockClickHouseError) -> Self {
                ChainError::ClickHouseError("mock clickhouse error".to_string())
            }
        }

        let ch_error = MockClickHouseError;
        let error: ChainError = ch_error.into();
        assert!(
            matches!(error, ChainError::ClickHouseError(msg) if msg == "mock clickhouse error")
        );
    }

    // Test Display trait implementation
    #[test]
    fn test_display_trait() {
        let test_cases = vec![
            (
                ChainError::SessionError("session problem".to_string()),
                "Session Error: session problem",
            ),
            (
                ChainError::StdError("standard error".to_string()),
                "Std Error: standard error",
            ),
            (
                ChainError::InvalidState("invalid state".to_string()),
                "Invalid State: invalid state",
            ),
            (
                ChainError::Internal("internal error".to_string()),
                "Internal Error: internal error",
            ),
            (
                ChainError::NotFound("resource missing".to_string()),
                "Not Found: resource missing",
            ),
            (
                ChainError::SimulatorError("simulation failed".to_string()),
                "Simulator Error: simulation failed",
            ),
            (
                ChainError::ClickHouseError("database error".to_string()),
                "ClickHouse Error: database error",
            ),
            (
                ChainError::NotEnoughData("insufficient data points".to_string()),
                "Not Enough Data: insufficient data points",
            ),
        ];

        for (error, expected_str) in test_cases {
            assert_eq!(error.to_string(), expected_str);
        }
    }

    // Test Error trait implementation
    #[test]
    fn test_error_trait() {
        let error = ChainError::SessionError("test error".to_string());

        // Check that it can be used as a standard Error
        let _: &dyn Error = &error;

        // Verify description is non-empty
        assert!(!error.to_string().is_empty());
    }

    // Comprehensive pattern matching test
    #[test]
    fn test_error_variants() {
        let errors = vec![
            ChainError::SessionError("session issue".to_string()),
            ChainError::StdError("standard issue".to_string()),
            ChainError::InvalidState("invalid state".to_string()),
            ChainError::Internal("internal issue".to_string()),
            ChainError::NotFound("not found".to_string()),
            ChainError::SimulatorError("simulation problem".to_string()),
            ChainError::ClickHouseError("database issue".to_string()),
            ChainError::NotEnoughData("insufficient data".to_string()),
        ];

        for error in errors {
            match error {
                ChainError::SessionError(msg) => assert_eq!(msg, "session issue"),
                ChainError::StdError(msg) => assert_eq!(msg, "standard issue"),
                ChainError::InvalidState(msg) => assert_eq!(msg, "invalid state"),
                ChainError::Internal(msg) => assert_eq!(msg, "internal issue"),
                ChainError::NotFound(msg) => assert_eq!(msg, "not found"),
                ChainError::SimulatorError(msg) => assert_eq!(msg, "simulation problem"),
                ChainError::ClickHouseError(msg) => assert_eq!(msg, "database issue"),
                ChainError::NotEnoughData(msg) => assert_eq!(msg, "insufficient data"),
            }
        }
    }
}
