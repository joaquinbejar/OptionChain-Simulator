//! The service-wide bound on concurrent CPU-heavy jobs.
//!
//! Pricing an option chain is the one thing this service does that is neither
//! I/O nor cheap: a snapshot may price up to `OCS_MAX_SNAPSHOT_CONTRACTS`
//! contracts, and asking for the full greek set costs about half as much again
//! on top. Two problems follow, and this module answers both.
//!
//! **It must not run on the async runtime.** A worker thread executing seconds
//! of pricing is a worker not serving anything else, including the requests
//! that never asked for a chain.
//!
//! **Moving it to `spawn_blocking` alone is not enough.** A blocking task is not
//! cancellable once it starts, so a burst of requests commits the machine to
//! every job in the burst even if every client has since disconnected. The
//! permit is therefore acquired *before* the task is spawned, in async code
//! that a dropped future cancels for free — a client that goes away while queued
//! costs nothing at all.
//!
//! Lives in `utils` rather than in a layer because both the API renderers and
//! the v2 session manager contend for the same cores, and a bound that only one
//! of them respects is not a bound.

use crate::utils::ChainError;
use async_trait::async_trait;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::warn;

/// Default number of pricing jobs allowed to run at once.
///
/// Small on purpose: one job can be seconds of CPU, so the useful default is
/// "a few cores' worth", not "however many requests arrived".
pub const DEFAULT_MAX_CONCURRENT_PRICING_JOBS: usize = 4;

/// How many pricing jobs may run at once (`OCS_MAX_CONCURRENT_PRICING_JOBS`).
///
/// Requests above the bound WAIT rather than being rejected. Waiting is the
/// right answer here: the work is legitimate and will be served, just not all at
/// once, and the queue is bounded in practice by the client timeouts that sit in
/// front of it.
static PRICING_PERMITS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(configured_jobs()));

/// The configured bound, read the same way wherever it is needed.
///
/// The number means one instance for the local semaphore and the whole
/// deployment for the shared gate, which is the point of installing the gate:
/// an operator writes the number they want the deployment to run.
#[must_use]
pub fn configured_jobs() -> usize {
    let raw = super::env::read_var("OCS_MAX_CONCURRENT_PRICING_JOBS");
    raw.as_deref()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|permits| *permits >= 1)
        .unwrap_or_else(|| {
            // Only a value that was actually written and is unusable warrants a
            // warning; a blank one reads as unset (`utils::env`) and falls back
            // in silence, because that is a knob someone commented out.
            if raw.is_some() {
                warn!(
                    default = DEFAULT_MAX_CONCURRENT_PRICING_JOBS,
                    "invalid OCS_MAX_CONCURRENT_PRICING_JOBS; falling back to the default"
                );
            }
            DEFAULT_MAX_CONCURRENT_PRICING_JOBS
        })
}

/// A bound that spans every instance of the deployment.
///
/// The semaphore below bounds ONE process, which is the wrong unit once the
/// service runs replicated: an operator asking for four concurrent pricing
/// jobs and running two replicas gets eight, and finds out when the host
/// saturates (issue #135). A deployment-wide gate closes that, and it is a
/// port rather than an implementation because the thing that can hold it —
/// Redis — belongs to the infrastructure layer, which this module must not
/// depend on.
///
/// Implementations must never fail a request. An unreachable gate reports that
/// it could not lease, and the caller proceeds under the local bound, which is
/// the behaviour the service had before this existed.
#[async_trait]
pub trait SharedPricingGate: Send + Sync {
    /// Takes a lease, waiting up to `deadline` for one to come free.
    ///
    /// Returns the token to release, or `None` when no lease could be taken:
    /// the gate is unreachable, or the wait ran out. Both mean "carry on under
    /// the local bound".
    async fn acquire(&self, deadline: Duration) -> Option<String>;

    /// Gives a lease back. Best effort: a lease that is never released expires
    /// on its own, which is what keeps a killed instance from holding one
    /// forever.
    async fn release(&self, token: &str);
}

/// The deployment-wide gate, when one has been installed.
static SHARED_GATE: OnceLock<Arc<dyn SharedPricingGate>> = OnceLock::new();

/// How long a job waits for a deployment-wide lease before proceeding under
/// the local bound alone.
///
/// Bounded on purpose. Waiting forever would turn a Redis that is up but
/// saturated into an outage, while proceeding leaves the per-process bound in
/// force, which is exactly the guarantee the service had before the shared one
/// existed. It is deliberately longer than a pricing job so a queue drains
/// rather than leaks past the gate.
const SHARED_LEASE_WAIT: Duration = Duration::from_secs(30);

