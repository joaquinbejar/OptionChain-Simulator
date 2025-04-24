use crate::infrastructure::telemetry::collector::MetricsCollector;
use actix_web::{
    Error,
    dev::{Service, ServiceRequest, ServiceResponse, Transform, forward_ready},
};
use futures::future::{LocalBoxFuture, Ready, ok};
use std::sync::Arc;
use std::time::Instant;

/// `MetricsMiddleware` is a struct that acts as middleware for collecting
/// metrics in an application. It facilitates the integration of a `MetricsCollector`
/// into the middleware pipeline by allowing for concurrent access and usage.
///
/// # Fields
/// - `metrics`:
///   An atomically reference-counted pointer (`Arc`) to a `MetricsCollector`.
///   The `MetricsCollector` is responsible for collecting and storing metrics
///   such as request counts, response times, or error rates.
///
/// # Usage
/// This middleware is typically used in server applications where tracking
/// application performance and health are critical. By using an `Arc`,
/// the `MetricsCollector` can be shared safely across multiple threads.
///
/// The `MetricsMiddleware` should be configured to intercept requests and responses,
/// allowing it to record relevant metrics automatically.
///
pub struct MetricsMiddleware {
    metrics: Arc<MetricsCollector>,
}

impl MetricsMiddleware {
    /// Creates a new instance of the struct with the provided `MetricsCollector`.
    ///
    /// # Arguments
    ///
    /// * `metrics` - An `Arc`-wrapped `MetricsCollector` instance to be used
    ///   by the struct for collecting and managing metrics.
    ///
    /// # Returns
    ///
    /// A new instance of the struct containing the provided `MetricsCollector`.
    ///
    pub fn new(metrics: Arc<MetricsCollector>) -> Self {
        Self { metrics }
    }
}

impl<S, B> Transform<S, ServiceRequest> for MetricsMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = MetricsMiddlewareService<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(MetricsMiddlewareService {
            service,
            metrics: self.metrics.clone(),
        })
    }
}

pub struct MetricsMiddlewareService<S> {
    service: S,
    metrics: Arc<MetricsCollector>,
}

impl<S, B> Service<ServiceRequest> for MetricsMiddlewareService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let metrics = self.metrics.clone();
        let path = req.path().to_string();
        let method = req.method().to_string();
        let start_time = Instant::now();

        let fut = self.service.call(req);

        Box::pin(async move {
            let res = fut.await?;
            let status = res.status().as_u16().to_string();
            let duration = start_time.elapsed();

            metrics.record_request(&path, &method, &status);
            metrics.record_request_duration(&path, &method, duration);

            if !res.status().is_success() {
                metrics.record_error(&path, &status);
            }

            Ok(res)
        })
    }
}
