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
/// A gate that is UP and full must make the caller wait rather than report
/// failure. Falling back to the local bound under saturation would bypass the
/// deployment-wide cap exactly when it is doing its job, so the local bound is
/// reserved for a gate that cannot be reached at all.
#[async_trait]
pub trait SharedPricingGate: Send + Sync {
    /// Takes a lease, waiting as long as it takes for one to come free.
    ///
    /// Returns the token to release, or `None` ONLY when the gate itself could
    /// not be reached, which means "carry on under the local bound". A full
    /// gate is not a failure and must not return `None`: the wait happens in
    /// the caller's future, so a client that goes away while queued cancels it
    /// for free.
    async fn acquire(&self) -> Option<String>;

    /// Refreshes a held lease. `false` means it is no longer held.
    ///
    /// Leases expire so that a killed instance cannot hold one forever, which
    /// means a job outliving that window would otherwise lose its lease while
    /// still burning the CPU it was leased for. The supervisor renews every
    /// [`SharedPricingGate::renewal_interval`] until the job finishes.
    async fn renew(&self, token: &str) -> bool;

    /// How often a held lease is refreshed.
    ///
    /// Must be comfortably shorter than the implementation's expiry window, or
    /// the sweep reaps a lease the renewal was about to refresh.
    fn renewal_interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    /// Gives a lease back. Best effort: a lease that is never released expires
    /// on its own, which is what keeps a killed instance from holding one
    /// forever.
    async fn release(&self, token: &str);
}

/// The deployment-wide gate, when one has been installed.
static SHARED_GATE: OnceLock<Arc<dyn SharedPricingGate>> = OnceLock::new();

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
/// # What holds the bound
///
/// Both permits are handed to a supervisor task that OUTLIVES this future. A
/// blocking task keeps running when its `JoinHandle` is dropped, so releasing
/// on cancellation would let the CPU work continue outside the bound it was
/// admitted under: a burst of clients that disconnect would run unbounded
/// pricing. The caller waits on a channel instead, and dropping it cancels the
/// wait, not the accounting.
///
/// Everything before the spawn is cancellable and costs nothing: a client that
/// goes away while queued for either permit takes no slot with it.
///
/// # Errors
///
/// Returns [`ChainError::Internal`] if the semaphore is closed, if the
/// blocking task panics or is dropped, or if the supervisor disappears without
/// answering, and whatever the job itself returns.
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
        Some(gate) => gate.acquire().await,
        None => None,
    };

    // No `.await` between taking the lease and handing it to the supervisor,
    // so there is no point at which a cancelled caller can drop a lease it has
    // already taken.
    let (answer, wait) = tokio::sync::oneshot::channel();
    let gate = SHARED_GATE.get().cloned();
    tokio::spawn(async move {
        let outcome = supervise(job, gate.as_ref(), lease.as_deref()).await;
        drop(permit);
        // A caller that went away is the normal case here, not a failure: the
        // work still had to finish under the bound, which is why we ran it.
        let _ = answer.send(outcome);
    });

    match wait.await {
        Ok(outcome) => outcome?,
        Err(error) => Err(ChainError::Internal(format!(
            "a pricing job did not report back: {error}"
        ))),
    }
}

