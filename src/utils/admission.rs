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
use std::sync::LazyLock;
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
static PRICING_PERMITS: LazyLock<Semaphore> = LazyLock::new(|| {
    let raw = super::env::read_var("OCS_MAX_CONCURRENT_PRICING_JOBS");
    let configured = raw
        .as_deref()
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
        });
    Semaphore::new(configured)
});

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

    let outcome = tokio::task::spawn_blocking(job)
        .await
        .map_err(|error| ChainError::Internal(format!("a pricing job did not finish: {error}")))?;

    drop(permit);
    outcome
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
