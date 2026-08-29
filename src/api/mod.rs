pub(crate) mod rest;

pub use rest::controller::start_server;
// Re-exported from `infrastructure`, where the type lives: `optionchain_simulator
// ::api::ListenOn` is the path consumers already use, and the move must not cost
// them an import.
pub use crate::infrastructure::ListenOn;
// The readiness probe surface: a binary assembles `Readiness` from the
// dependencies it actually configured, so the list lives outside this crate's
// wiring.
// The probe response shapes. What is probed lives in `infrastructure`, beside
// the clients it calls; these are the bodies the endpoints render.
pub use rest::health::{
    DependencyState, DependencyStatus, HealthResponse, ReadinessResponse, ReadinessState,
};
pub use rest::patch::Patch;
pub use rest::requests::{CreateSessionRequest, UpdateSessionRequest};
pub use rest::requests_v2::CreateSimulationRequest;
// The greek payload the response DTOs carry. Re-exported beside them: a
// consumer that can see `OptionPriceResponse.greeks` must be able to name its
// type to match on it or build one.
pub use rest::greeks::{FirstOrderGreeks, FullGreeks, GreeksResponse};
pub use rest::responses::*;
