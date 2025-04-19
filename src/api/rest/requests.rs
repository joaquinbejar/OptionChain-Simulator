use serde::{Deserialize, Serialize};

/// Represents a request to create a new simulation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionRequest {
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
    /// The simulation method to use (e.g., "BlackScholes", "MonteCarlo")
    pub method: String,
    /// The number of steps to simulate (optional, defaults to 20)
    #[serde(default)]
    pub steps: usize,
    /// The time frame for simulation steps (optional, defaults to "Day")
    #[serde(default)]
    pub time_frame: String,
    /// The dividend yield of the underlying asset (optional, defaults to 0)
    #[serde(default)]
    pub dividend_yield: f64,
    /// Factor for adjusting volatility skew (optional)
    pub skew_factor: Option<f64>,
    /// Bid-ask spread factor (optional, defaults to 0.01)
    #[serde(default)]
    pub spread: f64,
}

impl Default for CreateSessionRequest {
    fn default() -> Self {
        Self {
            // Debes proporcionar valores predeterminados para todos los campos
            symbol: String::new(),
            initial_price: 100.0,
            volatility: 0.2,
            risk_free_rate: 0.03,
            strikes: vec![],
            expirations: vec![],
            method: "BlackScholes".to_string(),
            steps: 20,
            time_frame: "Day".to_string(),
            dividend_yield: 0.0,
            skew_factor: None,
            spread: 0.01,
        }
    }
}

/// Represents a request to update an existing simulation session.
/// This is a partial update, so all fields are optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSessionRequest {
    /// The ticker symbol or identifier for the underlying asset
    pub symbol: Option<String>,
    /// The initial price of the underlying asset
    pub initial_price: Option<f64>,
    /// The volatility parameter for the simulation (annualized)
    pub volatility: Option<f64>,
    /// The risk-free interest rate (decimal, e.g., 0.05 for 5%)
    pub risk_free_rate: Option<f64>,
    /// A list of strike prices to include in the option chain
    pub strikes: Option<Vec<f64>>,
    /// A list of expiration dates in ISO 8601 format (YYYY-MM-DD)
    pub expirations: Option<Vec<String>>,
    /// The simulation method to use (e.g., "GeometricBrownian", "Historical")
    pub method: Option<String>,
    /// The number of steps to simulate
    pub steps: Option<usize>,
    /// The time frame for simulation steps
    pub time_frame: Option<String>,
    /// The dividend yield of the underlying asset
    pub dividend_yield: Option<f64>,
    /// Factor for adjusting volatility skew
    pub skew_factor: Option<f64>,
    /// Bid-ask spread factor
    pub spread: Option<f64>,
}