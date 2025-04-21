// use actix_web::{
//     dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
//     Error, HttpResponse,
// };
// use futures::future::{ok, Either, Ready};
// use futures::Future;
// use std::pin::Pin;
// use std::task::{Context, Poll};
// use std::time::Instant;
//
// use crate::utils::telemetry::{get_metrics, MetricsCollector};
//
// /// Middleware for exposing Prometheus metrics
// pub struct PrometheusMetrics;
//
// impl<S, B> Transform<S, ServiceRequest> for PrometheusMetrics
// where
//     S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
//     S::Future: 'static,
//     B: 'static,
// {
//     type Response = ServiceResponse<B>;
//     type Error = Error;
//     type Transform = PrometheusMetricsMiddleware<S>;
//     type InitError = ();
//     type Future = Ready<Result<Self::Transform, Self::InitError>>;
//
//     fn new_transform(&self, service: S) -> Self::Future {
//         ok(PrometheusMetricsMiddleware { service })
//     }
// }
//
// pub struct PrometheusMetricsMiddleware<S> {
//     service: S,
// }
//
// impl<S, B> Service<ServiceRequest> for PrometheusMetricsMiddleware<S>
// where
//     S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
//     S::Future: 'static,
//     B: 'static,
// {
//     type Response = ServiceResponse<B>;
//     type Error = Error;
//     type Future = Either<
//         Ready<Result<Self::Response, Self::Error>>,
//         Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>,
//     >;
//
//     forward_ready!(service);
//
//     fn call(&self, req: ServiceRequest) -> Self::Future {
//         // Check if this is a request to the metrics endpoint
//         if req.path() == "/metrics" {
//             // Return metrics in Prometheus format
//             let metrics = get_metrics();
//             Either::Left(ok(
//                 req.into_response(HttpResponse::Ok().body(metrics).into_body())
//             ))
//         } else {
//             // For other requests, measure and record timing
//             let start = Instant::now();
//             let path = req.path().to_string();
//
//             let fut = self.service.call(req);
//
//             Either::Right(Box::pin(async move {
//                 let res = fut.await?;
//                 // Record the request duration
//                 let elapsed = start.elapsed().as_secs_f64();
//                 let collector = MetricsCollector;
//                 collector.record_api_request_duration(&path, elapsed);
//
//                 Ok(res)
//             }))
//         }
//     }
// }
