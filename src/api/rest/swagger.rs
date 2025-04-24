use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::api::rest::handlers::create_session,
        crate::api::rest::handlers::get_next_step,
        crate::api::rest::handlers::replace_session,
        crate::api::rest::handlers::update_session,
        crate::api::rest::handlers::delete_session,
    ),
    components(
        schemas(
            crate::api::rest::responses::OptionContractResponse,
            crate::api::rest::responses::OptionPriceResponse,
            crate::api::rest::responses::SessionInfoResponse,
            crate::api::rest::requests::CreateSessionRequest,
            crate::api::rest::requests::UpdateSessionRequest,
            crate::api::rest::models::SessionId,
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
            "/api/v1/chain", // Matches the handlers in the macro
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
}
