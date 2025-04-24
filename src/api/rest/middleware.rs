use crate::infrastructure::MetricsCollector;
use actix_web::{HttpResponse, Responder, web};
use std::sync::Arc;

#[utoipa::path(
    get,
    path = "/metrics",
    responses(
        (status = 200, description = "Prometheus metrics", content_type = "text/plain")
    )
)]
pub(crate) async fn metrics_endpoint(
    metrics_collector: web::Data<Arc<MetricsCollector>>,
) -> impl Responder {
    let metrics_text = metrics_collector.export_metrics();
    HttpResponse::Ok()
        .content_type("text/plain")
        .body(metrics_text)
}
