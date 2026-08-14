use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::api::rest::handlers::create_session,
        crate::api::rest::handlers::get_current_step,
        crate::api::rest::handlers::advance_step,
        crate::api::rest::handlers::replace_session,
        crate::api::rest::handlers::update_session,
        crate::api::rest::handlers::delete_session,
        crate::api::rest::handlers_v2::create_simulation,
        crate::api::rest::handlers_v2::get_simulation,
        crate::api::rest::handlers_v2::peek_snapshot,
        crate::api::rest::handlers_v2::advance_simulation,
        crate::api::rest::handlers_v2::delete_simulation,
    ),
    components(
        schemas(
            crate::api::rest::responses::OptionContractResponse,
            crate::api::rest::responses::OptionPriceResponse,
            crate::api::rest::responses::SessionInfoResponse,
            crate::api::rest::requests::CreateSessionRequest,
            crate::api::rest::requests::UpdateSessionRequest,
            crate::api::rest::models::SessionId,
            crate::api::rest::responses::ValidationErrorResponse,
            crate::api::rest::requests_v2::CreateSimulationRequest,
            crate::api::rest::responses_v2::SimulationResponse,
            crate::api::rest::responses_v2::SimulationParametersResponse,
            crate::api::rest::responses_v2::ScheduleRuleResponse,
            crate::api::rest::responses_v2::SnapshotResponse,
            crate::api::rest::responses_v2::ExpiryChainResponse,
            crate::api::rest::responses_v2::ContractResponse,
            crate::api::rest::responses_v2::OptionQuoteResponse,
            crate::api::rest::responses_v2::UnderlyingResponse,
            crate::api::rest::responses_v2::CursorResponse,
        )
    ),
    tags(
        (name = "Options-Simulator", description = "Options Simulator endpoints")
    )
)]
pub(crate) struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use utoipa::OpenApi;

    /// Test that the OpenAPI specification can be generated without errors
    #[test]
    fn test_openapi_spec_generation() {
        let openapi = ApiDoc::openapi();

        // Verify basic structure of OpenAPI spec
        assert!(
            !openapi.to_json().expect("REASON").is_empty(),
            "OpenAPI spec should not be empty"
        );
    }

    /// Test paths are correctly defined in the OpenAPI specification
    #[test]
    fn test_openapi_paths() {
        let openapi = ApiDoc::openapi();
        let json = openapi.to_json().unwrap();

        // Parse the JSON
        let parsed: Value = serde_json::from_str(&json).expect("Failed to parse OpenAPI JSON");

        // Check paths section exists
        assert!(parsed.get("paths").is_some(), "Paths section should exist");

        // Verify specific paths are present
        let paths = parsed.get("paths").unwrap();

        // Expected paths based on the OpenAPI derive macro
        let expected_paths = vec![
            "/api/v1/chain",      // create / peek (GET) / replace / update / delete
            "/api/v1/chain/step", // advance one step (POST)
        ];

        for path in expected_paths {
            assert!(
                paths.get(path).is_some(),
                "Path {} should be defined in OpenAPI spec",
                path
            );
        }
    }

    /// Test components/schemas are correctly defined
    #[test]
    fn test_openapi_schemas() {
        let openapi = ApiDoc::openapi();
        let json = openapi.to_json().unwrap();

        // Parse the JSON
        let parsed: Value = serde_json::from_str(&json).expect("Failed to parse OpenAPI JSON");

        // Check components and schemas sections exist
        let components = parsed
            .get("components")
            .expect("Components section should exist");
        let schemas = components
            .get("schemas")
            .expect("Schemas section should exist");

        // Expected schemas based on the macro
        let expected_schemas = vec![
            "OptionContractResponse",
            "OptionPriceResponse",
            "SessionInfoResponse",
            "CreateSessionRequest",
            "UpdateSessionRequest",
            "SessionId",
        ];

        for schema_name in expected_schemas {
            assert!(
                schemas.get(schema_name).is_some(),
                "Schema {} should be defined in OpenAPI spec",
                schema_name
            );
        }
    }

    /// Test tags are correctly defined
    #[test]
    fn test_openapi_tags() {
        let openapi = ApiDoc::openapi();
        let json = openapi.to_json().unwrap();

        // Parse the JSON
        let parsed: Value = serde_json::from_str(&json).expect("Failed to parse OpenAPI JSON");

        // Check tags section exists
        let tags = parsed.get("tags").expect("Tags section should exist");

        // Verify the Options-Simulator tag
        assert!(tags.is_array(), "Tags should be an array");

        let tag_exists = tags
            .as_array()
            .unwrap()
            .iter()
            .any(|tag| tag.get("name").and_then(|n| n.as_str()) == Some("Options-Simulator"));

        assert!(tag_exists, "Options-Simulator tag should be defined");
    }

    /// Validate that the JSON schema can be deserialized
    #[test]
    fn test_openapi_json_deserializability() {
        let openapi = ApiDoc::openapi();
        let json = openapi.to_json().unwrap();

        // Attempt to deserialize the JSON
        let result: Result<serde_json::Value, _> = serde_json::from_str(&json);

        assert!(result.is_ok(), "OpenAPI JSON should be valid JSON");
    }

    /// Verify that no sensitive information is leaked in the OpenAPI spec
    #[test]
    fn test_no_sensitive_info_in_spec() {
        let openapi = ApiDoc::openapi();
        let json = openapi.to_json().unwrap();

        // Check no environment-specific or sensitive information is present
        assert!(
            !json.contains("localhost")
                && !json.contains("127.0.0.1")
                && !json.contains("password")
                && !json.contains("secret"),
            "OpenAPI spec should not contain sensitive information"
        );
    }

    /// Ensure the spec version is set
    #[test]
    fn test_openapi_version() {
        let openapi = ApiDoc::openapi();
        let json = openapi.to_json().unwrap();

        // Parse the JSON
        let parsed: Value = serde_json::from_str(&json).expect("Failed to parse OpenAPI JSON");

        // Check OpenAPI version is defined
        assert!(
            parsed.get("openapi").is_some(),
            "OpenAPI version should be specified"
        );

        // Optional: Check it matches expected format
        if let Some(version) = parsed.get("openapi") {
            assert!(
                version.as_str().is_some_and(|v| v.starts_with("3.")),
                "OpenAPI version should be 3.x"
            );
        }
    }

    /// The v1 OpenAPI paths and their operations are unchanged.
    ///
    /// ADR §12.1 freezes "every status code and OpenAPI example". #47 adds five v2
    /// paths to the same document, so this asserts the v1 half of it: the exact set
    /// of v1 paths, and the exact set of methods on each. A v2 addition that
    /// accidentally renamed or dropped a v1 operation fails here.
    #[test]
    fn test_v1_openapi_paths_are_unchanged() {
        let spec = match ApiDoc::openapi().to_json() {
            Ok(json) => json,
            Err(error) => panic!("the OpenAPI document must render: {error}"),
        };
        let parsed: Value = match serde_json::from_str(&spec) {
            Ok(parsed) => parsed,
            Err(error) => panic!("the OpenAPI document must parse: {error}"),
        };
        let paths = match parsed.get("paths").and_then(Value::as_object) {
            Some(paths) => paths,
            None => panic!("the document must carry paths"),
        };

        let mut v1_paths: Vec<&str> = paths
            .keys()
            .map(String::as_str)
            .filter(|path| path.starts_with("/api/v1/"))
            .collect();
        v1_paths.sort_unstable();
        assert_eq!(v1_paths, vec!["/api/v1/chain", "/api/v1/chain/step"]);

        let mut chain_methods: Vec<&str> =
            match paths.get("/api/v1/chain").and_then(Value::as_object) {
                Some(operations) => operations.keys().map(String::as_str).collect(),
                None => panic!("/api/v1/chain must be documented"),
            };
        chain_methods.sort_unstable();
        assert_eq!(chain_methods, vec!["delete", "get", "patch", "post", "put"]);

        let step_methods: Vec<&str> =
            match paths.get("/api/v1/chain/step").and_then(Value::as_object) {
                Some(operations) => operations.keys().map(String::as_str).collect(),
                None => panic!("/api/v1/chain/step must be documented"),
            };
        assert_eq!(step_methods, vec!["post"]);
    }

    /// The v1 operations keep their documented status codes.
    ///
    /// The other half of §12.1's OpenAPI freeze: a cleanup that dropped the `412`
    /// from the advance, or the `410` from either serving path, would be a silent
    /// contract change.
    #[test]
    fn test_v1_openapi_status_codes_are_unchanged() {
        let spec = match ApiDoc::openapi().to_json() {
            Ok(json) => json,
            Err(error) => panic!("the OpenAPI document must render: {error}"),
        };
        let parsed: Value = match serde_json::from_str(&spec) {
            Ok(parsed) => parsed,
            Err(error) => panic!("the OpenAPI document must parse: {error}"),
        };

        let codes = |path: &str, method: &str| -> Vec<String> {
            let responses = parsed
                .get("paths")
                .and_then(|paths| paths.get(path))
                .and_then(|operations| operations.get(method))
                .and_then(|operation| operation.get("responses"))
                .and_then(Value::as_object);
            match responses {
                Some(responses) => {
                    let mut codes: Vec<String> = responses.keys().cloned().collect();
                    codes.sort_unstable();
                    codes
                }
                None => panic!("{method} {path} must document its responses"),
            }
        };

        assert_eq!(
            codes("/api/v1/chain/step", "post"),
            vec!["200", "404", "409", "410", "412", "500"]
        );
        assert_eq!(
            codes("/api/v1/chain", "get"),
            vec!["200", "404", "410", "500"]
        );
    }
}
