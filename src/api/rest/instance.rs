//! Which instance answered.
//!
//! A replicated deployment sits behind one address, and every response looks
//! the same whichever process produced it. That is fine until something has to
//! be attributed: an operator asking which replica served a slow request, a
//! dashboard reading a per-process gauge, or a test claiming it exercised more
//! than one instance rather than assuming it (issue #139).
//!
//! Every response therefore carries `X-OCS-Instance`, a value fixed for the
//! life of the process. It says nothing about the host, the container or the
//! network — it is an opaque identity, so it cannot leak where the service
//! runs — and it is stable, so two responses carrying the same value came from
//! the same process and two different values came from two.

use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::http::header::{HeaderName, HeaderValue};
use actix_web::{Error, http::header};
use futures::future::{LocalBoxFuture, Ready, ready};
use std::sync::LazyLock;
use uuid::Uuid;

/// The header every response carries.
pub const INSTANCE_HEADER: &str = "x-ocs-instance";

/// This process's identity, fixed at first use and unchanged until it exits.
///
/// Random rather than derived from the host or the container: the value has to
/// distinguish processes, and anything descriptive would be a detail about the
/// deployment on an unauthenticated response.
static INSTANCE_ID: LazyLock<String> = LazyLock::new(|| Uuid::new_v4().to_string());

/// This instance's identity, for the handlers that report it in a body.
#[must_use]
pub fn instance_id() -> &'static str {
    &INSTANCE_ID
}

/// Stamps every response with the identity of the process that produced it.
pub struct InstanceHeader;

impl<S, B> Transform<S, ServiceRequest> for InstanceHeader
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = InstanceHeaderService<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(InstanceHeaderService { service }))
    }
}

/// The service that does the stamping.
pub struct InstanceHeaderService<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for InstanceHeaderService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let future = self.service.call(request);
        Box::pin(async move {
            let mut response = future.await?;
            // A value that will not parse as a header cannot happen — it is a
            // UUID — but an unwrap here would be a panic on a request path.
            if let Ok(value) = HeaderValue::from_str(instance_id()) {
                response
                    .headers_mut()
                    .insert(HeaderName::from_static(INSTANCE_HEADER), value);
            }
            let _ = header::CONTENT_TYPE;
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, HttpResponse, test, web};

    /// Every response carries the header, and it is the same one twice.
    #[actix_web::test]
    async fn test_every_response_names_the_instance() {
        let app = test::init_service(App::new().wrap(InstanceHeader).route(
            "/probe",
            web::get().to(|| async { HttpResponse::Ok().finish() }),
        ))
        .await;

        let first =
            test::call_service(&app, test::TestRequest::get().uri("/probe").to_request()).await;
        let second =
            test::call_service(&app, test::TestRequest::get().uri("/probe").to_request()).await;

        let read = |response: &ServiceResponse<_>| -> String {
            response
                .headers()
                .get(INSTANCE_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };

        let one = read(&first);
        assert!(!one.is_empty(), "every response must name its instance");
        assert_eq!(one, read(&second), "the identity is fixed for the process");
        assert_eq!(one, instance_id(), "and it is the one the handlers report");
    }

    /// The identity says nothing about where the service runs.
    #[actix_web::test]
    async fn test_the_identity_is_opaque() {
        let id = instance_id();
        assert!(
            Uuid::parse_str(id).is_ok(),
            "the identity must be an opaque uuid, it was {id}"
        );
    }
}
