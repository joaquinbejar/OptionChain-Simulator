# OptionChain-Simulator

<div style="text-align: center;">
<img src="https://raw.githubusercontent.com/joaquinbejar/OptionChain-Simulator/main/doc/images/logo.png" alt="OptionChain-Simulator" style="width: 100%; height: 200px;">
</div>

[![License](https://img.shields.io/badge/license-MIT-blue)](./LICENSE)
[![Build](https://img.shields.io/github/actions/workflow/status/joaquinbejar/OptionChain-Simulator/ci.yml)](https://github.com/joaquinbejar/OptionChain-Simulator/actions)
[![Crates.io](https://img.shields.io/crates/v/optionchain-simulator.svg)](https://crates.io/crates/optionchain-simulator)
[![Downloads](https://img.shields.io/crates/d/optionchain-simulator.svg)](https://crates.io/crates/optionchain-simulator)
[![Stars](https://img.shields.io/github/stars/joaquinbejar/OptionChain-Simulator.svg)](https://github.com/joaquinbejar/OptionChain-Simulator/stargazers)
[![Issues](https://img.shields.io/github/issues/joaquinbejar/OptionChain-Simulator.svg)](https://github.com/joaquinbejar/OptionChain-Simulator/issues)

## OptionChain-Simulator: RESTful Option Chain Time Simulator

### Table of Contents
1. [Introduction](#introduction)
2. [Features](#features)
3. [Project Structure](#project-structure)
4. [Setup Instructions](#setup-instructions)
5. [API Usage](#api-usage)
6. [Development](#development)
7. [Contribution and Contact](#contribution-and-contact)

## Introduction

**OptionChain-Simulator** is a lightweight REST API service that simulates an evolving option chain with every request. It is designed for developers building or testing trading systems, backtesters, and visual tools that depend on option data streams but want to avoid relying on live data feeds.

## Features

- 📡 REST API to fetch a simulated option chain.
- ⏱ Each API request advances the simulation one time step.
- 🧮 Option pricing using Black-Scholes or configurable models.
- 🔄 Internal state memory with market evolution.
- ⚙️ Easily configurable initial parameters (IV, strikes, steps).
- 📦 JSON output for easy integration with other tools.
- 📁 Static data support (CSV/JSON-based initial chains).

## Project Structure

```
option-chain-simulator/
├── Cargo.toml
├── src/
│   ├── main.rs          # API server launcher
│   ├── api.rs           # API routes and handlers
│   ├── simulator.rs     # Core simulation engine
│   ├── models.rs        # Structs for options, chains, etc.
│   └── pricing.rs       # Pricing logic (e.g., Black-Scholes)
├── data/
│   └── initial_chain.json
├── tests/
│   └── integration.rs
├── Makefile
├── README.md
└── LICENSE
```

## Setup Instructions

1. Clone the repository:
```bash
git clone https://github.com/joaquinbejar/OptionChain-Simulator.git
cd OptionChain-Simulator
```

2. Build the project:
```bash
cargo build --release
```

3. Run the API server:
```bash
cargo run
```

4. Access the API:
```http
GET http://localhost:8080/chain
```

## API Usage

### `GET /chain`

Returns the current option chain and advances the simulation.

#### Response Example:
```json
{
  "underlying_price": 102.5,
  "options": [
    {
      "strike": 100,
      "type": "Call",
      "expiration_days": 30,
      "implied_volatility": 0.2,
      "price": 4.32
    }
  ]
}
```

## Development

Run the server with:
```bash
cargo run
```

Run tests:
```bash
cargo test
```

Run formatting and linting:
```bash
cargo fmt
cargo clippy
```

## Contribution and Contact

Contributions are welcome! Please submit pull requests, issues, or suggestions.

Maintainer: **Joaquín Béjar García**  
📧 jb@taunais.com  
🔗 [GitHub Profile](https://github.com/joaquinbejar)

---

License: MIT
