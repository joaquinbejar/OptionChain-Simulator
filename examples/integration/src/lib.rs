//! Integration tests against a DEPLOYED optionchain-simulator.
//!
//! Every other test in this workspace runs in process. That covers the code
//! and not the thing an operator actually has: a service listening on a port,
//! behind a container, with Redis and MongoDB and possibly ClickHouse behind
//! it. Nothing in the hermetic suite would catch a route that stopped being
//! mounted, a middleware that changed a status code, or a deployment whose
//! contract differs from the one the tests describe.
//!
//! # Pointing the tests at a service
//!
//! `OCS_INTEGRATION_BASE_URL` carries the full base URL of the service under
//! test, scheme and port included, for example `http://localhost:7070`. Where
//! it points is deployment configuration and belongs in an operator's
//! environment; this repository never names a deployment.
//!
//! When the variable is unset — or blank, which
//! [`optionchain_simulator::utils::env::read_var`] treats as unset — every
//! test SKIPS instead of failing, so `cargo test --workspace` stays hermetic
//! and opens no socket.
//!
//! ```text
//! OCS_INTEGRATION_BASE_URL=http://a.host:7070 make test-integration
//! ```
//!
//! # A deployed build is not `main`
//!
//! These tests exercise whatever is deployed, which can be older than the
//! working tree: a feature that landed this week may answer 404 there. The
//! rule this crate follows is to state which contract it is testing and to
//! SKIP loudly when a feature is absent, never to fail for it. A skip is a
//! report that something was not exercised; only a real disagreement with the
//! contract is a failure.
//!
//! # No HTTP client dependency
//!
//! `rules/global_rules.md` forbids adding a dependency without the owner's
//! approval, and this workspace has no HTTP client. Rather than stall, the
//! harness speaks HTTP/1.1 over [`std::net::TcpStream`] itself: enough of it
//! to send a request with a JSON body and read a response, including chunked
//! transfer encoding. It is deliberately small, and issue #117 proposes
//! replacing it with `reqwest` if that dependency is ever approved.

use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// The variable naming the service under test.
pub const BASE_URL_VARIABLE: &str = "OCS_INTEGRATION_BASE_URL";

/// How long a request may take before the harness gives up on it.
///
/// An export of a long simulation is the slowest thing the service does, so
/// this is generous; a hung connection still fails rather than hanging a CI
/// job forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// What went wrong, always with enough context to act on.
#[derive(Debug)]
pub enum IntegrationError {
    /// The base URL is not one this harness can talk to.
    UnusableBaseUrl {
        /// The URL as configured.
        url: String,
        /// Why it cannot be used.
        reason: String,
    },
    /// The service could not be reached.
    Unreachable {
        /// The URL that was tried.
        url: String,
        /// The underlying failure.
        cause: String,
    },
    /// The service answered something that is not HTTP.
    MalformedResponse {
        /// The URL that was tried.
        url: String,
        /// What was wrong with the answer.
        reason: String,
    },
    /// The body did not decode as the expected JSON shape.
    Decode {
        /// The URL that was tried.
        url: String,
        /// The decode failure.
        cause: String,
    },
}

impl fmt::Display for IntegrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnusableBaseUrl { url, reason } => {
                write!(
                    formatter,
                    "{BASE_URL_VARIABLE}={url} cannot be used: {reason}"
                )
            }
            Self::Unreachable { url, cause } => {
                write!(formatter, "{url} could not be reached: {cause}")
            }
            Self::MalformedResponse { url, reason } => {
                write!(formatter, "{url} answered something unusable: {reason}")
            }
            Self::Decode { url, cause } => {
                write!(
                    formatter,
                    "{url} answered a body that did not decode: {cause}"
                )
            }
        }
    }
}

impl std::error::Error for IntegrationError {}

/// One HTTP response, read whole.
#[derive(Debug)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The response headers, names lowercased.
    pub headers: Vec<(String, String)>,
    /// The body, as bytes, since an export is not text.
    pub body: Vec<u8>,
}

impl Response {
    /// The body as UTF-8, lossily, for a message or an assertion.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body decoded as JSON.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::Decode`] when the body is not the expected shape,
    /// naming the URL so a failure says WHICH endpoint disagreed.
    pub fn json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, IntegrationError> {
        serde_json::from_slice(&self.body).map_err(|error| IntegrationError::Decode {
            url: url.to_string(),
            cause: format!("{error}; body was {}", self.text()),
        })
    }

