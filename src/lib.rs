//! # OptionChain-Simulator
//!
//! ## OptionChain-Simulator: RESTful Option Chain Time Simulator
//!
//! ### Table of Contents
//! 1. [Introduction](#introduction)
//! 2. [Features](#features)
//! 3. [Project Structure](#project-structure)
//! 4. [Setup Instructions](#setup-instructions)
//! 5. [API Usage](#api-usage)
//! 6. [Development](#development)
//! 7. [Contribution and Contact](#contribution-and-contact)
//!
//! ## Introduction
//!
//! **OptionChain-Simulator** is a lightweight REST API service that simulates an evolving option chain with every request. It is designed for developers building or testing trading systems, backtesters, and visual tools that depend on option data streams but want to avoid relying on live data feeds.
//!
//! ## Features
//!
//! - 📡 REST API to fetch a simulated option chain.
//! - ⏱ Each API request advances the simulation one time step.
//! - 🧮 Option pricing using Black-Scholes or configurable models.
//! - 🔄 Internal state memory with market evolution.
//! - ⚙️ Easily configurable initial parameters (IV, strikes, steps).
//! - 📦 JSON output for easy integration with other tools.
//! - 📁 Static data support (CSV/JSON-based initial chains).
//!
//!
//! ## Setup Instructions
//!
//! 1. Clone the repository:
//! ```bash
//! git clone https://github.com/joaquinbejar/OptionChain-Simulator.git
//! cd OptionChain-Simulator
//! ```
//!
//! 2. Build the project:
//! ```bash
//! cargo build --release
//! ```
//!
//! 3. Run the API server:
//! ```bash
//! cargo run
//! ```
//!
//! 4. Access the API:
//! ```http
//! GET http://localhost:8080/chain
//! ```
//!
//! ## API Usage
//!
//! ### `GET /chain`
//!
//! Returns the current option chain and advances the simulation.
//!
//! #### Response Example:
//! ```json
//! {
//!   "underlying_price": 102.5,
//!   "options": [
//!     {
//!       "strike": 100,
//!       "type": "Call",
//!       "expiration_days": 30,
//!       "implied_volatility": 0.2,
//!       "price": 4.32
//!     }
//!   ]
//! }
//! ```
//!
//! ## Development
//!
//! Run the server with:
//! ```bash
//! cargo run
//! ```
//!
//! Run tests:
//! ```bash
//! cargo test
//! ```
//!
//! Run formatting and linting:
//! ```bash
//! cargo fmt
//! cargo clippy
//! ```
//!

/// The `domain` module is intended to encapsulate and manage all the core business logic
/// and domain-specific functionality of the application.
///
/// This module acts as a boundary for the domain layer, typically containing:
/// - Structures, enums, and traits that represent core entities and value objects.
/// - Business rules and invariant logic pertaining to the domain.
/// - Interactions and transformations for the domain without leaking implementation details.
///
/// Other parts of the application (e.g., infrastructure or application layers)
/// should depend on this module to ensure a clear separation of concerns and maintain
/// a clean architecture.
///
/// The actual implementation of the `domain` module is organized within its internal code.
///
mod domain;

/// The `infrastructure` module serves as a dedicated module for providing
/// foundational support and systems required for the application.
///
/// This module typically includes components such as database connection
/// management, caching systems, configuration loading, messaging, or interfaces
/// to external systems.
///
/// It acts as the backbone of the application and ensures that all other
/// modules and functionalities can leverage these shared infrastructure
/// resources efficiently and consistently.
///
/// Usage:
/// - Define core infrastructure services here.
/// - Keep reusable, application-wide systems within this module.
/// - Encapsulate external integrations to avoid coupling them with the rest
///   of the codebase.
pub mod infrastructure;

/// The `api` module serves as a namespace for handling APIs within the project.
///
/// This module can include functionality for managing API requests, responses,
/// routing, and any other API-related tasks. Consider this module as the
/// central point to organize and define your application's API logic.
///
/// # Usage
/// Include the `api` module in your project to handle all API-related processes.
///
/// # Structure
/// You can further structure the `api` module by creating submodules
/// or defining functions and types directly within it to suit the application's needs.
///
/// Modify and extend this module as necessary to fit your implementation.
pub mod api;

/// The `session` module provides functionality for managing and maintaining
/// user sessions within the application. This module may include features such as:
///
/// - Creating and initializing sessions.
/// - Updating session state or data.
/// - Managing session expiration.
/// - Supporting user authentication or authorization workflows through sessions.
///
/// This module serves as a central location for session-related logic,
/// aiming to simplify session lifecycle management and enhance code reusability.
///
/// Modules, structs, functions, or interfaces within `session` should be used
/// to handle all operations related to session management efficiently and securely.
///
pub mod session;

/// This module `utils` serves as a container for utility functions, types,
/// and other reusable components that can be shared across different parts
/// of the application.
///
/// # Purpose
/// The `utils` module is designed to provide commonly used helper functionality,
/// simplifying the logic in other parts of the program and avoiding code duplication.
///
/// Note that the specific utility helpers and functionality provided will
/// depend on the implementation within this module.
pub mod utils;
