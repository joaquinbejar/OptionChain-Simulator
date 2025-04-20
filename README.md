<div style="text-align: center;">
<img src="https://raw.githubusercontent.com/joaquinbejar/OptionChain-Simulator/refs/heads/main/doc/images/logo.png" alt="optionchain_simulator" style="width: 100%; height: 100%;">
</div>

[![Dual License](https://img.shields.io/badge/license-MIT%20and%20Apache%202.0-blue)](./LICENSE)
[![Crates.io](https://img.shields.io/crates/v/optionchain_simulator.svg)](https://crates.io/crates/optionchain_simulator)
[![Downloads](https://img.shields.io/crates/d/optionchain_simulator.svg)](https://crates.io/crates/optionchain_simulator)
[![Stars](https://img.shields.io/github/stars/joaquinbejar/OptionChain-Simulator.svg)](https://github.com/joaquinbejar/OptionChain-Simulator/stargazers)
[![Issues](https://img.shields.io/github/issues/joaquinbejar/OptionChain-Simulator.svg)](https://github.com/joaquinbejar/OptionChain-Simulator/issues)
[![PRs](https://img.shields.io/github/issues-pr/joaquinbejar/OptionChain-Simulator.svg)](https://github.com/joaquinbejar/OptionChain-Simulator/pulls)
[![Build Status](https://img.shields.io/github/workflow/status/joaquinbejar/OptionChain-Simulator/CI)](https://github.com/joaquinbejar/OptionChain-Simulator/actions)
[![Coverage](https://img.shields.io/codecov/c/github/joaquinbejar/OptionChain-Simulator)](https://codecov.io/gh/joaquinbejar/OptionChain-Simulator)
[![Dependencies](https://img.shields.io/librariesio/github/joaquinbejar/OptionChain-Simulator)](https://libraries.io/github/joaquinbejar/OptionChain-Simulator)
[![Documentation](https://img.shields.io/badge/docs-latest-blue.svg)](https://docs.rs/optionchain_simulator)



## OptionChain-Simulator API and Architecture

Let me update the architecture details with proper Mermaid diagrams and include the REST API endpoints and JSON request/response formats.

### System Architecture

```mermaid
flowchart TD
Client[Client Applications] --> API[API Layer]
API --> SM[Session Management]
SM --> App[Application Layer]
App --> Domain[Domain Layer]
App --> Infra[Infrastructure Layer]
Domain --> SimEngine[Simulation Engine]
Infra --> ClickHouse[(ClickHouse DB)]
Infra --> Redis[(Redis Store)]

subgraph API Layer
Controllers[REST Controllers]
Middleware[Middleware]
Models[Request/Response Models]
end

subgraph Session Management
SessionMgr[Session Manager]
StateHandler[State Handler]
SessionStore[Session Store]
end
```

### Session State Transitions

```mermaid
stateDiagram-v2
[*] --> Initialized: POST /api/v1/chain
Initialized --> InProgress: GET
InProgress --> InProgress: GET
InProgress --> Modified: PATCH
Modified --> InProgress: GET
InProgress --> Reinitialized: PUT
Modified --> Reinitialized: PUT
Reinitialized --> InProgress: GET
Initialized --> [*]: DELETE
InProgress --> [*]: DELETE
Modified --> [*]: DELETE
Reinitialized --> [*]: DELETE
```

### API Request Flow

```mermaid
sequenceDiagram
participant Client
participant API as REST API
participant SM as Session Manager
participant SS as Simulator Service

Client->>API: POST /api/v1/chain
API->>SM: Create new session
SM->>SS: Initialize simulation
SS-->>SM: Initial state
SM-->>API: Session created (id: abc123)
API-->>Client: 201 Created (session details)

Client->>API: GET /api/v1/chain
API->>SM: Get next step
SM->>SS: Advance simulation
SS-->>SM: Step data
SM-->>API: Chain data
API-->>Client: 200 OK (Chain data)
```

### REST API Endpoints

The OptionChain-Simulator exposes the following REST API endpoints:

| Method | Endpoint       | Action           | Description                                      |
|--------|---------------|------------------|--------------------------------------------------|
| POST   | /api/v1/chain | Create Session   | Creates a new simulation session                 |
| GET    | /api/v1/chain | Read Next Step   | Gets the next step in the simulation            |
| PUT    | /api/v1/chain | Replace Session  | Completely replaces session parameters          |
| PATCH  | /api/v1/chain | Update Parameters| Updates specific session parameters             |
| DELETE | /api/v1/chain | Delete Session   | Terminates and removes a session                 |

### Request/Response Models

#### 1. Create Session (POST /api/v1/chain)

**Request Body:**
```json
{
"symbol": "AAPL",
"initial_price": 150.25,
"volatility": 0.2,
"risk_free_rate": 0.03,
"strikes": [140, 145, 150, 155, 160],
"expirations": ["2023-06-30", "2023-09-30"],
"method": "GeometricBrownian",
"steps": 20,
"time_frame": "Day",
"dividend_yield": 0.0,
"skew_factor": 0.0005,
"spread": 0.01
}
```

**Response (201 Created):**
```json
{
"id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
"created_at": "2023-04-15T14:30:00Z",
"updated_at": "2023-04-15T14:30:00Z",
"parameters": {
"symbol": "AAPL",
"initial_price": 150.25,
"volatility": 0.2,
"risk_free_rate": 0.03,
"strikes": [140, 145, 150, 155, 160],
"expirations": ["2023-06-30", "2023-09-30"],
"method": "GeometricBrownian",
"time_frame": "Day",
"dividend_yield": 0.0,
"skew_factor": 0.0005,
"spread": 0.01
},
"current_step": 0,
"total_steps": 20,
"state": "Initialized"
}
```

#### 2. Get Next Step (GET /api/v1/chain)

**Response (200 OK):**
```json
{
"underlying": "AAPL",
"timestamp": "2023-04-15T14:35:00Z",
"price": 151.23,
"contracts": [
{
"strike": 150.0,
"expiration": "2023-06-30",
"call": {
"bid": 5.60,
"ask": 5.74,
"mid": 5.67,
"delta": 0.58
},
"put": {
"bid": 4.25,
"ask": 4.39,
"mid": 4.32,
"delta": -0.42
},
"implied_volatility": 0.22,
"gamma": 0.04
},
// Additional contracts...
],
"session_info": {
"id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
"current_step": 1,
"total_steps": 20
}
}
```

#### 3. Update Session Parameters (PATCH /api/v1/chain)

**Request Body:**
```json
{
"volatility": 0.25,
"risk_free_rate": 0.035
}
```

**Response (200 OK):**
```json
{
"id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
"updated_at": "2023-04-15T14:45:00Z",
"parameters": {
"symbol": "AAPL",
"initial_price": 150.25,
"volatility": 0.25,
"risk_free_rate": 0.035,
"strikes": [140, 145, 150, 155, 160],
"expirations": ["2023-06-30", "2023-09-30"],
"method": "Historical",
"time_frame": "Day",
"dividend_yield": 0.0,
"skew_factor": 0.0005,
"spread": 0.01
},
"current_step": 5,
"total_steps": 20,
"state": "Modified"
}
```

#### 4. Replace Session (PUT /api/v1/chain)

**Request Body:**
```json
{
"symbol": "AAPL",
"initial_price": 155.0,
"volatility": 0.22,
"risk_free_rate": 0.04,
"strikes": [145, 150, 155, 160, 165],
"expirations": ["2023-06-30", "2023-09-30"],
"method": "Historical",
"steps": 30,
"time_frame": "Day",
"dividend_yield": 0.01,
"skew_factor": 0.0005,
"spread": 0.01
}
```

**Response (200 OK):**
```json
{
"id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
"updated_at": "2023-04-15T15:00:00Z",
"parameters": {
"symbol": "AAPL",
"initial_price": 155.0,
"volatility": 0.22,
"risk_free_rate": 0.04,
"strikes": [145, 150, 155, 160, 165],
"expirations": ["2023-06-30", "2023-09-30"],
"method": "Historical",
"time_frame": "Day",
"dividend_yield": 0.01,
"skew_factor": 0.0005,
"spread": 0.01
},
"current_step": 0,
"total_steps": 30,
"state": "Reinitialized"
}
```

#### 5. Delete Session (DELETE /api/v1/chain)

**Response (200 OK):**
```json
{
"message": "Session successfully terminated",
"id": "f47ac10b-58cc-4372-a567-0e02b2c3d479"
}
```

### Domain Models

```mermaid
classDiagram
class SessionManager {
+createSession(params) Session
+getNextStep(id) (Session, OptionChain)
+updateSession(id, params) Session
+reinitializeSession(id, params) Session
+deleteSession(id) bool
}

class Session {
+id UUID
+createdAt DateTime
+updatedAt DateTime
+parameters SimulationParameters
+currentStep usize
+totalSteps usize
+state SessionState
+advanceStep() Result
+modifyParameters(params)
+reinitialize(params, steps)
}

class SessionState {
<<enumeration>>
Initialized
InProgress
Modified
Reinitialized
Completed
Error
}

class SimulationParameters {
+symbol String
+initialPrice Positive
+volatility Positive
+riskFreeRate Decimal
+strikes Vec~Positive~
+expirations Vec~String~
+method SimulationMethod
+timeFrame TimeFrame
}

class Simulator {
+simulateNextStep(session) OptionChain
-createRandomWalk(session) RandomWalk
}

class OptionChain {
+underlying String
+timestamp DateTime
+price Positive
+contracts Vec~OptionContract~
}

class OptionContract {
+strike Positive
+expiration String
+call OptionData
+put OptionData
+impliedVolatility Positive
+gamma Positive
}

Session --> SimulationParameters
Session --> SessionState
SessionManager --> Session: manages
SessionManager --> Simulator: uses
Simulator --> OptionChain: produces
OptionChain --> OptionContract: contains
```

### Infrastructure Components

```mermaid
classDiagram
class SessionStore {
<<interface>>
+get(id) Session
+save(session) void
+delete(id) bool
+cleanup() int
}

class InMemorySessionStore {
-sessions Map~UUID, Session~
+get(id) Session
+save(session) void
+delete(id) bool
+cleanup() int
}

class RedisSessionStore {
-client RedisClient
+get(id) Session
+save(session) void
+delete(id) bool
+cleanup() int
}

class HistoricalDataRepository {
<<interface>>
+getHistoricalPrices(symbol, timeframe, startDate, endDate) Vec~Positive~
+listAvailableSymbols() Vec~String~
+getDateRangeForSymbol(symbol) (DateTime, DateTime)
}

class ClickHouseHistoricalRepository {
-client ClickHouseClient
+getHistoricalPrices(symbol, timeframe, startDate, endDate) Vec~Positive~
+listAvailableSymbols() Vec~String~
+getDateRangeForSymbol(symbol) (DateTime, DateTime)
}

SessionStore <|.. InMemorySessionStore: implements
SessionStore <|.. RedisSessionStore: implements
HistoricalDataRepository <|.. ClickHouseHistoricalRepository: implements
```

### Makefile Commands for Development

The project includes a Makefile with useful commands for development:

| Command | Description |
|---------|-------------|
| `make build` | Builds the project |
| `make release` | Builds the project in release mode |
| `make test` | Runs all tests |
| `make fmt` | Formats the code using rustfmt |
| `make lint` | Runs clippy for linting |
| `make check` | Runs tests, formatting check, and linting |
| `make run` | Runs the project |
| `make clean` | Cleans build artifacts |
| `make doc` | Generates documentation |
| `make coverage` | Generates code coverage report |
| `make bench` | Runs benchmarks |

Additional commands for CI/CD and deployment:

| Command | Description |
|---------|-------------|
| `make pre-push` | Runs fixes, formatting, linting, and tests before pushing |
| `make workflow` | Runs all GitHub Actions workflows locally |
| `make publish` | Publishes the package to crates.io |
| `make zip` | Creates a ZIP archive of the project |

This comprehensive architecture provides a solid foundation for building the OptionChain-Simulator with clear separation of concerns, well-defined interfaces, and scalable infrastructure components.



## Contribution and Contact

We welcome contributions to this project! If you would like to contribute, please follow these steps:

1. Fork the repository.
2. Create a new branch for your feature or bug fix.
3. Make your changes and ensure that the project still builds and all tests pass.
4. Commit your changes and push your branch to your forked repository.
5. Submit a pull request to the main repository.

If you have any questions, issues, or would like to provide feedback, please feel free to contact the project maintainer:

**Joaquín Béjar García**
- Email: jb@taunais.com
- GitHub: [joaquinbejar](https://github.com/joaquinbejar)

We appreciate your interest and look forward to your contributions!

## ✍️ License

Licensed under MIT license
