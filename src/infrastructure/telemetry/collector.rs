use prometheus::{Gauge, Histogram, HistogramVec, IntCounter, IntCounterVec, IntGauge, Registry};
use std::time::Duration;

/// MetricsCollector is responsible for tracking and exporting API metrics
pub struct MetricsCollector {
    registry: Registry,
    // Request metrics
    request_counter: IntCounterVec,
    request_duration: HistogramVec,
    error_counter: IntCounterVec,

    // Session metrics
    active_sessions: IntGauge,
    session_creation_counter: IntCounter,
    session_deletion_counter: IntCounter,

    // Simulation metrics
    simulation_steps_counter: IntCounterVec,
    simulation_duration: Histogram,

    // Cache metrics
    cache_hit_counter: IntCounter,
    cache_miss_counter: IntCounter,

    // Resource metrics
    memory_usage: Gauge,

    // MongoDB metrics
    mongodb_insert_counter: IntCounterVec, // Count inserts by collection
    mongodb_insert_duration: HistogramVec, // Track latency of inserts by collection
}

impl MetricsCollector {
    /// Creates a new metrics collector with all metrics registered
    pub fn new() -> Result<Self, prometheus::Error> {
        let registry = Registry::new();

        // Create request metrics
        let request_counter = IntCounterVec::new(
            prometheus::opts!("api_requests_total", "Total number of API requests"),
            &["endpoint", "method", "status"],
        )?;

        let request_duration = HistogramVec::new(
            prometheus::histogram_opts!(
                "api_request_duration_seconds",
                "HTTP request duration in seconds",
                vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0]
            ),
            &["endpoint", "method"],
        )?;

        let error_counter = IntCounterVec::new(
            prometheus::opts!("api_errors_total", "Total number of API errors"),
            &["endpoint", "error_type"],
        )?;

        // Create session metrics
        let active_sessions = IntGauge::new(
            "active_sessions",
            "Number of currently active simulation sessions",
        )?;

        let session_creation_counter = IntCounter::new(
            "session_creations_total",
            "Total number of sessions created",
        )?;

        let session_deletion_counter = IntCounter::new(
            "session_deletions_total",
            "Total number of sessions deleted",
        )?;

        // Create simulation metrics
        let simulation_steps_counter = IntCounterVec::new(
            prometheus::opts!(
                "simulation_steps_total",
                "Total number of simulation steps processed"
            ),
            &["method"],
        )?;

