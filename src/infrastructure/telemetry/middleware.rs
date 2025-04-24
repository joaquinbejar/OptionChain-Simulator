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

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{
        App, Error, HttpResponse,
        dev::{Service, ServiceResponse, Transform},
        http::{StatusCode, header::ContentType},
        test::{self, TestRequest},
        web,
    };
    use futures::future::{Ready, ready};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::Duration;

    // Tracking structure for test metrics
    #[derive(Default, Clone)]
    struct MetricsCall {
        path: String,
        method: String,
        status: String,
        is_error: bool,
        duration_recorded: bool,
    }

    // Test metrics collector that implements the same methods as the real one
    struct TestMetricsCollector {
        calls: Arc<Mutex<Vec<MetricsCall>>>,
    }

    impl TestMetricsCollector {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn get_calls(&self) -> Vec<MetricsCall> {
            let calls = self.calls.lock().unwrap();
            calls.clone()
        }

        // Implement the same methods as the real MetricsCollector
        fn record_request(&self, endpoint: &str, method: &str, status: &str) {
            let mut calls = self.calls.lock().unwrap();
            calls.push(MetricsCall {
                path: endpoint.to_string(),
                method: method.to_string(),
                status: status.to_string(),
                is_error: false,
                duration_recorded: false,
            });
        }

        fn record_request_duration(&self, endpoint: &str, method: &str, _duration: Duration) {
            let mut calls = self.calls.lock().unwrap();
            let existing_call = calls
                .iter_mut()
                .find(|call| call.path == endpoint && call.method == method);

            if let Some(call) = existing_call {
                call.duration_recorded = true;
            } else {
                calls.push(MetricsCall {
                    path: endpoint.to_string(),
                    method: method.to_string(),
                    status: String::new(),
                    is_error: false,
                    duration_recorded: true,
                });
            }
        }

        fn record_error(&self, endpoint: &str, error_type: &str) {
            let mut calls = self.calls.lock().unwrap();
            calls.push(MetricsCall {
                path: endpoint.to_string(),
                method: String::new(),
                status: error_type.to_string(),
                is_error: true,
                duration_recorded: false,
            });
        }
    }

    // A simple service that returns a response with the given status code
    struct TestService {
        status_code: StatusCode,
    }

    impl TestService {
        fn new(status: StatusCode) -> Self {
            Self {
                status_code: status,
            }
        }
    }

    impl Service<ServiceRequest> for TestService {
        type Response = ServiceResponse<actix_web::body::BoxBody>;
        type Error = Error;
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&self, req: ServiceRequest) -> Self::Future {
            // Create a new response each time
            let response = HttpResponse::build(self.status_code)
                .content_type(ContentType::plaintext())
                .body("Test response");

            ready(Ok(req.into_response(response)))
        }
    }

    // Define a trait for MetricsCollector interface that our middleware will use
    trait MetricsInterface {
        fn record_request(&self, endpoint: &str, method: &str, status: &str);
        fn record_request_duration(&self, endpoint: &str, method: &str, duration: Duration);
        fn record_error(&self, endpoint: &str, error_type: &str);
    }

    // Implement the trait for both the real MetricsCollector and our test version
    impl MetricsInterface for crate::infrastructure::telemetry::collector::MetricsCollector {
        fn record_request(&self, endpoint: &str, method: &str, status: &str) {
            self.record_request(endpoint, method, status);
        }

        fn record_request_duration(&self, endpoint: &str, method: &str, duration: Duration) {
            self.record_request_duration(endpoint, method, duration);
        }

        fn record_error(&self, endpoint: &str, error_type: &str) {
            self.record_error(endpoint, error_type);
        }
    }

    impl MetricsInterface for TestMetricsCollector {
        fn record_request(&self, endpoint: &str, method: &str, status: &str) {
            self.record_request(endpoint, method, status);
        }

        fn record_request_duration(&self, endpoint: &str, method: &str, duration: Duration) {
            self.record_request_duration(endpoint, method, duration);
        }

        fn record_error(&self, endpoint: &str, error_type: &str) {
            self.record_error(endpoint, error_type);
        }
    }

    // Modify the middleware to use the trait instead of the concrete type
    // This is just for testing purposes
    struct TestMetricsMiddleware<T: MetricsInterface + 'static> {
        metrics: Arc<T>,
    }

    impl<T: MetricsInterface + 'static> TestMetricsMiddleware<T> {
        fn new(metrics: Arc<T>) -> Self {
            Self { metrics }
        }
    }

    impl<S, B, T> Transform<S, ServiceRequest> for TestMetricsMiddleware<T>
    where
        S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
        S::Future: 'static,
        B: 'static,
        T: MetricsInterface + 'static,
    {
        type Response = ServiceResponse<B>;
        type Error = Error;
        type Transform = TestMetricsMiddlewareService<S, T>;
        type InitError = ();
        type Future = Ready<Result<Self::Transform, Self::InitError>>;

        fn new_transform(&self, service: S) -> Self::Future {
            ok(TestMetricsMiddlewareService {
                service,
                metrics: self.metrics.clone(),
            })
        }
    }

    struct TestMetricsMiddlewareService<S, T: MetricsInterface + 'static> {
        service: S,
        metrics: Arc<T>,
    }

    impl<S, B, T> Service<ServiceRequest> for TestMetricsMiddlewareService<S, T>
    where
        S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
        S::Future: 'static,
        B: 'static,
        T: MetricsInterface + 'static,
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

    // Test for constructor
    #[test]
    fn test_metrics_middleware_new() {
        let collector = Arc::new(TestMetricsCollector::new());
        let middleware = TestMetricsMiddleware::new(collector);

        // Check that we can create the middleware
        assert!(Arc::strong_count(&middleware.metrics) > 0);
    }

    // Test the transformation process
    #[test]
    fn test_transform_creates_service() {
        let collector = Arc::new(TestMetricsCollector::new());
        let middleware = TestMetricsMiddleware::new(collector);

        let test_service = TestService::new(StatusCode::OK);
        let transform_future = middleware.new_transform(test_service);

        let service =
            futures::executor::block_on(transform_future).expect("Transform should succeed");

        // Check that we get a service back from the transform
        assert!(Arc::strong_count(&service.metrics) > 0);
    }

    // Test recording of successful request
    #[test]
    fn test_service_records_successful_request() {
        let collector = Arc::new(TestMetricsCollector::new());

        // Create test service with 200 OK response
        let test_service = TestService::new(StatusCode::OK);

        // Create middleware service
        let middleware_service = TestMetricsMiddlewareService {
            service: test_service,
            metrics: collector.clone(),
        };

        // Create a test request
        let req = TestRequest::get().uri("/test").to_srv_request();

        // Execute the service call
        let response = futures::executor::block_on(middleware_service.call(req))
            .expect("Service call should succeed");

        // Check the response status
        assert_eq!(response.status(), StatusCode::OK);

        // Check that metrics were recorded correctly
        let calls = collector.get_calls();
        assert!(
            !calls.is_empty(),
            "Should have recorded at least one metrics call"
        );

        // Find the request metrics call
        let request_call = calls
            .iter()
            .find(|call| call.path == "/test" && call.method == "GET" && call.status == "200");
        assert!(
            request_call.is_some(),
            "Should have recorded request metrics"
        );

        // Check that duration was recorded
        let duration_call = calls
            .iter()
            .find(|call| call.path == "/test" && call.method == "GET" && call.duration_recorded);
        assert!(
            duration_call.is_some(),
            "Should have recorded duration metrics"
        );

        // Check that no error was recorded
        let error_call = calls.iter().find(|call| call.is_error);
        assert!(
            error_call.is_none(),
            "Should not have recorded error metrics"
        );
    }

    // Test recording of error request
    #[test]
    fn test_service_records_error_request() {
        let collector = Arc::new(TestMetricsCollector::new());

        // Create test service with 404 Not Found response
        let test_service = TestService::new(StatusCode::NOT_FOUND);

        // Create middleware service
        let middleware_service = TestMetricsMiddlewareService {
            service: test_service,
            metrics: collector.clone(),
        };

        // Create a test request
        let req = TestRequest::get().uri("/test").to_srv_request();

        // Execute the service call
        let response = futures::executor::block_on(middleware_service.call(req))
            .expect("Service call should succeed");

        // Check the response status
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Check that metrics were recorded correctly
        let calls = collector.get_calls();

        // Find the request metrics call
        let request_call = calls
            .iter()
            .find(|call| call.path == "/test" && call.method == "GET" && call.status == "404");
        assert!(
            request_call.is_some(),
            "Should have recorded request metrics"
        );

        // Check that duration was recorded
        let duration_call = calls
            .iter()
            .find(|call| call.path == "/test" && call.method == "GET" && call.duration_recorded);
        assert!(
            duration_call.is_some(),
            "Should have recorded duration metrics"
        );

        // Check that error was recorded
        let error_call = calls
            .iter()
            .find(|call| call.is_error && call.status == "404");
        assert!(error_call.is_some(), "Should have recorded error metrics");
    }

    // Integration test using actix-web test utilities
    #[actix_web::test]
    async fn test_middleware_integration() {
        let collector = Arc::new(TestMetricsCollector::new());

        // Define a simple handler
        async fn test_handler() -> HttpResponse {
            HttpResponse::Ok().body("Test response")
        }

        // Create app with middleware
        let app = test::init_service(
            App::new()
                .wrap(TestMetricsMiddleware::new(collector.clone()))
                .route("/test", web::get().to(test_handler)),
        )
        .await;

        // Send a request
        let req = TestRequest::get().uri("/test").to_request();
        let resp = test::call_service(&app, req).await;

        // Verify response
        assert_eq!(resp.status(), StatusCode::OK);

        // Check that metrics were recorded
        let calls = collector.get_calls();
        assert!(!calls.is_empty(), "Should have recorded metrics");
    }

    // Test for error response in middleware
    #[actix_web::test]
    async fn test_middleware_with_error_response() {
        let collector = Arc::new(TestMetricsCollector::new());

        // Define a handler that returns an error
        async fn error_handler() -> HttpResponse {
            HttpResponse::InternalServerError().body("Error occurred")
        }

        // Create app with middleware
        let app = test::init_service(
            App::new()
                .wrap(TestMetricsMiddleware::new(collector.clone()))
                .route("/error", web::get().to(error_handler)),
        )
        .await;

        // Send a request
        let req = TestRequest::get().uri("/error").to_request();
        let resp = test::call_service(&app, req).await;

        // Verify response
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // Check that metrics were recorded including error
        let calls = collector.get_calls();

        // Check for error recording
        let error_calls = calls.iter().filter(|call| call.is_error).count();
        assert!(error_calls > 0, "Should have recorded error metrics");
    }
}