/// Runs the blocking job, keeping its deployment-wide lease alive meanwhile.
///
/// Separate from [`admit_blocking`] because it must run inside the supervisor
/// task: renewing from the caller's future would stop the moment the client
/// went away, and the sweep would then reap the lease of a job still running.
async fn supervise<T, F>(
    job: F,
    gate: Option<&Arc<dyn SharedPricingGate>>,
    lease: Option<&str>,
) -> Result<Result<T, ChainError>, ChainError>
where
    F: FnOnce() -> Result<T, ChainError> + Send + 'static,
    T: Send + 'static,
{
    let mut running = tokio::task::spawn_blocking(job);

    let outcome = match (gate, lease) {
        (Some(gate), Some(token)) => {
            let interval = gate.renewal_interval();
            let mut renewals =
                tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
            loop {
                tokio::select! {
                    finished = &mut running => break finished,
                    _ = renewals.tick() => {
                        if !gate.renew(token).await {
                            warn!(
                                "a running pricing job's deployment-wide lease is gone; the \
                                 bound may be exceeded until it finishes"
                            );
                        }
                    }
                }
            }
        }
        _ => (&mut running).await,
    };

    // Released before the job's own error is inspected, so a failing job cannot
    // hold a deployment-wide lease until it expires.
    if let (Some(gate), Some(token)) = (gate, lease) {
        gate.release(token).await;
    }

    outcome.map_err(|error| ChainError::Internal(format!("a pricing job did not finish: {error}")))
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

    /// A gate that records what the supervisor asked of it.
    struct RecordingGate {
        renewals: Arc<std::sync::atomic::AtomicUsize>,
        released: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl SharedPricingGate for RecordingGate {
        async fn acquire(&self) -> Option<String> {
            Some("token".to_string())
        }

        async fn renew(&self, _token: &str) -> bool {
            self.renewals
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            true
        }

        fn renewal_interval(&self) -> Duration {
            Duration::from_millis(20)
        }

        async fn release(&self, _token: &str) {
            self.released
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A lease is renewed for as long as its job runs.
    ///
    /// Without this a job outliving the expiry window loses its lease to the
    /// sweep while still burning the CPU it was leased for, and another job
    /// takes the slot it never gave up.
    #[tokio::test]
    async fn test_a_running_job_keeps_its_lease_alive() {
        let renewals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let released = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate: Arc<dyn SharedPricingGate> = Arc::new(RecordingGate {
            renewals: Arc::clone(&renewals),
            released: Arc::clone(&released),
        });

        let outcome = supervise(
            || {
                std::thread::sleep(Duration::from_millis(120));
                Ok(3_usize)
            },
            Some(&gate),
            Some("token"),
        )
        .await;

        match outcome {
            Ok(Ok(value)) => assert_eq!(value, 3),
            other => panic!("the job must run: {other:?}"),
        }
        assert!(
            renewals.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "a job outliving several renewal intervals must have renewed its lease"
        );
        assert_eq!(
            released.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the lease must be given back exactly once"
        );
    }

    /// A job that panics still gives its lease back.
    #[tokio::test]
    async fn test_a_panicking_job_releases_its_lease() {
        let renewals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let released = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate: Arc<dyn SharedPricingGate> = Arc::new(RecordingGate {
            renewals: Arc::clone(&renewals),
            released: Arc::clone(&released),
        });

        let outcome: Result<Result<usize, ChainError>, ChainError> =
            supervise(|| panic!("the job blew up"), Some(&gate), Some("token")).await;

        assert!(
            matches!(outcome, Err(ChainError::Internal(_))),
            "a panicking job must surface as an internal error"
        );
        assert_eq!(
            released.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a panic must not leave a lease held until it expires"
        );
    }

    /// A client that goes away does not release the bound early.
    ///
    /// `spawn_blocking` keeps running once its handle is dropped, so releasing
    /// on cancellation would let that CPU work continue outside the bound it
    /// was admitted under. The permit is therefore held by a supervisor that
    /// outlives the caller, and comes back only when the work actually ends.
    #[tokio::test]
    async fn test_a_cancelled_caller_holds_the_bound_until_its_job_ends() {
        let (started, mut was_started) = tokio::sync::oneshot::channel::<()>();
        let (finish, may_finish) = std::sync::mpsc::channel::<()>();

        let mut job = Box::pin(admit_blocking(move || {
            let _ = started.send(());
            let _ = may_finish.recv();
            Ok(1_usize)
        }));

        // Driven, not timed: the future is polled until its job reports that it
        // started, so a busy semaphore delays this test rather than breaking it.
        let mut waited = Duration::ZERO;
        while was_started.try_recv().is_err() {
            match futures::future::select(
                &mut job,
                Box::pin(tokio::time::sleep(Duration::from_millis(10))),
            )
            .await
            {
                futures::future::Either::Left((outcome, _)) => {
                    panic!("the job cannot finish before it is let go: {outcome:?}")
                }
                futures::future::Either::Right(((), _)) => {}
            }
            waited += Duration::from_millis(10);
            assert!(
                waited < Duration::from_secs(5),
                "the job never started, so there is nothing to cancel"
            );
        }

        // The caller goes away with its job still running.
        drop(job);
        let during = PRICING_PERMITS.available_permits();
        assert!(
            during < configured_jobs(),
            "a cancelled caller must not have given the permit back while its job runs"
        );

        let _ = finish.send(());
        let mut waited = Duration::ZERO;
        while PRICING_PERMITS.available_permits() <= during && waited < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += Duration::from_millis(10);
        }
        assert!(
            PRICING_PERMITS.available_permits() > during,
            "the permit must come back once the job actually finishes"
        );
    }
}
