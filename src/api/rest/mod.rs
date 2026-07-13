pub(crate) mod controller;
mod error;
mod favicon;
pub(crate) mod handlers;
pub(crate) mod limits;
mod middleware;
pub(crate) mod models;
pub(crate) mod requests;
pub(crate) mod responses;
mod routes;
pub mod swagger;
pub(crate) mod validation;

pub(crate) use favicon::get_favicon;
