//! # OptionChain-Simulator API and Architecture
//!
//! ## System Architecture
//!
//! ```mermaid
//! flowchart TD
//! Client[Client Applications] --> API[API Layer]
//! API --> SM[Session Management]
//! SM --> App[Application Layer]
//! App --> Domain[Domain Layer]
//! App --> Infra[Infrastructure Layer]
//! Domain --> SimEngine[Simulation Engine]
//! Infra --> ClickHouse[(ClickHouse DB)]
//! Infra --> Redis[(Redis Store)]
//! ```
//!
//! ## Session State Transitions
//!
//! ```mermaid
//! stateDiagram-v2
//! [*] --> Initialized: POST /api/v1/chain
//! Initialized --> InProgress: GET
//! InProgress --> InProgress: GET
//! InProgress --> Modified: PATCH
//! Modified --> InProgress: GET
//! InProgress --> Reinitialized: PUT
//! Modified --> Reinitialized: PUT
//! Reinitialized --> InProgress: GET
//! Initialized --> [*]: DELETE
//! InProgress --> [*]: DELETE
//! Modified --> [*]: DELETE
//! Reinitialized --> [*]: DELETE
//! ```
//!
//! ## API Request Flow
//!
//! ```mermaid
//! sequenceDiagram
//! participant Client
//! participant API as REST API
//! participant SM as Session Manager
//! participant SS as Simulator Service
//!
//! Client->>API: POST /api/v1/chain
//! API->>SM: Create new session
//! SM->>SS: Initialize simulation
//! SS-->>SM: Initial state
//! SM-->>API: Session created (id: abc123)
//! API-->>Client: 201 Created (session details)
//!
//! Client->>API: GET /api/v1/chain
//! API->>SM: Get next step
//! SM->>SS: Advance simulation
//! SS-->>SM: Step data
//! SM-->>API: Chain data
//! API-->>Client: 200 OK (Chain data)
//! ```
//!
//! ## REST API Endpoints
//!
//! The OptionChain-Simulator exposes the following REST API endpoints:
//!
//! | Method | Endpoint       | Action           | Description                                      |
//! |--------|---------------|------------------|--------------------------------------------------|
//! | POST   | /api/v1/chain | Create Session   | Creates a new simulation session                 |
//! | GET    | /api/v1/chain | Read Next Step   | Gets the next step in the simulation            |
//! | PUT    | /api/v1/chain | Replace Session  | Completely replaces session parameters          |
//! | PATCH  | /api/v1/chain | Update Parameters| Updates specific session parameters             |
//! | DELETE | /api/v1/chain | Delete Session   | Terminates and removes a session                 |
//!
//! ## Request/Response Models
//!
//! ### 1. Create Session (POST /api/v1/chain)
//!
//! **Request Body:**
//! ```json
//! {
//!   "symbol": "AAPL",
//!   "steps": 10,
//!   "initial_price": 185.5,
//!   "days_to_expiration": 45.0,
//!   "volatility": 0.25,
//!   "risk_free_rate": 0.04,
//!   "dividend_yield": 0.005,
//!   "method": {
//!     "GeometricBrownian": {
//!       "dt": 0.004,
//!       "drift": 0.05,
//!       "volatility": 0.25
//!     }
//!   },
//!   "time_frame": "Day",
//!   "chain_size": 15,
//!   "strike_interval": 5.0,
//!   "skew_factor": 0.0005,
//!   "spread": 0.02
//! }
//! ```
//!
//! **Response (201 Created):**
//! ```json
//! {
//!     "id": "6af613b6-569c-5c22-9c37-2ed93f31d3af",
//!     "created_at": "2025-04-21T15:37:30.518022+00:00",
//!     "updated_at": "2025-04-21T15:37:30.518022+00:00",
//!     "parameters": {
//!         "symbol": "AAPL",
//!         "initial_price": 185.5,
//!         "volatility": 0.25,
//!         "risk_free_rate": 0.04,
//!         "method": "GeometricBrownian { dt: 0.004, drift: 0.05, volatility: 0.25 }",
//!         "time_frame": "day",
//!         "dividend_yield": 0.005,
//!         "skew_factor": 0.0005,
//!         "spread": 0.02
//!     },
//!     "current_step": 0,
//!     "total_steps": 10,
//!     "state": "Initialized"
//! }
//! ```
//!
//! ### 2. Get Next Step (GET /api/v1/chain?sessionid=6af613b6-569c-5c22-9c37-2ed93f31d3af)
//!
//! **Response (200 OK):**
//! ```json
//! {
//!     "underlying": "AAPL",
//!     "timestamp": "2025-04-21T15:33:03.597061+00:00",
//!     "price": 185.299430466522,
//!     "contracts": [
//!         {
//!             "strike": 160.0,
//!             "expiration": "2025-06-05",
//!             "call": {
//!                 "bid": 26.08,
//!                 "ask": 26.1,
//!                 "mid": 26.09,
//!                 "delta": 0.9993778215543331
//!             },
//!             "put": {
//!                 "bid": null,
//!                 "ask": null,
//!                 "mid": null,
//!                 "delta": -4.2479708093406946e-6
//!             },
//!             "implied_volatility": 0.09731095458186256,
//!             "gamma": 3.121236702609213e-6
//!         },
//!         {
//!             "strike": 165.0,
//!             "expiration": "2025-06-05",
//!             "call": {
//!                 "bid": 21.14,
//!                 "ask": 21.16,
//!                 "mid": 21.15,
//!                 "delta": 0.9888998386575956
//!             },
//!             "put": {
//!                 "bid": 0.03,
//!                 "ask": 0.05,
//!                 "mid": 0.04,
//!                 "delta": -0.010482230867546823
//!             },
//!             "implied_volatility": 0.15077922021760087,
//!             "gamma": 0.0028266289100911603
//!         },
//!         {
//!             "strike": 170.0,
//!             "expiration": "2025-06-05",
//!             "call": {
//!                 "bid": 16.62,
//!                 "ask": 16.64,
//!                 "mid": 16.63,
//!                 "delta": 0.9153696474659715
//!             },
//!             "put": {
//!                 "bid": 0.49,
//!                 "ask": 0.51,
//!                 "mid": 0.5,
//!                 "delta": -0.08401242205917087
//!             },
//!             "implied_volatility": 0.1927733286389461,
//!             "gamma": 0.012279670056243013
//!         },
//!         {
//!             "strike": 175.0,
//!             "expiration": "2025-06-05",
//!             "call": {
//!                 "bid": 12.87,
//!                 "ask": 12.89,
//!                 "mid": 12.88,
//!                 "delta": 0.7964192920937592
//!             },
//!             "put": {
//!                 "bid": 1.71,
//!                 "ask": 1.73,
//!                 "mid": 1.72,
//!                 "delta": -0.2029627774313833
//!             },
//!             "implied_volatility": 0.22329327984589836,
//!             "gamma": 0.019409579420062936
//!         },
//!         {
//!             "strike": 180.0,
//!             "expiration": "2025-06-05",
//!             "call": {
//!                 "bid": 9.76,
//!                 "ask": 9.78,
//!                 "mid": 9.77,
//!                 "delta": 0.6700429413591044
//!             },
//!             "put": {
//!                 "bid": 3.57,
//!                 "ask": 3.59,
//!                 "mid": 3.58,
//!                 "delta": -0.3293391281660381
//!             },
//!             "implied_volatility": 0.24233907383845762,
//!             "gamma": 0.022910122989513254
//!         },
//!         {
//!             "strike": 185.0,
//!             "expiration": "2025-06-05",
//!             "call": {
//!                 "bid": 7.09,
//!                 "ask": 7.11,
//!                 "mid": 7.1,
//!                 "delta": 0.5468721177394451
//!             },
//!             "put": {
//!                 "bid": 5.87,
//!                 "ask": 5.89,
//!                 "mid": 5.88,
//!                 "delta": -0.45250995178569736
//!             },
//!             "implied_volatility": 0.24991071061662393,
//!             "gamma": 0.024315069945191076
//!         },
//!         {
//!             "strike": 190.0,
//!             "expiration": "2025-06-05",
//!             "call": {
//!                 "bid": 4.68,
//!                 "ask": 4.7,
//!                 "mid": 4.69,
//!                 "delta": 0.4237521134194814
//!             },
//!             "put": {
//!                 "bid": 8.45,
//!                 "ask": 8.47,
//!                 "mid": 8.46,
//!                 "delta": -0.5756299561056611
//!             },
//!             "implied_volatility": 0.24385078722742481,
//!             "gamma": 0.024638652336979393
//!         },
//!         {
//!             "strike": 195.0,
//!             "expiration": "2025-06-05",
//!             "call": {
//!                 "bid": 2.62,
//!                 "ask": 2.64,
//!                 "mid": 2.63,
//!                 "delta": 0.29452137751494756
//!             },
//!             "put": {
//!                 "bid": 11.36,
//!                 "ask": 11.38,
//!                 "mid": 11.37,
//!                 "delta": -0.7048606920101947
//!             },
//!             "implied_volatility": 0.22617927813392658,
//!             "gamma": 0.023389127623181388
//!         },
//!         {
//!             "strike": 200.0,
//!             "expiration": "2025-06-05",
//!             "call": {
//!                 "bid": 1.03,
//!                 "ask": 1.05,
//!                 "mid": 1.04,
//!                 "delta": 0.15952905609846607
//!             },
//!             "put": {
//!                 "bid": 14.75,
//!                 "ask": 14.77,
//!                 "mid": 14.76,
//!                 "delta": -0.8398530134266764
//!             },
//!             "implied_volatility": 0.19703361182603538,
//!             "gamma": 0.01891326128023662
//!         },
//!         {
//!             "strike": 205.0,
//!             "expiration": "2025-06-05",
//!             "call": {
//!                 "bid": 0.16,
//!                 "ask": 0.18,
//!                 "mid": 0.17,
//!                 "delta": 0.04271051015963935
//!             },
//!             "put": {
//!                 "bid": 18.85,
//!                 "ask": 18.87,
//!                 "mid": 18.86,
//!                 "delta": -0.9566715593655031
//!             },
//!             "implied_volatility": 0.15641378830375124,
//!             "gamma": 0.008916660747165772
//!         },
//!         {
//!             "strike": 210.0,
//!             "expiration": "2025-06-05",
//!             "call": {
//!                 "bid": null,
//!                 "ask": null,
//!                 "mid": null,
//!                 "delta": 0.0005597778266970925
//!             },
//!             "put": {
//!                 "bid": 23.66,
//!                 "ask": 23.68,
//!                 "mid": 23.67,
//!                 "delta": -0.9988222916984453
//!             },
//!             "implied_volatility": 0.10431980756707404,
//!             "gamma": 0.0002902662707065403
//!         }
//!     ],
//!     "session_info": {
//!         "id": "6af613b6-569c-5c22-9c37-2ed93f31d3af",
//!         "current_step": 1,
//!         "total_steps": 10
//!     }
//! }
//! ```
//!
//! ### 3. Update Session Parameters (PATCH /api/v1/chain?sessionid=6af613b6-569c-5c22-9c37-2ed93f31d3af)
//!
//! **Request Body:**
//! ```json
//! {
//!   "symbol": "AAPL",
//!    "initial_price": 385.5,
//!   "steps": 8,
//!   "volatility": 0.2,
//!   "risk_free_rate": 0.03,
//!   "dividend_yield": 0.005,
//!   "days_to_expiration": 30.0,
//!   "time_frame": "Day"
//! }
//! ```
//!
//! **Response (200 OK):**
//! ```json
//! {
//!     "id": "6af613b6-569c-5c22-9c37-2ed93f31d3af",
//!     "created_at": "2025-04-21T15:32:59.551486+00:00",
//!     "updated_at": "2025-04-21T15:33:19.515911+00:00",
//!     "parameters": {
//!         "symbol": "AAPL",
//!         "initial_price": 385.5,
//!         "volatility": 0.2,
//!         "risk_free_rate": 0.03,
//!         "method": "GeometricBrownian { dt: 0.004, drift: 0.05, volatility: 0.25 }",
//!         "time_frame": "day",
//!         "dividend_yield": 0.005,
//!         "skew_factor": 0.0005,
//!         "spread": 0.02
//!     },
//!     "current_step": 0,
//!     "total_steps": 30,
//!     "state": "Reinitialized"
//! }
//! ```
//!
//! ### 4. Replace Session (PUT /api/v1/chain)
//!
//! **Request Body:**
//! ```json
//! {
//!   "symbol": "AAPL",
//!   "steps": 30,
//!   "initial_price": 385.5,
//!   "days_to_expiration": 45.0,
//!   "volatility": 0.25,
//!   "risk_free_rate": 0.04,
//!   "dividend_yield": 0.005,
//!   "method": {
//!     "GeometricBrownian": {
//!       "dt": 0.004,
//!       "drift": 0.05,
//!       "volatility": 0.25
//!     }
//!   },
//!   "time_frame": "Day",
//!   "chain_size": 15,
//!   "strike_interval": 5.0,
//!   "skew_factor": 0.0005,
//!   "spread": 0.02
//! }
//! ```
//!
//! **Response (200 OK):**
//! ```json
//! {
//!     "id": "6af613b6-569c-5c22-9c37-2ed93f31d3af",
//!     "created_at": "2025-04-21T15:37:30.518022+00:00",
//!     "updated_at": "2025-04-21T15:37:33.951540+00:00",
//!     "parameters": {
//!         "symbol": "AAPL",
//!         "initial_price": 385.5,
//!         "volatility": 0.25,
//!         "risk_free_rate": 0.04,
//!         "method": "GeometricBrownian { dt: 0.004, drift: 0.05, volatility: 0.25 }",
//!         "time_frame": "day",
//!         "dividend_yield": 0.005,
//!         "skew_factor": 0.0005,
//!         "spread": 0.02
//!     },
//!     "current_step": 0,
//!     "total_steps": 30,
//!     "state": "Reinitialized"
//! }
//! ```
//!
//! ### 5. Delete Session (DELETE /api/v1/chain?sessionid=6af613b6-569c-5c22-9c37-2ed93f31d3af)
//!
//! **Response (200 OK):**
//! ```json
//! {
//!     "message": "Session deleted successfully: 6af613b6-569c-5c22-9c37-2ed93f31d3af",
//!     "session_id": "6af613b6-569c-5c22-9c37-2ed93f31d3af"
//! }
//! ```
//!
//! ## Domain Models
//!
//! ```mermaid
//! classDiagram
//! class SessionManager {
//! +createSession(params) Session
//! +getNextStep(id) (Session, OptionChain)
//! +updateSession(id, params) Session
//! +reinitializeSession(id, params) Session
//! +deleteSession(id) bool
//! }
//!
//! class Session {
//! +id UUID
//! +createdAt DateTime
//! +updatedAt DateTime
//! +parameters SimulationParameters
//! +currentStep usize
//! +totalSteps usize
//! +state SessionState
//! +advanceStep() Result
//! +modifyParameters(params)
//! +reinitialize(params, steps)
//! }
//!
//! class SessionState {
//! <<enumeration>>
//! Initialized
//! InProgress
//! Modified
//! Reinitialized
//! Completed
//! Error
//! }
//!
//! class SimulationParameters {
//! +symbol String
//! +initialPrice Positive
//! +volatility Positive
//! +riskFreeRate Decimal
//! +strikes Vec~Positive~
//! +expirations Vec~String~
//! +method SimulationMethod
//! +timeFrame TimeFrame
//! }
//!
//! class Simulator {
//! +simulateNextStep(session) OptionChain
//! -createRandomWalk(session) RandomWalk
//! }
//!
//! class OptionChain {
//! +underlying String
//! +timestamp DateTime
//! +price Positive
//! +contracts Vec~OptionContract~
//! }
//!
//! class OptionContract {
//! +strike Positive
//! +expiration String
//! +call OptionData
//! +put OptionData
//! +impliedVolatility Positive
//! +gamma Positive
//! }
//!
//! Session --> SimulationParameters
//! Session --> SessionState
//! SessionManager --> Session: manages
//! SessionManager --> Simulator: uses
//! Simulator --> OptionChain: produces
//! OptionChain --> OptionContract: contains
//! ```
//!
//! ## Infrastructure Components
//!
//! ```mermaid
//! classDiagram
//! class SessionStore {
//! <<interface>>
//! +get(id) Session
//! +save(session) void
//! +delete(id) bool
//! +cleanup() int
//! }
//!
//! class InMemorySessionStore {
//! -sessions Map~UUID, Session~
//! +get(id) Session
//! +save(session) void
//! +delete(id) bool
//! +cleanup() int
//! }
//!
//! class RedisSessionStore {
//! -client RedisClient
//! +get(id) Session
//! +save(session) void
//! +delete(id) bool
//! +cleanup() int
//! }
//!
//! class HistoricalDataRepository {
//! <<interface>>
//! +getHistoricalPrices(symbol, timeframe, startDate, endDate) Vec~Positive~
//! +listAvailableSymbols() Vec~String~
//! +getDateRangeForSymbol(symbol) (DateTime, DateTime)
//! }
//!
//! class ClickHouseHistoricalRepository {
//! -client ClickHouseClient
//! +getHistoricalPrices(symbol, timeframe, startDate, endDate) Vec~Positive~
//! +listAvailableSymbols() Vec~String~
//! +getDateRangeForSymbol(symbol) (DateTime, DateTime)
//! }
//!
//! SessionStore <|.. InMemorySessionStore: implements
//! SessionStore <|.. RedisSessionStore: implements
//! HistoricalDataRepository <|.. ClickHouseHistoricalRepository: implements
//! ```
//!
//! ## Makefile Commands for Development
//!
//! The project includes a Makefile with useful commands for development:
//!
//! | Command | Description |
//! |---------|-------------|
//! | `make build` | Builds the project |
//! | `make release` | Builds the project in release mode |
//! | `make test` | Runs all tests |
//! | `make fmt` | Formats the code using rustfmt |
//! | `make lint` | Runs clippy for linting |
//! | `make check` | Runs tests, formatting check, and linting |
//! | `make run` | Runs the project |
//! | `make clean` | Cleans build artifacts |
//! | `make doc` | Generates documentation |
//! | `make coverage` | Generates code coverage report |
//! | `make bench` | Runs benchmarks |
//!
//! Additional commands for CI/CD and deployment:
//!
//! | Command | Description |
//! |---------|-------------|
//! | `make pre-push` | Runs fixes, formatting, linting, and tests before pushing |
//! | `make workflow` | Runs all GitHub Actions workflows locally |
//! | `make publish` | Publishes the package to crates.io |
//! | `make zip` | Creates a ZIP archive of the project |
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
