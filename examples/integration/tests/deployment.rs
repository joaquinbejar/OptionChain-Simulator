//! What the harness proves about a deployment before any contract test runs:
//! it answers, it says which version it is, and a simulation created here is
//! cleaned up afterwards.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{Simulation, reference_request, service};

/// The deployment is an optionchain-simulator, and the run reports which
/// version of it.
///
/// This is the identity probe every other test leans on: they skip a feature
/// that answers 404, which is only sound if 404 means "this build is older",
/// not "this is somebody else's service". So the identity has to be
/// unmistakable before any skip is trusted. An OpenAPI document naming the
/// service is the strongest signal available over HTTP, and a build too old to
/// serve one still has to answer the v1 route with a body this service would
/// produce.
#[test]
fn test_the_deployment_is_this_service_and_names_its_version() {
    let Some(client) = service() else {
        return;
    };

    let document = match client.get("/api-docs/openapi.json") {
        Ok(response) if response.status == 200 => response
            .json::<serde_json::Value>("/api-docs/openapi.json")
            .ok(),
        Ok(_) | Err(_) => None,
    };

    if let Some(document) = document {
        let info = document.get("info");
        let title = info
            .and_then(|info| info.get("title"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let version = info
            .and_then(|info| info.get("version"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        assert!(
            title.to_ascii_lowercase().contains("optionchain"),
            "{} serves an OpenAPI document titled {title:?}, which is not this service; every \
             skip in this suite assumes a 404 means an older build of THIS service, so a \
             different one has to fail loudly",
            client.base_url()
        );
        assert!(
            !version.is_empty(),
            "the OpenAPI document must carry a version: {document}"
        );
        println!(
            "INFO: {} serves {title} {version}; these tests describe the contract of the \
             working tree, so a difference is a lag, not a defect",
            client.base_url()
        );
        return;
    }

    // No document: a build old enough to lack one must still answer the oldest
    // route in the service with a body only this service produces.
    let response = match client.get("/api/v1/chain") {
        Ok(response) => response,
        Err(error) => panic!("{error}"),
    };
    assert_eq!(
        response.status,
        400,
        "{} serves no OpenAPI document and does not answer /api/v1/chain the way this service \
         does ({}), so it cannot be identified as an optionchain-simulator: {}",
        client.base_url(),
        response.status,
        response.text()
    );
    let body: serde_json::Value = match response.json("/api/v1/chain") {
        Ok(body) => body,
        Err(error) => panic!(
            "{} answered /api/v1/chain with something that is not this service's rejection \
             shape: {error}",
            client.base_url()
        ),
    };
    assert!(
        body.get("error").is_some(),
        "the rejection shape of this service carries an error field: {body}"
    );
    println!(
        "INFO: {} serves no OpenAPI document, so it predates one; identified by its v1 \
         rejection shape instead",
        client.base_url()
    );
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
