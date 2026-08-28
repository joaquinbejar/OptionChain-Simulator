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
        crate::api::rest::export::export_simulation,
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
            crate::api::rest::greeks::GreeksResponse,
            crate::api::rest::greeks::FirstOrderGreeks,
            crate::api::rest::greeks::FullGreeks,
            crate::api::rest::greeks::GreekLevel,
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

    /// Every chain-serving operation advertises the `greeks` parameter, names
    /// its three values, and states the per-one-long-contract convention — the
    /// one thing a client cannot infer from the numbers and would otherwise
    /// double-count.
    #[test]
    fn test_the_openapi_document_describes_the_greek_parameter() {
        let spec = match ApiDoc::openapi().to_json() {
            Ok(json) => json,
            Err(error) => panic!("the OpenAPI document must render: {error}"),
        };
        let parsed: Value = match serde_json::from_str(&spec) {
            Ok(parsed) => parsed,
            Err(error) => panic!("the OpenAPI document must parse: {error}"),
        };

        let serving = [
            ("/api/v1/chain", "get"),
            ("/api/v1/chain/step", "post"),
            ("/api/v2/simulations/{id}/snapshot", "get"),
            ("/api/v2/simulations/{id}/step", "post"),
        ];

        for (path, method) in serving {
            let parameters = parsed
                .get("paths")
                .and_then(|paths| paths.get(path))
                .and_then(|operations| operations.get(method))
                .and_then(|operation| operation.get("parameters"))
                .and_then(Value::as_array);
            let parameters = match parameters {
                Some(parameters) => parameters,
                None => panic!("{method} {path} must document its parameters"),
            };
            let greeks = parameters
                .iter()
                .find(|parameter| parameter.get("name").and_then(Value::as_str) == Some("greeks"));
            let greeks = match greeks {
                Some(greeks) => greeks,
                None => panic!("{method} {path} must advertise the greeks parameter"),
            };
            assert_eq!(
                greeks.get("in").and_then(Value::as_str),
                Some("query"),
                "{method} {path}"
            );
            let description = greeks
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            for expected in ["`none`", "`first`", "`all`", "ONE LONG CONTRACT"] {
                assert!(
                    description.contains(expected),
                    "{method} {path} must document {expected}, got {description}"
                );
            }
        }
    }

    /// The `greeks` parameter is published as a closed enum, not a free string.
    ///
    /// A generated client then cannot send a fourth value at all, and a reader
    /// of the document sees the vocabulary without reading the prose. The typed
    /// 400 stays the runtime backstop for a hand-written client.
    #[test]
    fn test_the_greek_parameter_is_published_as_an_enum() {
        let spec = match ApiDoc::openapi().to_json() {
            Ok(json) => json,
            Err(error) => panic!("the OpenAPI document must render: {error}"),
        };
        let parsed: Value = match serde_json::from_str(&spec) {
            Ok(parsed) => parsed,
            Err(error) => panic!("the OpenAPI document must parse: {error}"),
        };

        let level = parsed
            .get("components")
            .and_then(|components| components.get("schemas"))
            .and_then(|schemas| schemas.get("GreekLevel"));
        let level = match level {
            Some(level) => level,
            None => panic!("the document must carry the GreekLevel schema"),
        };
        let values: Vec<&str> = level
            .get("enum")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert_eq!(values, vec!["none", "first", "all"], "{level}");

        for (path, method) in [
            ("/api/v1/chain", "get"),
            ("/api/v1/chain/step", "post"),
            ("/api/v2/simulations/{id}/snapshot", "get"),
            ("/api/v2/simulations/{id}/step", "post"),
        ] {
            let greeks = parsed
                .get("paths")
                .and_then(|paths| paths.get(path))
                .and_then(|operations| operations.get(method))
                .and_then(|operation| operation.get("parameters"))
                .and_then(Value::as_array)
                .and_then(|parameters| {
                    parameters.iter().find(|parameter| {
                        parameter.get("name").and_then(Value::as_str) == Some("greeks")
                    })
                });
            let schema = match greeks.and_then(|greeks| greeks.get("schema")) {
                Some(schema) => schema.to_string(),
                None => panic!("{method} {path} must give the greeks parameter a schema"),
            };
            assert!(
                schema.contains("GreekLevel"),
                "{method} {path} must type the parameter as the enum, got {schema}"
            );
        }
    }

    /// The greek payload types reach the document, so a generated client has
    /// something to deserialise the new field into.
    #[test]
    fn test_the_openapi_document_carries_the_greek_schemas() {
        let spec = match ApiDoc::openapi().to_json() {
            Ok(json) => json,
            Err(error) => panic!("the OpenAPI document must render: {error}"),
        };
        let parsed: Value = match serde_json::from_str(&spec) {
            Ok(parsed) => parsed,
            Err(error) => panic!("the OpenAPI document must parse: {error}"),
        };
        let schemas = parsed
            .get("components")
            .and_then(|components| components.get("schemas"))
            .and_then(Value::as_object);
        let schemas = match schemas {
            Some(schemas) => schemas,
            None => panic!("the document must carry component schemas"),
        };

        for name in ["GreeksResponse", "FirstOrderGreeks", "FullGreeks"] {
            assert!(
                schemas.contains_key(name),
                "the document must carry the {name} schema"
            );
        }

        // The quote types carry the optional field the parameter fills in.
        for quote in ["OptionPriceResponse", "OptionQuoteResponse"] {
            let properties = schemas
                .get(quote)
                .and_then(|schema| schema.get("properties"))
                .and_then(Value::as_object);
            match properties {
                Some(properties) => assert!(
                    properties.contains_key("greeks"),
                    "{quote} must document the greeks field"
                ),
                None => panic!("{quote} must document its properties"),
            }
        }
    }

    /// The two branches of the greek payload are mutually exclusive.
    ///
    /// `GreeksResponse` is an untagged enum, which utoipa renders as a `oneOf`
    /// — "valid against exactly one". `FirstOrderGreeks` requires only `theta`
    /// and `vega`, so without `additionalProperties: false` a full twelve-value
    /// snapshot would satisfy BOTH branches and every `greeks=all` response
    /// would be invalid against the document this service publishes: strict
    /// validators reject it, and the `oneOf` deserialisers openapi-generator
    /// emits for Java, C# and Python raise "multiple matches found". Serde is
    /// unaffected either way because it resolves by variant order, so nothing
    /// but this test would notice.
    #[test]
    fn test_the_two_greek_branches_are_mutually_exclusive() {
        let spec = match ApiDoc::openapi().to_json() {
            Ok(json) => json,
            Err(error) => panic!("the OpenAPI document must render: {error}"),
        };
        let parsed: Value = match serde_json::from_str(&spec) {
            Ok(parsed) => parsed,
            Err(error) => panic!("the OpenAPI document must parse: {error}"),
        };
        let schema = parsed
            .get("components")
            .and_then(|components| components.get("schemas"))
            .and_then(|schemas| schemas.get("GreeksResponse"));
        let schema = match schema {
            Some(schema) => schema,
            None => panic!("the document must carry the GreeksResponse schema"),
        };

        let branches = match schema.get("oneOf").and_then(Value::as_array) {
            Some(branches) => branches,
            None => panic!("GreeksResponse must render as a oneOf: {schema}"),
        };
        let refs: Vec<&str> = branches
            .iter()
            .filter_map(|branch| branch.get("$ref").and_then(Value::as_str))
            .collect();
        assert_eq!(
            refs,
            vec![
                "#/components/schemas/FullGreeks",
                "#/components/schemas/FirstOrderGreeks",
            ],
            "the full snapshot must come first, as it does for serde"
        );

        // What makes the `oneOf` satisfiable: the narrow branch is closed, so
        // a twelve-value payload cannot also match it.
        let first_order = parsed
            .get("components")
            .and_then(|components| components.get("schemas"))
            .and_then(|schemas| schemas.get("FirstOrderGreeks"));
        let first_order = match first_order {
            Some(first_order) => first_order,
            None => panic!("the document must carry the FirstOrderGreeks schema"),
        };
        assert_eq!(
            first_order.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "FirstOrderGreeks must be closed or the oneOf is ambiguous: {first_order}"
        );
    }

    /// Every operation that takes the parameter documents a `400` for it, once,
    /// and says which of the two bodies a client will get.
    ///
    /// utoipa keys responses by status, so a second `400` entry silently
    /// replaces the first: an edit that appended one instead of replacing it
    /// would publish the description without the greek wording, and nothing
    /// else would notice.
    #[test]
    fn test_every_greek_operation_documents_its_400() {
        let spec = match ApiDoc::openapi().to_json() {
            Ok(json) => json,
            Err(error) => panic!("the OpenAPI document must render: {error}"),
        };
        let parsed: Value = match serde_json::from_str(&spec) {
            Ok(parsed) => parsed,
            Err(error) => panic!("the OpenAPI document must parse: {error}"),
        };

        for (path, method) in [
            ("/api/v1/chain", "get"),
            ("/api/v1/chain/step", "post"),
            ("/api/v2/simulations/{id}/snapshot", "get"),
            ("/api/v2/simulations/{id}/step", "post"),
        ] {
            let bad_request = parsed
                .get("paths")
                .and_then(|paths| paths.get(path))
                .and_then(|operations| operations.get(method))
                .and_then(|operation| operation.get("responses"))
                .and_then(|responses| responses.get("400"));
            let bad_request = match bad_request {
                Some(bad_request) => bad_request,
                None => panic!("{method} {path} must document a 400"),
            };

            let description = bad_request
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                description.contains("greeks"),
                "{method} {path} must say the 400 covers an unknown greek level, got {description}"
            );
            // These operations answer 400 with two different bodies — the
            // typed `{error, field}` for a rejected level, and `{error}` alone
            // for a malformed id or a terminal state — so there is no single
            // schema to publish. The description has to say which is which
            // instead of the document promising a `field` that is sometimes
            // absent.
            assert!(
                description.contains("ValidationErrorResponse"),
                "{method} {path} must say when the 400 body is the typed one, got {description}"
            );
            assert!(
                bad_request.get("content").is_none(),
                "{method} {path} must not promise one body schema for two shapes"
            );
        }
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

        // `400` joined both serving paths when the `greeks` parameter did. It
        // is a documentation completion rather than a new outcome: a malformed
        // `sessionid` has always been mapped to `400` through
        // `ChainError::InvalidState`, and the document simply never said so.
        // Every code that was frozen is still here; none was dropped.
        assert_eq!(
            codes("/api/v1/chain/step", "post"),
            vec!["200", "400", "404", "409", "410", "412", "500"]
        );
        assert_eq!(
            codes("/api/v1/chain", "get"),
            vec!["200", "400", "404", "410", "500"]
        );
    }
}
