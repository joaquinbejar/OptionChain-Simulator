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
