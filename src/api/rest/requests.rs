use optionstratlib::utils::TimeFrame;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use crate::api::rest::models::{ApiTimeFrame, ApiWalkType};
use crate::session::SimulationParameters;

/// Represents a request to create a new simulation session.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    /// - `symbol` (`String`): The name or ticker symbol of the asset being simulated.
    pub symbol: String,
    /// - `steps` (`usize`): The number of discrete time steps or intervals in the simulation process.
    pub steps: usize,
    /// - `initial_price` (`Positive`): The initial starting price of the asset. This must be a positive value.
    pub initial_price: f64,
    /// - `days_to_expiration` (`Positive`): The number of days until the expiration of the asset or contract. This must be a positive value.
    pub days_to_expiration: f64,
    /// - `volatility` (`Positive`): The expected volatility (standard deviation) of the asset's returns.
    pub volatility: f64,
    /// - `risk_free_rate` (`Decimal`): The risk-free rate of return, typically represented as an annualized percentage.
    pub risk_free_rate: f64,
    /// - `dividend_yield` (`Positive`): The annualized dividend yield of the asset, expressed as a positive value.
    pub dividend_yield: f64,
    /// - `method` (`SimulationMethod`): The simulation method or algorithm to be used, defining the behavior of the simulation process.
    pub method: ApiWalkType,
    /// - `time_frame` (`TimeFrame`): The time frame for the simulation intervals, such as daily, weekly, or hourly.
    pub time_frame: ApiTimeFrame,
    /// - `chain_size` (`Option<usize>`): The optional size of the option chain being simulated. If `None`, this is not specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_size: Option<usize>,
    /// - `strike_interval` (`Option<Positive>`): The optional interval between strike prices for options. If `None`, this is not specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strike_interval: Option<f64>,
    /// - `skew_factor` (`Option<Decimal>`): An optional factor that adjusts the skew of the distribution. For example, it can be used to bias option pricing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skew_factor: Option<f64>,
    /// - `spread` (`Option<Positive>`): An optional parameter to specify the spread value. If `None`, no spread is applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread: Option<f64>,
}

impl From<SimulationParameters> for CreateSessionRequest {
    fn from(params: SimulationParameters) -> Self {
        Self {
            symbol: params.symbol,
            steps: params.steps,
            initial_price: params.initial_price.to_f64(),
            days_to_expiration: params.days_to_expiration.to_f64(),
            volatility: params.volatility.to_f64(),
            risk_free_rate: params.risk_free_rate.to_f64().unwrap_or(0.0),
            dividend_yield: params.dividend_yield.to_f64(),
            method: params.method.into(),
            time_frame: params.time_frame.into(),
            chain_size: params.chain_size,
            strike_interval: params.strike_interval.map(|p| p.to_f64()),
            skew_factor: params.skew_factor.map(|d| d.to_f64().unwrap_or(0.0)),
            spread: params.spread.map(|p| p.to_f64()),
        }
    }
}

impl Default for CreateSessionRequest {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            steps: 20,
            initial_price: 100.0,
            days_to_expiration: 30.0,
            volatility: 0.2,
            risk_free_rate: 0.0,
            dividend_yield: 0.0,
            method: ApiWalkType::Brownian {
                dt: 1.0/252.0,
                drift: 0.0,
                volatility: 0.2,
            },
            time_frame: ApiTimeFrame::Day,
            chain_size: Some(30),
            strike_interval: Some(1.0),
            skew_factor: None,
            spread: Some(0.01),
        }
    }
}



/// Represents a request to update an existing simulation session.
/// This is a partial update, so all fields are optional.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateSessionRequest {
    /// The ticker symbol or identifier for the underlying asset
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// The initial price of the underlying asset
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_price: Option<f64>,
    /// The volatility parameter for the simulation (annualized)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volatility: Option<f64>,
    /// The risk-free interest rate (decimal, e.g., 0.05 for 5%)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_free_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strikes: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expirations: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_frame: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dividend_yield: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skew_factor: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread: Option<f64>,
}

#[cfg(test)]
mod tests_create_session_request {
    use super::*;
    use serde_json::{from_str, to_string};

    #[test]
    fn test_create_session_request_default() {
        let default_req = CreateSessionRequest::default();

        assert_eq!(default_req.symbol, "");
        assert_eq!(default_req.initial_price, 100.0);
        assert_eq!(default_req.volatility, 0.2);
        assert_eq!(default_req.risk_free_rate, 0.0);
        assert_eq!(default_req.days_to_expiration, 30.0);
        assert_eq!(default_req.dividend_yield, 0.0);
        assert_eq!(default_req.steps, 20);
        assert_eq!(default_req.time_frame, ApiTimeFrame::Day);
        assert_eq!(default_req.chain_size, Some(30));
        assert_eq!(default_req.strike_interval, Some(1.0));
        assert_eq!(default_req.skew_factor, None);
        assert_eq!(default_req.spread, Some(0.01));

        // Check method field
        match default_req.method {
            ApiWalkType::Brownian { dt, drift, volatility } => {
                assert!((dt - 1.0/252.0).abs() < 1e-6);
                assert_eq!(drift, 0.0);
                assert_eq!(volatility, 0.2);
            },
            _ => panic!("Expected default method to be Brownian"),
        }
    }