/// Installs the deployment-wide gate. Called once, at startup.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] if a gate was already installed, which is
/// a wiring mistake rather than a runtime condition.
pub fn install_shared_gate(gate: Arc<dyn SharedPricingGate>) -> Result<(), ChainError> {
    SHARED_GATE.set(gate).map_err(|_| {
        ChainError::Internal("the shared pricing gate is already installed".to_string())
    })
}

/// Runs a CPU-heavy job off the runtime, under the shared bound.
///
/// The job is the WHOLE unit of work — pricing and whatever encoding follows it.
/// Splitting them would put the second half back on the worker the first half
/// was moved off, which for a capped snapshot is a stall on its own.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] if the semaphore is closed or the blocking
/// task panics or is dropped, and whatever the job itself returns.
pub async fn admit_blocking<T, F>(job: F) -> Result<T, ChainError>
where
    F: FnOnce() -> Result<T, ChainError> + Send + 'static,
    T: Send + 'static,
{
    let permit = PRICING_PERMITS.acquire().await.map_err(|error| {
        ChainError::Internal(format!("the pricing admission gate is closed: {error}"))
    })?;

    // The local permit first, then the deployment-wide lease: the local one is
    // free and cancellable, so a client that goes away while queued costs
    // nothing, and only a job that has already passed the cheap gate spends a
    // round trip on the shared one.
    let lease = match SHARED_GATE.get() {
        Some(gate) => gate.acquire(SHARED_LEASE_WAIT).await,
        None => None,
    };

    let outcome = tokio::task::spawn_blocking(job)
        .await
        .map_err(|error| ChainError::Internal(format!("a pricing job did not finish: {error}")));

    // Released before the local permit and whatever the job did, so a failing
    // job cannot hold a deployment-wide lease until it expires.
    if let (Some(gate), Some(token)) = (SHARED_GATE.get(), lease) {
        gate.release(&token).await;
    }
    drop(permit);

    outcome?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bound is what the documentation says it is.
    ///
    /// `.env.example` and the crate docs quote the number, so it may not drift
    /// from them silently.
    #[test]
    fn test_the_default_matches_the_documentation() {
        assert_eq!(DEFAULT_MAX_CONCURRENT_PRICING_JOBS, 4);
    }

    /// A job runs, and its value comes back.
    #[tokio::test]
    async fn test_a_job_runs_under_the_bound() {
        match admit_blocking(|| Ok(7_usize)).await {
            Ok(value) => assert_eq!(value, 7),
            Err(error) => panic!("the job must run: {error}"),
        }
    }

    /// A job's own error is propagated rather than being wrapped as an
    /// admission failure, so a caller can still tell the two apart.
    #[tokio::test]
    async fn test_a_job_error_is_propagated() {
        let outcome: Result<usize, ChainError> =
            admit_blocking(|| Err(ChainError::Internal("the job failed".to_string()))).await;

        match outcome {
            Err(ChainError::Internal(message)) => assert_eq!(message, "the job failed"),
            other => panic!("the job's own error must survive, got {other:?}"),
        }
    }

    /// The bound actually bounds: with every permit held, a job waits.
    #[tokio::test]
    async fn test_a_job_waits_when_every_permit_is_held() {
        let held = match PRICING_PERMITS
            .acquire_many(u32::try_from(DEFAULT_MAX_CONCURRENT_PRICING_JOBS).unwrap_or(1))
            .await
        {
            Ok(permits) => permits,
            Err(error) => panic!("the semaphore must hand out its permits: {error}"),
        };

        let mut job = Box::pin(admit_blocking(|| Ok(1_usize)));
        // Not merely slow: with no permit free it cannot start at all.
        match futures::future::select(
            &mut job,
            Box::pin(tokio::time::sleep(std::time::Duration::from_millis(50))),
        )
        .await
        {
            futures::future::Either::Left((outcome, _)) => {
                panic!("the job must not start while the bound is full, got {outcome:?}")
            }
            futures::future::Either::Right(((), _)) => {}
        }

        // Released, it proceeds.
        drop(held);
        match job.await {
            Ok(value) => assert_eq!(value, 1),
            Err(error) => panic!("the job must run once a permit frees: {error}"),
        }
    }
}