    /// The value of one header, if present.
    pub fn header(&self, name: &str) -> Option<&str> {
        let wanted = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(header, _)| header == &wanted)
            .map(|(_, value)| value.as_str())
    }
}

/// A client bound to one deployment.
///
/// Owns the base URL so no test writes one by hand, and so every failure can
/// name the exact URL it was talking to.
#[derive(Debug, Clone)]
pub struct ServiceClient {
    base_url: String,
    host_header: String,
    address: String,
}

impl ServiceClient {
    /// Builds a client from `OCS_INTEGRATION_BASE_URL`, or `None` when the
    /// variable is unset or blank.
    ///
    /// A blank value is an unset one, the rule the service itself applies to
    /// every knob it reads.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::UnusableBaseUrl`] when the variable is set to
    /// something this harness cannot talk to, which is a configuration
    /// mistake and must not be mistaken for "no deployment configured".
    pub fn from_environment() -> Result<Option<Self>, IntegrationError> {
        match optionchain_simulator::utils::env::read_var(BASE_URL_VARIABLE) {
            Some(url) => Self::new(&url).map(Some),
            None => Ok(None),
        }
    }

    /// Builds a client for one base URL.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::UnusableBaseUrl`] for anything but an `http://`
    /// URL with a host. HTTPS would need a TLS implementation, which is a
    /// dependency nobody has approved, so it is refused with a message that
    /// says so rather than failing later as a protocol error.
    pub fn new(base_url: &str) -> Result<Self, IntegrationError> {
        let unusable = |reason: &str| IntegrationError::UnusableBaseUrl {
            url: base_url.to_string(),
            reason: reason.to_string(),
        };

        // Trim the scheme FIRST: trimming trailing slashes off the whole URL
        // would eat the scheme's own separator and turn `http://` into
        // `http:`, reporting a missing scheme where the real fault is a
        // missing host.
        let trimmed = base_url.trim();
        let authority = match trimmed.strip_prefix("http://") {
            Some(authority) => authority,
            None if trimmed.starts_with("https://") => {
                return Err(unusable(
                    "this harness speaks plain HTTP only, because a TLS client would be a \
                     dependency nobody has approved; put the service behind a plain-HTTP \
                     address, or approve the dependency in issue #117",
                ));
            }
            None => return Err(unusable("the URL must start with http://")),
        };

        let authority = authority.trim_end_matches('/');

        // A path component would silently prefix every request, so refuse it
        // rather than produce puzzling 404s.
        if authority.contains('/') {
            return Err(unusable(
                "the URL must be a scheme, host and port, with no path",
            ));
        }
        if authority.is_empty() {
            return Err(unusable("the URL names no host"));
        }

        let address = if authority.contains(':') {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };

        Ok(Self {
            base_url: format!("http://{authority}"),
            host_header: authority.to_string(),
            address,
        })
    }

    /// The base URL this client talks to, for a test that wants to report it.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sends a request and reads the whole response.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::Unreachable`] when the connection fails or times
    /// out, naming the URL rather than surfacing a bare timeout, and
    /// [`IntegrationError::MalformedResponse`] when the answer is not HTTP the
    /// harness can read.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<Response, IntegrationError> {
        let url = format!("{}{path}", self.base_url);
        let unreachable = |cause: String| IntegrationError::Unreachable {
            url: url.clone(),
            cause,
        };