    #[test]
    fn test_create_session_request_serialization() {
        let req = CreateSessionRequest {
            symbol: "AAPL".to_string(),
            initial_price: 185.5,
            volatility: 0.25,
            risk_free_rate: 0.04,
            days_to_expiration: 45.0,
            method: ApiWalkType::GeometricBrownian {
                dt: 0.004,
                drift: 0.05,
                volatility: 0.25,
            },
            steps: 30,
            time_frame: ApiTimeFrame::Day,
            dividend_yield: 0.005,
            skew_factor: Some(0.0005),
            spread: Some(0.02),
            chain_size: Some(15),
            strike_interval: Some(5.0),
        };

        let json = to_string(&req).unwrap();

        // Check that the JSON contains expected fields
        assert!(json.contains("\"symbol\":\"AAPL\""));
        assert!(json.contains("\"initial_price\":185.5"));
        assert!(json.contains("\"volatility\":0.25"));
        assert!(json.contains("\"risk_free_rate\":0.04"));
        assert!(json.contains("\"days_to_expiration\":45.0"));
        assert!(json.contains("\"steps\":30"));
        assert!(json.contains("\"dividend_yield\":0.005"));
        assert!(json.contains("\"skew_factor\":0.0005"));
        assert!(json.contains("\"spread\":0.02"));
        assert!(json.contains("\"chain_size\":15"));
        assert!(json.contains("\"strike_interval\":5.0"));

        // Check method field
        assert!(json.contains("\"GeometricBrownian\""));
        assert!(json.contains("\"dt\":0.004"));
        assert!(json.contains("\"drift\":0.05"));

        // Check time_frame field
        assert!(json.contains("\"time_frame\":\"Day\""));
    }

    #[test]
    fn test_create_session_request_deserialization() {
        let json = r#"{
            "symbol": "TSLA",
            "initial_price": 250.0,
            "volatility": 0.4,
            "risk_free_rate": 0.035,
            "days_to_expiration": 60.0,
            "method": {
                "Brownian": {
                    "dt": 0.0027,
                    "drift": 0.02,
                    "volatility": 0.4
                }
            },
            "steps": 25,
            "time_frame": "Week",
            "dividend_yield": 0.0,
            "skew_factor": 0.001,
            "spread": 0.015,
            "chain_size": 20,
            "strike_interval": 10.0
        }"#;

        let req: CreateSessionRequest = from_str(json).unwrap();

        assert_eq!(req.symbol, "TSLA");
        assert_eq!(req.initial_price, 250.0);
        assert_eq!(req.volatility, 0.4);
        assert_eq!(req.risk_free_rate, 0.035);
        assert_eq!(req.days_to_expiration, 60.0);
        assert_eq!(req.steps, 25);
        assert_eq!(req.time_frame, ApiTimeFrame::Week);
        assert_eq!(req.dividend_yield, 0.0);
        assert_eq!(req.skew_factor, Some(0.001));
        assert_eq!(req.spread, Some(0.015));
        assert_eq!(req.chain_size, Some(20));
        assert_eq!(req.strike_interval, Some(10.0));

        // Check method field
        match req.method {
            ApiWalkType::Brownian { dt, drift, volatility } => {
                assert_eq!(dt, 0.0027);
                assert_eq!(drift, 0.02);
                assert_eq!(volatility, 0.4);
            },
            _ => panic!("Expected method to be Brownian"),
        }
    }

    #[test]
    fn test_partial_updates_create_session_request() {
        // Test with partial JSON (missing fields should use defaults)
        let json = r#"{
            "symbol": "AAPL",
            "steps": 30,
            "initial_price": 150.0,
            "volatility": 0.2,
            "risk_free_rate": 0.03,
            "dividend_yield": 0.005,
            "days_to_expiration": 30.0,
            "method": {
              "GeometricBrownian": {
                "dt": 0.004,
                "drift": 0.05,
                "volatility": 0.25
              }
            },
            "time_frame": "Day"
        }"#;

        let req: CreateSessionRequest = from_str(json).unwrap();

        // Provided fields
        assert_eq!(req.symbol, "AAPL");
        assert_eq!(req.initial_price, 150.0);
        assert_eq!(req.volatility, 0.2);
        assert_eq!(req.risk_free_rate, 0.03);
        assert_eq!(req.days_to_expiration, 30.0);


        // Default fields 
        assert_eq!(req.steps, 30); // Default value
        assert_eq!(req.time_frame, ApiTimeFrame::Day); // Default value
        assert_eq!(req.dividend_yield, 0.005); // Default value
        assert_eq!(req.skew_factor, None); // Default value
        assert_eq!(req.spread, None); // Default value

        // Method field should be default Brownian
        match req.method {
            ApiWalkType::GeometricBrownian { dt, drift, volatility } => {
                assert!((dt - 0.004).abs() < 1e-6);
                assert_eq!(drift, 0.05);
                assert_eq!(volatility, 0.25);
            },
            _ => panic!("Expected default method to be Brownian"),
        }
    }
}