        let simulation_duration = Histogram::with_opts(prometheus::histogram_opts!(
            "simulation_step_duration_seconds",
            "Simulation step processing time in seconds",
            vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0]
        ))?;

        // Create cache metrics
        let cache_hit_counter = IntCounter::new("cache_hits_total", "Total number of cache hits")?;

        let cache_miss_counter =
            IntCounter::new("cache_misses_total", "Total number of cache misses")?;

        // Create resource metrics
        let memory_usage = Gauge::new("memory_usage_bytes", "Current memory usage in bytes")?;

        // Create MongoDB metrics
        let mongodb_insert_counter = IntCounterVec::new(
            prometheus::opts!(
                "mongodb_inserts_total",
                "Total number of MongoDB insert operations"
            ),
            &["collection"],
        )?;

        let mongodb_insert_duration = HistogramVec::new(
            prometheus::histogram_opts!(
                "mongodb_insert_duration_seconds",
                "MongoDB insert operation duration in seconds",
                vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0]
            ),
            &["collection"],
        )?;

        // Register all metrics with the registry
        registry.register(Box::new(request_counter.clone()))?;
        registry.register(Box::new(request_duration.clone()))?;
        registry.register(Box::new(error_counter.clone()))?;
        registry.register(Box::new(active_sessions.clone()))?;
        registry.register(Box::new(session_creation_counter.clone()))?;
        registry.register(Box::new(session_deletion_counter.clone()))?;
        registry.register(Box::new(simulation_steps_counter.clone()))?;
        registry.register(Box::new(simulation_duration.clone()))?;
        registry.register(Box::new(cache_hit_counter.clone()))?;
        registry.register(Box::new(cache_miss_counter.clone()))?;
        registry.register(Box::new(memory_usage.clone()))?;

        // Register MongoDB metrics
        registry.register(Box::new(mongodb_insert_counter.clone()))?;
        registry.register(Box::new(mongodb_insert_duration.clone()))?;

        Ok(Self {
            registry,
            request_counter,
            request_duration,
            error_counter,
            active_sessions,
            session_creation_counter,
            session_deletion_counter,
            simulation_steps_counter,
            simulation_duration,
            cache_hit_counter,
            cache_miss_counter,
            memory_usage,
            mongodb_insert_counter,
            mongodb_insert_duration,
        })
    }

    /// Records a new API request
    pub fn record_request(&self, endpoint: &str, method: &str, status: &str) {
        self.request_counter
            .with_label_values(&[endpoint, method, status])
            .inc();
    }

    /// Records the duration of an API request
    pub fn record_request_duration(&self, endpoint: &str, method: &str, duration: Duration) {
        self.request_duration
            .with_label_values(&[endpoint, method])
            .observe(duration.as_secs_f64());
    }

    /// Records an API error
    pub fn record_error(&self, endpoint: &str, error_type: &str) {
        self.error_counter
            .with_label_values(&[endpoint, error_type])
            .inc();
    }

    /// Increments the active sessions counter
    pub fn increment_active_sessions(&self) {
        self.active_sessions.inc();
        self.session_creation_counter.inc();
    }

    /// Decrements the active sessions counter
    pub fn decrement_active_sessions(&self) {
        self.active_sessions.dec();
        self.session_deletion_counter.inc();
    }

    /// Records a simulation step
    pub fn record_simulation_step(&self, method: &str) {
        self.simulation_steps_counter
            .with_label_values(&[method])
            .inc();
    }

    /// Records the duration of a simulation step
    pub fn record_simulation_duration(&self, duration: Duration) {
        self.simulation_duration.observe(duration.as_secs_f64());
    }

    /// Records a cache hit
    pub fn record_cache_hit(&self) {
        self.cache_hit_counter.inc();
    }

    /// Records a cache miss
    pub fn record_cache_miss(&self) {
        self.cache_miss_counter.inc();
    }

    /// Records the current memory usage
    pub fn record_memory_usage(&self, bytes: f64) {
        self.memory_usage.set(bytes);
    }

    /// Records a MongoDB insert operation
    pub fn record_mongodb_insert(&self, collection: &str) {
        self.mongodb_insert_counter
            .with_label_values(&[collection])
            .inc();
    }

    /// Records the duration of a MongoDB insert operation
    pub fn record_mongodb_insert_duration(&self, collection: &str, duration: Duration) {
        self.mongodb_insert_duration
            .with_label_values(&[collection])
            .observe(duration.as_secs_f64());
    }

    /// Returns the current metric registry
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Exports metrics in Prometheus text format
    pub fn export_metrics(&self) -> String {
        use prometheus::Encoder;
        let encoder = prometheus::TextEncoder::new();

        let mut buffer = Vec::new();
        encoder
            .encode(&self.registry.gather(), &mut buffer)
            .expect("Failed to encode metrics");

        String::from_utf8(buffer).unwrap_or_else(|_| "Failed to export metrics".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_new_metrics_collector() {
        // Test that a new MetricsCollector can be created without errors
        let collector = MetricsCollector::new();
        assert!(collector.is_ok(), "Failed to create MetricsCollector");
    }

    #[test]
    fn test_record_request() {
        // Create a new MetricsCollector
        let collector = MetricsCollector::new().expect("Failed to create MetricsCollector");

        // Record a request
        collector.record_request("/api/v1/chain", "GET", "200");

        // Check that the counter was incremented
        let metrics = collector.export_metrics();
        assert!(metrics.contains(
            "api_requests_total{endpoint=\"/api/v1/chain\",method=\"GET\",status=\"200\"} 1"
        ));
    }

    #[test]
    fn test_record_multiple_requests() {
        // Create a new MetricsCollector
        let collector = MetricsCollector::new().expect("Failed to create MetricsCollector");

        // Record multiple requests
        collector.record_request("/api/v1/chain", "GET", "200");
        collector.record_request("/api/v1/chain", "GET", "200");
        collector.record_request("/api/v1/chain", "POST", "201");

        // Check that the counters were incremented correctly
        let metrics = collector.export_metrics();
        assert!(metrics.contains(
            "api_requests_total{endpoint=\"/api/v1/chain\",method=\"GET\",status=\"200\"} 2"
        ));
        assert!(metrics.contains(
            "api_requests_total{endpoint=\"/api/v1/chain\",method=\"POST\",status=\"201\"} 1"
        ));
    }

    #[test]
    fn test_record_request_duration() {
        // Create a new MetricsCollector
        let collector = MetricsCollector::new().expect("Failed to create MetricsCollector");

        // Record request duration
        collector.record_request_duration("/api/v1/chain", "GET", Duration::from_millis(100));

        // Check that the histogram was updated
        let metrics = collector.export_metrics();
        assert!(metrics.contains("api_request_duration_seconds_bucket{endpoint=\"/api/v1/chain\",method=\"GET\",le=\"0.1\"} 1"));
        assert!(metrics.contains(
            "api_request_duration_seconds_count{endpoint=\"/api/v1/chain\",method=\"GET\"} 1"
        ));
    }

    #[test]
    fn test_record_error() {
        // Create a new MetricsCollector
        let collector = MetricsCollector::new().expect("Failed to create MetricsCollector");

        // Record an error
        collector.record_error("/api/v1/chain", "not_found");

        // Check that the counter was incremented
        let metrics = collector.export_metrics();
        assert!(
            metrics.contains(
                "api_errors_total{endpoint=\"/api/v1/chain\",error_type=\"not_found\"} 1"
            )
        );
    }

    #[test]
    fn test_active_sessions() {
        // Create a new MetricsCollector
        let collector = MetricsCollector::new().expect("Failed to create MetricsCollector");

        // Increment and decrement active sessions
        collector.increment_active_sessions();
        collector.increment_active_sessions();
        collector.decrement_active_sessions();

        // Check that the gauge and counters were updated correctly
        let metrics = collector.export_metrics();
        assert!(metrics.contains("active_sessions 1"));
        assert!(metrics.contains("session_creations_total 2"));
        assert!(metrics.contains("session_deletions_total 1"));
    }

    #[test]
    fn test_simulation_metrics() {
        // Create a new MetricsCollector
        let collector = MetricsCollector::new().expect("Failed to create MetricsCollector");

        // Record simulation metrics
        collector.record_simulation_step("GeometricBrownian");
        collector.record_simulation_step("GeometricBrownian");
        collector.record_simulation_duration(Duration::from_millis(50));

        // Check that the counters and histogram were updated
        let metrics = collector.export_metrics();
        assert!(metrics.contains("simulation_steps_total{method=\"GeometricBrownian\"} 2"));
        assert!(metrics.contains("simulation_step_duration_seconds_bucket{le=\"0.05\"} 1"));
        assert!(metrics.contains("simulation_step_duration_seconds_count 1"));
    }

    #[test]
    fn test_cache_metrics() {
        // Create a new MetricsCollector
        let collector = MetricsCollector::new().expect("Failed to create MetricsCollector");

        // Record cache hits and misses
        collector.record_cache_hit();
        collector.record_cache_hit();
        collector.record_cache_miss();

        // Check that the counters were incremented correctly
        let metrics = collector.export_metrics();
        assert!(metrics.contains("cache_hits_total 2"));
        assert!(metrics.contains("cache_misses_total 1"));
    }

    #[test]
    fn test_memory_usage() {
        // Create a new MetricsCollector
        let collector = MetricsCollector::new().expect("Failed to create MetricsCollector");

        // Record memory usage
        collector.record_memory_usage(1024.0 * 1024.0); // 1MB

        // Check that the gauge was updated
        let metrics = collector.export_metrics();
        assert!(metrics.contains("memory_usage_bytes 1048576"));
    }

    #[test]
    fn test_mongodb_metrics() {
        // Create a new MetricsCollector
        let collector = MetricsCollector::new().expect("Failed to create MetricsCollector");

        // Record MongoDB operations
        collector.record_mongodb_insert("sessions");
        collector.record_mongodb_insert("sessions");
        collector.record_mongodb_insert("chains");
        collector.record_mongodb_insert_duration("sessions", Duration::from_millis(30));

        // Check that the counters and histogram were updated
        let metrics = collector.export_metrics();
        assert!(metrics.contains("mongodb_inserts_total{collection=\"sessions\"} 2"));
        assert!(metrics.contains("mongodb_inserts_total{collection=\"chains\"} 1"));
        assert!(metrics.contains(
            "mongodb_insert_duration_seconds_bucket{collection=\"sessions\",le=\"0.05\"} 1"
        ));
        assert!(
            metrics.contains("mongodb_insert_duration_seconds_count{collection=\"sessions\"} 1")
        );
    }

    #[test]
    fn test_registry() {
        // Create a new MetricsCollector
        let collector = MetricsCollector::new().expect("Failed to create MetricsCollector");

        // Get the registry
        let registry = collector.registry();

        // Check that it's a valid Registry
        assert!(
            !registry.gather().is_empty(),
            "Registry should contain metrics"
        );
    }

    #[test]
    fn test_export_metrics_format() {
        // Create a new MetricsCollector
        let collector = MetricsCollector::new().expect("Failed to create MetricsCollector");

        // Record various metrics
        collector.record_request("/api/v1/chain", "GET", "200");
        collector.increment_active_sessions();

        // Export metrics
        let metrics = collector.export_metrics();

        // Check the format of the exported metrics
        assert!(
            metrics.contains("# HELP"),
            "Metrics should contain HELP comments"
        );
        assert!(
            metrics.contains("# TYPE"),
            "Metrics should contain TYPE comments"
        );
        assert!(
            metrics.contains("api_requests_total"),
            "Metrics should contain request counter"
        );
        assert!(
            metrics.contains("active_sessions"),
            "Metrics should contain active sessions gauge"
        );
    }
}
