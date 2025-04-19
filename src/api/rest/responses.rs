use serde::{Deserialize, Serialize};

/// Response containing session information.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionParametersResponse {
    /// The ticker symbol or identifier for the underlying asset
    pub symbol: String,
    /// The initial price of the underlying asset
    pub initial_price: f64,
    /// The volatility parameter for the simulation (annualized)
    pub volatility: f64,
    /// The risk-free interest rate (decimal, e.g., 0.05 for 5%)
    pub risk_free_rate: f64,
    /// A list of strike prices to include in the option chain
    pub strikes: Vec<f64>,
    /// A list of expiration dates in ISO 8601 format (YYYY-MM-DD)
    pub expirations: Vec<String>,
    /// The simulation method to use (e.g., "Brownian", "GeometricBrownian")
    pub method: String,
    /// The time frame for simulation steps
    pub time_frame: String,
    /// The dividend yield of the underlying asset
    pub dividend_yield: f64,
    /// Factor for adjusting volatility skew
    pub skew_factor: Option<f64>,
    /// Bid-ask spread factor
    pub spread: Option<f64>,
}

/// Response containing option chain data directly using OptionChain from OptionStratLib
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfoResponse {
    /// The unique identifier for the session
    pub id: String,
    /// The current step in the simulation
    pub current_step: usize,
    /// The total number of steps
    pub total_steps: usize,
}