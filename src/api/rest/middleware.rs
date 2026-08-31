use crate::infrastructure::MetricsCollector;
use actix_web::{HttpResponse, Responder, web};
use std::sync::Arc;

/// Serves this INSTANCE's metrics, in the Prometheus text format.
///
/// Every counter, gauge and histogram here belongs to the process that
/// answers, which is normal and correct for a Prometheus exposition and is the
/// thing to remember when the service runs replicated: the numbers are not a
/// deployment total, they are one instance's share of it.
///
/// A deployment behind one published port therefore has to be scraped **per
/// replica**, each as its own target with its own instance label. Scraping the
/// balanced address instead returns whichever instance answered, so a counter
/// appears to jump up and down between scrapes, Prometheus reads each drop as
/// a counter reset, and every `rate()` over it is fiction. Summing across the
/// instance label is what produces a deployment total.
#[utoipa::path(
    get,
    path = "/metrics",
    responses(
        (status = 200, description = "This instance's Prometheus metrics. Counters are PER PROCESS: a replicated deployment must be scraped per replica, each as its own target with an instance label, and totalled by summing across that label. Scraping a load-balanced address returns one instance at a time, which reads as a counter reset.", content_type = "text/plain")
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
