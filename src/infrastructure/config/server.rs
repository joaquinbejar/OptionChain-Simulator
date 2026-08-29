//! Where the HTTP server listens.
//!
//! Both the address and the port used to be compile-time constants, so the only
//! ways to move the service off `0.0.0.0:7070` were patching `main.rs` or
//! publishing a different host port through Docker.
//!
//! That blocks a concrete use case: **several instances on one host**. Tape
//! materialisation is embarrassingly parallel — each simulation is independent
//! and shares no state — so a batch consumer scales by sharding work across N
//! instances. With a hardcoded port that needs N containers where N processes
//! would do.
//!
//! # The default moved to loopback
//!
//! `ListenOn::All` was the old constant. The default is now `127.0.0.1`,
//! because this service has no authentication and no rate limiting, so
//! reachability off the host should be a decision rather than a default. **A
//! deployment that relied on the implicit `0.0.0.0` must now set
//! `OCS_BIND_ADDRESS=0.0.0.0`**; `Docker/docker-compose.yml` sets it and the
//! dev override inherits it by merge.
//!
//! [`ListenOn`](crate::infrastructure::ListenOn) lives here rather than beside
//! the request DTOs because it is
//! not a DTO: it never crosses the wire, it is read from the environment, and
//! putting it in `api` would leave this module importing a layer above it.

use crate::utils::ChainError;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};

/// The interface the server binds to.
///
/// `All` and `Localhost` are named because they are the two an operator
/// actually reaches for, and the difference between them matters: this service
/// has no authentication and no rate limiting, so binding beyond loopback is a
/// decision rather than a default. `Address` covers a specific interface, which
/// is what a host running several instances behind a proxy needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ListenOn {
    /// Every interface, `0.0.0.0`.
    All,
    /// Loopback only, `127.0.0.1`. The default, deliberately: an unauthenticated
    /// service should not be reachable off the host unless someone said so.
    #[default]
    Localhost,
    /// One specific interface.
    Address(IpAddr),
}

impl ListenOn {
    /// The address to bind.
    #[must_use]
    pub fn ip(&self) -> IpAddr {
        match self {
            ListenOn::All => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ListenOn::Localhost => IpAddr::V4(Ipv4Addr::LOCALHOST),
            ListenOn::Address(address) => *address,
        }
    }

    /// Whether this binding is reachable from off the host.
    ///
    /// Used to say so once at startup: an unauthenticated service listening
    /// beyond loopback is worth a line in the log, not a silent default.
    #[must_use]
    pub fn is_public(&self) -> bool {
        !self.ip().is_loopback()
    }

    /// Parses a bind address.
    ///
    /// Accepts any IP literal, plus `localhost` and `all` as names for the two
    /// common cases, so an operator can write either form.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming `field` when the value is not
    /// a valid IP address, `localhost` or `all`. This is a SYNTAX check: an
    /// address the host does not own parses here and fails later, at bind
    /// time, with whatever the operating system says.
    pub fn parse(raw: &str, field: &str) -> Result<Self, ChainError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "localhost" => return Ok(ListenOn::Localhost),
            "all" => return Ok(ListenOn::All),
            _ => {}
        }

        let address: IpAddr = raw.trim().parse().map_err(|_| ChainError::Validation {
            field: field.to_string(),
            reason: format!(
                "must be an IP address, or `localhost` or `all`, got {:?}",
                raw.trim()
            ),
        })?;

        // Normalised so the two named forms and their literals compare equal,
        // and so a log line reads the same either way.
        Ok(match address {
            address if address == IpAddr::V4(Ipv4Addr::UNSPECIFIED) => ListenOn::All,
            address if address == IpAddr::V4(Ipv4Addr::LOCALHOST) => ListenOn::Localhost,
            address => ListenOn::Address(address),
        })
    }
}

impl From<ListenOn> for String {
    fn from(listen_on: ListenOn) -> Self {
        listen_on.ip().to_string()
    }
}

impl fmt::Display for ListenOn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.ip())
    }
}

/// The environment variable naming the interface to bind.
pub const BIND_ADDRESS_VAR: &str = "OCS_BIND_ADDRESS";

/// The environment variable naming the port to bind.
pub const PORT_VAR: &str = "OCS_PORT";

/// The interface bound when `OCS_BIND_ADDRESS` is unset.
pub const DEFAULT_BIND_ADDRESS: ListenOn = ListenOn::Localhost;

/// The port bound when `OCS_PORT` is unset.
pub const DEFAULT_PORT: u16 = 7070;

