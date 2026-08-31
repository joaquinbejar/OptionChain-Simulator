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
/// a counter reset, and every `rate()` over it is fiction.
///
/// How to aggregate depends on what the metric is, and only counters and
/// histograms aggregate cleanly:
///
/// - **counters** (`api_requests_total`, `api_errors_total`,
///   `session_creations_total`, the cache counters) and **histograms**
///   (`api_request_duration_seconds`): take the rate per instance and sum, the
///   usual `sum(rate(...))`. Summing raw counter values across restarts is not
///   the same thing.
/// - **gauges** (`active_sessions`, `simulation_cache_size`,
///   `memory_usage_bytes`): each is that PROCESS's own view. `active_sessions`
///   in particular is not a deployment total under any aggregation: a create
///   served by one replica and a delete served by another leave the first
///   replica's gauge untouched, and neither reaps nor restarts move it. Read
///   them per instance; the authoritative live-session count is the store's,
///   not a metric's.
#[utoipa::path(
    get,
    path = "/metrics",
    responses(
        (status = 200, description = "This instance's Prometheus metrics. Every series is PER PROCESS: a replicated deployment must be scraped per replica, each as its own target with an instance label. Counters and histograms aggregate as sum(rate(..)) across instances; gauges such as active_sessions do NOT, since a create and a delete served by different replicas leave each gauge partial. Scraping a load-balanced address returns one instance at a time, which reads as a counter reset.", content_type = "text/plain")
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
