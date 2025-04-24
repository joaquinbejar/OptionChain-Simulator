pub(crate) mod controller;
mod error;
mod favicon;
pub(crate) mod handlers;
mod middleware;
pub(crate) mod models;
pub(crate) mod requests;
pub(crate) mod responses;
mod routes;
pub mod swagger;

pub(crate) use favicon::get_favicon;