        // `TcpStream::connect` has no deadline of its own, so a black-holed
        // address would hang for the operating system's TCP timeout, which on
        // a scheduled job means minutes rather than the request deadline. The
        // address is resolved first and dialled with an explicit one, so every
        // phase of a request is bounded by the same number.
        let address = self
            .address
            .to_socket_addrs()
            .map_err(|error| unreachable(format!("the address does not resolve: {error}")))?
            .next()
            .ok_or_else(|| unreachable("the address resolves to nothing".to_string()))?;
        let stream = TcpStream::connect_timeout(&address, REQUEST_TIMEOUT)
            .map_err(|error| unreachable(error.to_string()))?;
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|error| unreachable(error.to_string()))?;
        stream
            .set_write_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|error| unreachable(error.to_string()))?;

        let mut stream = stream;
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAccept: */*\r\nConnection: close\r\n",
            self.host_header
        );
        if let Some(body) = body {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        if let Some(body) = body {
            request.push_str(body);
        }

        stream
            .write_all(request.as_bytes())
            .map_err(|error| unreachable(error.to_string()))?;
        stream
            .flush()
            .map_err(|error| unreachable(error.to_string()))?;

        read_response(&mut stream, &url)
    }

    /// `GET`, the shape most tests want.
    ///
    /// # Errors
    ///
    /// As [`ServiceClient::request`].
    pub fn get(&self, path: &str) -> Result<Response, IntegrationError> {
        self.request("GET", path, None)
    }

    /// `POST` with a JSON body.
    ///
    /// # Errors
    ///
    /// As [`ServiceClient::request`].
    pub fn post(&self, path: &str, body: &serde_json::Value) -> Result<Response, IntegrationError> {
        self.request("POST", path, Some(&body.to_string()))
    }

    /// `DELETE`.
    ///
    /// # Errors
    ///
    /// As [`ServiceClient::request`].
    pub fn delete(&self, path: &str) -> Result<Response, IntegrationError> {
        self.request("DELETE", path, None)
    }
}

/// Reads one HTTP/1.1 response: status line, headers, and a body that is
/// either length-delimited, chunked, or read to end of stream.
fn read_response(stream: &mut TcpStream, url: &str) -> Result<Response, IntegrationError> {
    let malformed = |reason: String| IntegrationError::MalformedResponse {
        url: url.to_string(),
        reason,
    };
    let unreachable = |cause: String| IntegrationError::Unreachable {
        url: url.to_string(),
        cause,
    };

    let mut reader = BufReader::new(stream);

    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|error| unreachable(error.to_string()))?;
    if status_line.is_empty() {
        return Err(malformed(
            "the connection closed before any answer".to_string(),
        ));
    }
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| malformed(format!("no status code in {status_line:?}")))?;

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| unreachable(error.to_string()))?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }

    let header = |name: &str| {
        headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    };

    let body =
        if header("transfer-encoding").is_some_and(|value| value.eq_ignore_ascii_case("chunked")) {
            read_chunked(&mut reader, url)?
        } else if let Some(length) = header("content-length").and_then(|value| value.parse().ok()) {
            let mut body = vec![0_u8; length];
            reader
                .read_exact(&mut body)
                .map_err(|error| unreachable(error.to_string()))?;
            body
        } else {
            let mut body = Vec::new();
            reader
                .read_to_end(&mut body)
                .map_err(|error| unreachable(error.to_string()))?;
            body
        };

    Ok(Response {
        status,
        headers,
        body,
    })
}

/// Reads a chunked body, which is how the service streams an export.
fn read_chunked(
    reader: &mut BufReader<&mut TcpStream>,
    url: &str,
) -> Result<Vec<u8>, IntegrationError> {
    let malformed = |reason: String| IntegrationError::MalformedResponse {
        url: url.to_string(),
        reason,
    };
    let unreachable = |cause: String| IntegrationError::Unreachable {
        url: url.to_string(),
        cause,
    };

    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        let read = reader
            .read_line(&mut size_line)
            .map_err(|error| unreachable(error.to_string()))?;

        // A chunked body is complete only after a zero-size chunk. End of
        // stream here means the connection died mid-download, and accepting it
        // would hand a test a truncated export that looks like valid partial
        // data — exactly the failure this suite exists to catch.
        if read == 0 {
            return Err(malformed(
                "the connection closed before the terminating zero chunk, so the body is \
                 truncated"
                    .to_string(),
            ));
        }
        let size_text = size_line.trim();
        if size_text.is_empty() {
            return Err(malformed(
                "a blank line where a chunk size was expected, so the body is truncated"
                    .to_string(),
            ));
        }

        // A chunk size may carry extensions after a semicolon.
        let size_text = size_text.split(';').next().unwrap_or(size_text);
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|error| malformed(format!("chunk size {size_text:?} is not hex: {error}")))?;
        if size == 0 {
            return Ok(body);
        }

        let mut chunk = vec![0_u8; size];
        reader
            .read_exact(&mut chunk)
            .map_err(|error| unreachable(error.to_string()))?;
        body.extend_from_slice(&chunk);

        // Each chunk is followed by CRLF; anything else means the framing is
        // not what it claims to be.
        let mut terminator = String::new();
        let read = reader
            .read_line(&mut terminator)
            .map_err(|error| unreachable(error.to_string()))?;
        if read == 0 || terminator.trim_end_matches(['\r', '\n']) != "" {
            return Err(malformed(format!(
                "a chunk of {size} bytes was followed by {terminator:?} rather than a line end"
            )));
        }
    }
}

/// The client for the configured deployment, or `None` when there is none.
///
/// Prints the reason once per test so a skipped run says WHY it exercised
/// nothing, which is the difference between "no deployment configured" and a
/// suite that silently tests nothing.
///
/// # Panics
///
/// When the variable is SET to something unusable. That is a configuration
/// mistake, not an absent deployment, and swallowing it would report a green
/// run that tested nothing.
pub fn service() -> Option<ServiceClient> {
    match ServiceClient::from_environment() {
        Ok(Some(client)) => Some(client),
        Ok(None) => {
            println!(
                "SKIP: {BASE_URL_VARIABLE} is unset, so nothing was exercised against a \
                 deployment. Set it to a base URL such as http://host:7070 to run these."
            );
            None
        }
        Err(error) => panic!("{error}"),
    }
}

/// A v2 simulation that deletes itself, so a failing test leaves no state on a
/// shared deployment.
#[derive(Debug)]
pub struct Simulation {
    client: ServiceClient,
    id: String,
}

impl Simulation {
    /// Creates a simulation from a request body.
    ///
    /// # Errors
    ///
    /// The transport failures of [`ServiceClient::request`], plus
    /// [`IntegrationError::Decode`] when the answer carries no id, which is
    /// what a deployment older than the v2 API looks like.
    pub fn create(
        client: &ServiceClient,
        request: &serde_json::Value,
    ) -> Result<Self, IntegrationError> {
        let path = "/api/v2/simulations";
        let response = client.post(path, request)?;
        let url = format!("{}{path}", client.base_url());

        if response.status != 201 && response.status != 200 {
            return Err(IntegrationError::Decode {
                url,
                cause: format!(
                    "creating a simulation answered {} rather than 201: {}",
                    response.status,
                    response.text()
                ),
            });
        }

        let body: serde_json::Value = response.json(&url)?;
        let id = body
            .get("id")
            .and_then(|id| id.as_str())
            .ok_or_else(|| IntegrationError::Decode {
                url,
                cause: format!("the created simulation carries no id: {body}"),
            })?
            .to_string();

        Ok(Self {
            client: client.clone(),
            id,
        })
    }

    /// The simulation's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The path of this simulation, with an optional suffix such as `/step`.
    pub fn path(&self, suffix: &str) -> String {
        format!("/api/v2/simulations/{}{suffix}", self.id)
    }
}

impl Drop for Simulation {
    fn drop(&mut self) {
        // A failing test must not leave a simulation behind on a shared
        // deployment, and a cleanup that itself fails must not mask the
        // failure that is already being reported — so this reports loudly and
        // never panics.
        report_cleanup(&self.client, &self.path(""), &self.id);
    }
}

/// Deletes one resource and says plainly whether it worked.
///
/// A transport error is not the only way cleanup fails: a 401, a 405 or a 500
/// all return `Ok(Response)`, and treating those as success is how a shared
/// deployment fills up with abandoned simulations. Only the documented success
/// codes count, plus 404, which means someone or something already removed it.
pub fn report_cleanup(client: &ServiceClient, path: &str, what: &str) {
    match client.delete(path) {
        Ok(response) if matches!(response.status, 200 | 202 | 204 | 404) => {}
        Ok(response) => println!(
            "WARNING: deleting {what} answered {}, so it may still exist on the deployment: {}",
            response.status,
            response.text()
        ),
        Err(error) => println!("WARNING: could not delete {what}: {error}"),
    }
}

/// A minimal v2 create request, as a starting point for a test that varies one
/// thing about it.
pub fn reference_request(symbol: &str) -> serde_json::Value {
    serde_json::json!({
        "symbol": symbol,
        "steps": 4,
        "timezone": "America/New_York",
        "expiration_time": "17:00",
        "schedules": [{"rule_id": "zero_dte", "kind": "daily", "target_count": 1}],
        "initial_price": 5000.0,
        "volatility": 0.2,
        "risk_free_rate": 0.05,
        "dividend_yield": 0.0,
        "method": {"GeometricBrownian": {"dt": 0.00396825396, "drift": 0.05, "volatility": 0.2}},
        "time_frame": "Day",
        "chain_size": 5,
        "strike_interval": 25.0,
        "seed": 42
    })
}
