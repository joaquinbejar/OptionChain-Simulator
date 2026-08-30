//! What the harness proves about a deployment before any contract test runs:
//! it answers, it says which version it is, and a simulation created here is
//! cleaned up afterwards.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{Simulation, reference_request, service};

/// The deployment answers, and the run reports which contract it exercised.
///
/// A deployed build is not `main`: it can be older, so this reads the version
/// and prints it rather than asserting one. What it does assert is that
/// something is listening and speaking HTTP where the operator said it would
/// be.
#[test]
fn test_the_deployment_answers_and_names_its_version() {
    let Some(client) = service() else {
        return;
    };

    // `/api/v1/chain` is the oldest route in the service and exists in every
    // build, so it is the safest probe for "is anything there".
    let response = match client.get("/api/v1/chain") {
        Ok(response) => response,
        Err(error) => panic!("{error}"),
    };

    assert!(
        response.status < 500,
        "{} answered {} for the oldest route in the service, which means it is not serving \
         normally: {}",
        client.base_url(),
        response.status,
        response.text()
    );

    // The OpenAPI document carries the version the deployment actually runs.
    // A build too old to serve it is a fact to report, not a failure.
    match client.get("/api-docs/openapi.json") {
        Ok(document) if document.status == 200 => {
            let version = document
                .json::<serde_json::Value>("/api-docs/openapi.json")
                .ok()
                .and_then(|body| {
                    body.get("info")
                        .and_then(|info| info.get("version"))
                        .and_then(|version| version.as_str())
                        .map(str::to_string)
                });
            match version {
                Some(version) => println!(
                    "INFO: {} serves optionchain-simulator {version}; these tests describe the \
                     contract of the working tree, so a difference is a lag, not a defect",
                    client.base_url()
                ),
                None => println!("INFO: the OpenAPI document carries no version"),
            }
        }
        Ok(document) => println!(
            "SKIP: {} does not serve an OpenAPI document ({}), so the version could not be read",
            client.base_url(),
            document.status
        ),
        Err(error) => println!("SKIP: the OpenAPI document could not be read: {error}"),
    }
}

/// A simulation created by a test is gone when the test is.
///
/// This is the guarantee every later test in the series relies on, since they
/// all run against a shared deployment: the fixture deletes on drop, including
/// when the test that created it panicked.
#[test]
fn test_a_created_simulation_is_deleted_when_it_goes_out_of_scope() {
    let Some(client) = service() else {
        return;
    };

    let request = reference_request("SPX");
    let path = {
        let simulation = match Simulation::create(&client, &request) {
            Ok(simulation) => simulation,
            Err(error) => {
                println!(
                    "SKIP: {} would not create a v2 simulation, which a build without the v2 \
                     API cannot: {error}",
                    client.base_url()
                );
                return;
            }
        };

        let path = simulation.path("");
        match client.get(&path) {
            Ok(response) => assert_eq!(
                response.status,
                200,
                "a simulation just created must be readable, got {}",
                response.text()
            ),
            Err(error) => panic!("{error}"),
        }
        path
    };

    match client.get(&path) {
        Ok(response) => assert_eq!(
            response.status, 404,
            "the fixture must delete the simulation it created"
        ),
        Err(error) => panic!("{error}"),
    }
}
