use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

/// Response containing session information.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct SessionResponse {
    /// The unique identifier for the session
    pub id: String,
    /// When the session was created
    pub created_at: String,
    /// When the session was last updated
    pub updated_at: String,
    /// The current simulation parameters
    pub parameters: SessionParametersResponse,
    /// The current step in the simulation
    pub current_step: usize,
    /// The total number of steps
    pub total_steps: usize,
    /// The current state of the session
    pub state: String,
}

/// Nested response with simulation parameters.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionParametersResponse {
    /// The ticker symbol or identifier for the underlying asset
    pub symbol: String,
    /// The initial price of the underlying asset
    pub initial_price: f64,
    /// The volatility parameter for the simulation (annualized)
    pub volatility: f64,
    /// The risk-free interest rate (decimal, e.g., 0.05 for 5%)
    pub risk_free_rate: f64,
    /// The simulation method to use (e.g., "Brownian", "GeometricBrownian")
    pub method: Value,
    /// The time frame for simulation steps
    pub time_frame: String,
    /// The dividend yield of the underlying asset
    pub dividend_yield: f64,
    /// Factor for adjusting volatility skew
    pub skew_slope: Option<f64>,
    /// Factor for adjusting volatility skew
    pub smile_curve: Option<f64>,
    /// Bid-ask spread factor
    pub spread: Option<f64>,
}

/// Response containing option chain data directly using OptionChain from OptionStratLib
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChainResponse {
    /// The underlying asset's identifier
    pub underlying: String,
    /// The timestamp when the chain was generated
    pub timestamp: String,
    /// The current price of the underlying asset
    pub price: f64,
    /// The list of option contracts in the chain
    pub contracts: Vec<OptionContractResponse>,
    /// Information about the session that generated this chain
    pub session_info: SessionInfoResponse,
}

/// Option contract data response that maps to the OptionStratLib's OptionData
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OptionContractResponse {
    /// The strike price of the option
    pub strike: f64,
    /// The expiration date in ISO 8601 format
    pub expiration: String,
    /// Call option information
    pub call: OptionPriceResponse,
    /// Put option information
    pub put: OptionPriceResponse,
    /// The implied volatility of the option
    pub implied_volatility: Option<f64>,
    /// The gamma of the option (same for both call and put)
    pub gamma: Option<f64>,
}

/// Price data for a specific option type (call or put)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct OptionPriceResponse {
    /// The bid price for the option
    pub bid: Option<f64>,
    /// The ask price for the option
    pub ask: Option<f64>,
    /// The mid-market price (average of bid and ask)
    pub mid: Option<f64>,
    /// The delta of the option
    pub delta: Option<f64>,
}

/// Simplified session information for inclusion in chain responses.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct SessionInfoResponse {
    /// The unique identifier for the session
    pub id: String,
    /// The current step in the simulation
    pub current_step: usize,
    /// The total number of steps
    pub total_steps: usize,
}

/// Represents an error response structure used in the application.
///
/// This struct is typically used to standardize the format of error messages
/// returned by APIs or other components in the system.
///
///
/// # Traits
/// - `Debug`: Enables formatting the struct using the `{:?}` formatter for debugging purposes.
/// - `Clone`: Allows creating a copy of the struct.
/// - `Serialize`: Enables the struct to be serialized, often used to convert it into JSON.
/// - `Deserialize`: Allows the struct to be deserialized, commonly used when parsing input like JSON.
/// - `ToSchema`: Used to integrate with OpenAPI documentation generators (for instance, `utoipa`).
/// - `Default`: Provides a default implementation for the struct, which initializes `error` as an empty string.
///
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct ErrorResponse {
    /// - `error`: A `String` that contains a description or message for the error.
    ///   This is intended to provide clarity about what went wrong.
    pub error: String,
}

/// Default implementation for SessionParametersResponse
impl Default for SessionParametersResponse {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            initial_price: 0.0,
            volatility: 0.0,
            risk_free_rate: 0.0,
            method: Value::Null,
            time_frame: String::new(),
            dividend_yield: 0.0,
            skew_slope: None,
            smile_curve: None,
            spread: None,
        }
    }
}

impl Default for OptionContractResponse {
    fn default() -> Self {
        Self {
            strike: 0.0,
            expiration: String::new(),
            call: OptionPriceResponse::default(),
            put: OptionPriceResponse::default(),
            implied_volatility: None,
            gamma: None,
        }
    }
}

impl Default for ChainResponse {
    fn default() -> Self {
        Self {
            underlying: String::new(),
            timestamp: String::new(),
            price: 0.0,
            contracts: Vec::new(),
            session_info: SessionInfoResponse::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn default_session_parameters() {
        let sp = SessionParametersResponse::default();
        assert_eq!(sp.symbol, "");
        assert_eq!(sp.initial_price, 0.0);
        assert_eq!(sp.volatility, 0.0);
        assert_eq!(sp.risk_free_rate, 0.0);
        assert_eq!(sp.method, Value::Null);
        assert_eq!(sp.time_frame, "");
        assert_eq!(sp.dividend_yield, 0.0);
        assert!(sp.smile_curve.is_none());
        assert!(sp.spread.is_none());
    }

    #[test]
    fn default_session_info() {
        let si = SessionInfoResponse::default();
        assert_eq!(si.id, "");
        assert_eq!(si.current_step, 0);
        assert_eq!(si.total_steps, 0);
    }

    #[test]
    fn default_option_price() {
        let op = OptionPriceResponse::default();
        assert!(op.bid.is_none());
        assert!(op.ask.is_none());
        assert!(op.mid.is_none());
        assert!(op.delta.is_none());
    }

    #[test]
    fn default_option_contract() {
        let oc = OptionContractResponse::default();
        assert_eq!(oc.strike, 0.0);
        assert_eq!(oc.expiration, "");
        assert!(oc.call.bid.is_none());
        assert!(oc.put.ask.is_none());
        assert!(oc.implied_volatility.is_none());
        assert!(oc.gamma.is_none());
    }

    #[test]
    fn default_chain_and_session_response() {
        let cr = ChainResponse::default();
        assert_eq!(cr.underlying, "");
        assert_eq!(cr.timestamp, "");
        assert_eq!(cr.price, 0.0);
        assert!(cr.contracts.is_empty());
        assert_eq!(cr.session_info.current_step, 0);

        let sr = SessionResponse::default();
        assert_eq!(sr.id, "");
        assert_eq!(sr.created_at, "");
        assert_eq!(sr.updated_at, "");
        assert_eq!(sr.parameters.symbol, "");
        assert_eq!(sr.current_step, 0);
        assert_eq!(sr.total_steps, 0);
        assert_eq!(sr.state, "");
    }

    #[test]
    fn default_error_response() {
        let er = ErrorResponse::default();
        // just ensure it constructs
        let _ = format!("{:?}", er);
    }
}
