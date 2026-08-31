//! The harness itself: what it does with no deployment configured, and what it
//! says when the one it is given cannot be talked to.
//!
//! These run in the hermetic suite, so they must open no socket unless
//! `OCS_INTEGRATION_BASE_URL` is set.

use examples_integration::{IntegrationError, ServiceClient, service};

/// With nothing configured there is no client, and no connection is attempted.
///
/// The variable is read rather than mocked, so on a machine that HAS one
/// configured this asserts the other half: a client exists and names it.
#[test]
fn test_an_unconfigured_deployment_yields_no_client() {
    match (std::env::var("OCS_INTEGRATION_BASE_URL").ok(), service()) {
        (None, client) => assert!(
            client.is_none(),
            "no deployment is configured, so there must be no client"
        ),
        (Some(raw), client) if raw.trim().is_empty() => assert!(
            client.is_none(),
            "a blank value is an unset one, so there must be no client"
        ),
        (Some(raw), Some(client)) => assert!(
            raw.contains(client.base_url().trim_start_matches("http://")),
            "the client must talk to the configured deployment, not to {}",
            client.base_url()
        ),
        (Some(raw), None) => panic!("{raw} is configured but produced no client"),
    }
}

/// A URL this harness cannot talk to fails as a configuration mistake, with a
/// message that names it, rather than as a mysterious timeout.
#[test]
fn test_an_unusable_base_url_is_refused_with_its_reason() {
    for (url, expected) in [
        ("https://a.host:7070", "plain HTTP only"),
        ("a.host:7070", "must start with http://"),
        ("http://a.host:7070/api", "no path"),
        ("http://", "names no host"),
    ] {
        match ServiceClient::new(url) {
            Ok(client) => panic!(
                "{url} must be refused, got a client for {}",
                client.base_url()
            ),
            Err(error @ IntegrationError::UnusableBaseUrl { .. }) => {
                let rendered = error.to_string();
                assert!(
                    rendered.contains(url),
                    "the failure must name the URL: {rendered}"
                );
                assert!(
                    rendered.contains(expected),
                    "the failure must say why: {rendered}"
                );
            }
            Err(error) => panic!("{url} must be a configuration failure, got {error}"),
        }
    }
}

/// A usable URL keeps the trailing slash off and defaults the port.
#[test]
fn test_a_usable_base_url_is_normalised() {
    for (given, expected) in [
        ("http://a.host:7070", "http://a.host:7070"),
        ("http://a.host:7070/", "http://a.host:7070"),
        ("  http://a.host  ", "http://a.host"),
    ] {
        match ServiceClient::new(given) {
            Ok(client) => assert_eq!(client.base_url(), expected),
            Err(error) => panic!("{given} must be usable: {error}"),
        }
    }
}

/// An unreachable deployment names the URL it could not reach.
///
/// Asserted on the error VALUE rather than by dialling a socket. The promise
/// this crate makes is that `cargo test --workspace` with the variable unset
/// starts no network connection at all, and a loopback connect to a closed
/// port is still a connection: on a machine with an aggressive firewall or a
/// proxy it is also a slow one. What matters here is that a transport failure
/// carries the URL to the person reading the output, and that is a property of
/// the message.
#[test]
fn test_an_unreachable_service_names_the_url_in_its_error() {
    let error = IntegrationError::Unreachable {
        url: "http://a.host:7070/health".to_string(),
        cause: "connection refused".to_string(),
    };

    let rendered = error.to_string();
    assert!(
        rendered.contains("http://a.host:7070/health"),
        "the failure must name the URL: {rendered}"
    );
    assert!(
        rendered.contains("connection refused"),
        "the failure must carry the underlying cause: {rendered}"
    );
}

/// The same for a body that never arrives: the URL travels with the failure.
#[test]
fn test_a_malformed_answer_names_the_url_in_its_error() {
    let error = IntegrationError::MalformedResponse {
        url: "http://a.host:7070/export".to_string(),
        reason: "the connection closed before the terminating zero chunk".to_string(),
    };

    let rendered = error.to_string();
    assert!(rendered.contains("http://a.host:7070/export"), "{rendered}");
    assert!(rendered.contains("terminating zero chunk"), "{rendered}");
}