/// Where the server listens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerConfig {
    /// The interface to bind.
    pub address: ListenOn,
    /// The port to bind.
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            address: DEFAULT_BIND_ADDRESS,
            port: DEFAULT_PORT,
        }
    }
}

impl ServerConfig {
    /// Reads the configuration from the environment.
    ///
    /// Unlike the request caps, which warn and fall back, an unusable value
    /// here FAILS STARTUP. A cap that falls back still serves requests; a
    /// service that quietly ignored `OCS_PORT` would bind a port the operator
    /// did not ask for, and a batch consumer sharding across instances would
    /// find two of them fighting over one port. That matches how the v2 and
    /// snapshot knobs already behave.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming the offending variable when
    /// the address is not a valid IP address or name, or the port is not an
    /// integer in `1..=65535`. Port `0` is refused: the operating system would
    /// pick a free port, which is the opposite of what someone sharding across
    /// known ports wants.
    pub fn from_env() -> Result<Self, ChainError> {
        let address = match super::read_var(BIND_ADDRESS_VAR) {
            Some(raw) => ListenOn::parse(&raw, BIND_ADDRESS_VAR)?,
            None => DEFAULT_BIND_ADDRESS,
        };

        let port = match super::read_var(PORT_VAR) {
            Some(raw) => raw
                .parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| ChainError::Validation {
                    field: PORT_VAR.to_string(),
                    reason: format!("must be an integer between 1 and 65535, got {raw:?}"),
                })?,
            None => DEFAULT_PORT,
        };

        Ok(Self { address, port })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::net::{Ipv6Addr, SocketAddr};
    use std::sync::Mutex;

