use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::api::rest::handlers::create_session,
        // crate::api::rest::handlers::get_next_step,
        // crate::api::rest::handlers::replace_session,
        // crate::api::rest::handlers::update_session,
        // crate::api::rest::handlers::delete_session,
    ),
    components(
        schemas(
            crate::api::rest::responses::OptionContractResponse,
            crate::api::rest::responses::OptionPriceResponse,
            crate::api::rest::responses::SessionInfoResponse,
        )
    ),
    tags(
        (name = "Options-Simulator", description = "Options Simulator endpoints")
    )
)]
pub(crate) struct ApiDoc;