    static ENV_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn set_var(name: &str, value: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(name, value);
        }
    }

    fn remove_var(name: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(name);
        }
    }

    fn clear() {
        remove_var(BIND_ADDRESS_VAR);
        remove_var(PORT_VAR);
    }

    /// With neither variable set, the service binds loopback on 7070.
    ///
    /// The second half of that is a DEFAULT CHANGE: it used to be `0.0.0.0`.
    #[test]
    fn test_neither_variable_binds_loopback_on_the_default_port() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear();

        let config = match ServerConfig::from_env() {
            Ok(config) => config,
            Err(error) => panic!("an empty environment must resolve: {error}"),
        };

        assert_eq!(config.address, ListenOn::Localhost);
        assert_eq!(config.address.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(config.port, 7070);
        assert!(
            !config.address.is_public(),
            "an unauthenticated service must not default to reachable"
        );
    }

    /// Both variables are honoured, in either spelling.
    #[test]
    fn test_both_variables_are_honoured() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        for (raw, expected) in [
            ("0.0.0.0", ListenOn::All),
            ("all", ListenOn::All),
            ("127.0.0.1", ListenOn::Localhost),
            ("localhost", ListenOn::Localhost),
            (
                "10.1.2.3",
                ListenOn::Address(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))),
            ),
        ] {
            set_var(BIND_ADDRESS_VAR, raw);
            set_var(PORT_VAR, "9001");

            match ServerConfig::from_env() {
                Ok(config) => {
                    assert_eq!(config.address, expected, "for {raw:?}");
                    assert_eq!(config.port, 9001, "for {raw:?}");
                }
                Err(error) => panic!("{raw:?} must resolve: {error}"),
            }
        }
        clear();
    }

    /// A blank value is an unset value, like every other knob.
    #[test]
    fn test_blank_variables_fall_back_to_the_defaults() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        set_var(BIND_ADDRESS_VAR, "  ");
        set_var(PORT_VAR, "");

        match ServerConfig::from_env() {
            Ok(config) => assert_eq!(config, ServerConfig::default()),
            Err(error) => panic!("a blank value is unset, not invalid: {error}"),
        }
        clear();
    }

    /// An unusable address fails startup, naming the variable.
    ///
    /// Naming it is the point: the operator set something, and an error that
    /// says only "invalid configuration" sends them looking through all of it.
    #[test]
    fn test_an_unusable_address_names_the_variable() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        remove_var(PORT_VAR);

        for raw in ["not-an-address", "999.1.1.1", "127.0.0.1:7070"] {
            set_var(BIND_ADDRESS_VAR, raw);

            match ServerConfig::from_env() {
                Ok(config) => panic!("{raw:?} must not resolve, got {config:?}"),
                Err(ChainError::Validation { field, reason }) => {
                    assert_eq!(field, BIND_ADDRESS_VAR, "for {raw:?}");
                    assert!(
                        reason.contains(raw.trim()),
                        "the reason must quote it: {reason}"
                    );
                }
                Err(error) => panic!("expected a validation failure, got {error:?}"),
            }
        }
        clear();
    }

    /// An unusable port fails startup, naming the variable.
    ///
    /// `0` is refused along with the rest: the operating system would pick a
    /// free port, which is the opposite of what someone sharding work across
    /// known ports wants.
    #[test]
    fn test_an_unusable_port_names_the_variable() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        remove_var(BIND_ADDRESS_VAR);

        for raw in ["0", "-1", "70000", "http", "7070.5"] {
            set_var(PORT_VAR, raw);

            match ServerConfig::from_env() {
                Ok(config) => panic!("{raw:?} must not resolve, got {config:?}"),
                Err(ChainError::Validation { field, .. }) => {
                    assert_eq!(field, PORT_VAR, "for {raw:?}");
                }
                Err(error) => panic!("expected a validation failure, got {error:?}"),
            }
        }
        clear();
    }

    /// Every variant resolves to the address it says, and renders as it binds.
    ///
    /// `Display` and `From<ListenOn> for String` are what the bind string and
    /// the startup log are built from, so what they render is what the service
    /// actually listens on.
    #[test]
    fn test_every_variant_renders_as_the_address_it_binds() {
        for (listen_on, expected) in [
            (ListenOn::All, "0.0.0.0"),
            (ListenOn::Localhost, "127.0.0.1"),
            (
                ListenOn::Address(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))),
                "10.1.2.3",
            ),
            (
                ListenOn::Address(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1))),
                "::1",
            ),
        ] {
            assert_eq!(listen_on.ip().to_string(), expected);
            assert_eq!(listen_on.to_string(), expected, "Display for {listen_on:?}");
            assert_eq!(
                String::from(listen_on),
                expected,
                "String::from for {listen_on:?}"
            );
        }
    }

    /// The bind string a resolved configuration produces.
    ///
    /// An IPv6 address has to come out bracketed: `::1:9001` is ambiguous, and
    /// this is the string handed to the socket.
    #[test]
    fn test_the_resolved_configuration_produces_a_usable_bind_address() {
        for (address, port, expected) in [
            (ListenOn::All, 7070_u16, "0.0.0.0:7070"),
            (ListenOn::Localhost, 9001, "127.0.0.1:9001"),
            (
                ListenOn::Address(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1))),
                9001,
                "[::1]:9001",
            ),
        ] {
            let config = ServerConfig { address, port };
            assert_eq!(
                SocketAddr::new(config.address.ip(), config.port).to_string(),
                expected
            );
        }
    }

    /// Only loopback reads as private.
    ///
    /// The startup warning hangs off this, and a warning that fires on the
    /// default would be one an operator learns to ignore.
    #[test]
    fn test_only_loopback_is_not_public() {
        assert!(!ListenOn::Localhost.is_public());
        assert!(!ListenOn::Address(IpAddr::V6(Ipv6Addr::LOCALHOST)).is_public());
        assert!(ListenOn::All.is_public());
        assert!(ListenOn::Address(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))).is_public());
    }

    /// An IPv6 literal is accepted and kept as itself.
    #[test]
    fn test_an_ipv6_address_is_accepted() {
        match ListenOn::parse("::1", BIND_ADDRESS_VAR) {
            Ok(listen_on) => {
                assert_eq!(
                    listen_on,
                    ListenOn::Address(IpAddr::V6(Ipv6Addr::LOCALHOST))
                );
                assert!(!listen_on.is_public());
            }
            Err(error) => panic!("an IPv6 literal must parse: {error}"),
        }
    }

    /// Two instances configured for different ports do not collide.
    ///
    /// The whole point of the issue: sharding tape materialisation across
    /// several instances on one host. Asserted on the resolved configuration,
    /// since actually binding two sockets in a unit test would make it a port
    /// availability test rather than a configuration one.
    #[test]
    fn test_two_instances_resolve_to_different_ports() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        set_var(PORT_VAR, "7071");
        let first = ServerConfig::from_env();
        set_var(PORT_VAR, "7072");
        let second = ServerConfig::from_env();
        clear();

        match (first, second) {
            (Ok(first), Ok(second)) => {
                assert_eq!(first.port, 7071);
                assert_eq!(second.port, 7072);
                assert_ne!(first, second, "two shards must not fight over one port");
            }
            other => panic!("both must resolve, got {other:?}"),
        }
    }
}
