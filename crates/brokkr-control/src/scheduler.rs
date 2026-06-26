//! Job scheduler: bridges the REAPI `Execute` RPC (client-facing) to the
//! internal `brokkr.v1.WorkerService.Stream` (worker-facing).
//!
//! Multi-worker dispatch follows ADR 0008: each connected worker has its own
//! job channel ([`crate::scheduling::ConnectedWorkers`]); `execute` filters to
//! the eligible (matching + connected) workers and a pluggable
//! [`crate::scheduling::Strategy`] picks one. Global queueing, leases, and fair
//! scheduling are Phase 4 task 4.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use brokkr_cas::{ActionCache, Cas};
use brokkr_common::{Digest, JobId, WorkerId};
use brokkr_proto::brokkr_v1 as bv1;
use brokkr_proto::reapi_v2 as rapi;
use prost::Message;
use thiserror::Error;
use tokio::sync::{oneshot, Mutex};

use crate::matching::eligible_workers;
use crate::scheduling::{ConnectedWorkers, SharedConnectedWorkers, SimpleFifo, Strategy};
use crate::worker_service::SharedWorkerRegistry;

/// Default ceiling on how long [`Scheduler::execute`] waits for a worker to
/// report a result. Issue #63 — without this the oneshot wait was unbounded
/// and a stalled / crashed worker hung the gRPC caller forever. REAPI's
/// `Action.timeout`, when set, overrides this on a per-action basis.
pub const DEFAULT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Errors returned by [`Scheduler::execute`]. Typed (instead of `anyhow`) so
/// the gRPC service can map `Timeout` to `DEADLINE_EXCEEDED` and everything
/// else to `INTERNAL` without string-matching.
#[derive(Error, Debug)]
pub enum ExecutionError {
    /// The worker did not report a result within the per-action / scheduler
    /// timeout. Translates to gRPC `DEADLINE_EXCEEDED` (code 4).
    #[error("worker did not report within {0:?}")]
    Timeout(Duration),
    /// No registered, live worker satisfies the action's platform
    /// constraints. Translates to gRPC `FAILED_PRECONDITION` (code 9) so the
    /// client can distinguish "nothing can run this" from a transient
    /// failure and avoid pointless retries.
    #[error("no eligible worker for the action's platform constraints")]
    NoEligibleWorker,
    /// Catch-all for failures during dispatch (CAS read, action-cache write,
    /// worker reporting an error, etc.). Translates to gRPC `INTERNAL`
    /// (code 13).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Outcome of a scheduled action execution.
#[derive(Debug)]
pub struct ExecutionOutcome {
    /// REAPI ActionResult to return to the caller.
    pub result: rapi::ActionResult,
    /// True if the action cache was hit and execution skipped.
    pub cache_hit: bool,
}

/// Multi-worker job scheduler.
pub struct Scheduler {
    /// Workers with a live `Stream`, each with its own job channel + load.
    /// `WorkerService.Stream` writes connect/disconnect; `execute` routes
    /// through it.
    connected: SharedConnectedWorkers,
    /// Worker-selection policy (ADR 0008). Defaults to `SimpleFifo`.
    strategy: Arc<dyn Strategy>,
    waiters: Mutex<HashMap<JobId, oneshot::Sender<bv1::JobResult>>>,
    cas: Arc<dyn Cas>,
    action_cache: Arc<dyn ActionCache>,
    default_execution_timeout: Duration,
    /// Optional worker registry for platform-constraint filtering. When set,
    /// candidates are narrowed to healthy workers whose capabilities satisfy
    /// the action's platform; when `None` (e.g. fixtures that don't exercise
    /// matching), any connected worker is a candidate.
    worker_registry: Option<SharedWorkerRegistry>,
}

impl Scheduler {
    /// Construct a scheduler bound to the given storage backends, using the
    /// default execution timeout ([`DEFAULT_EXECUTION_TIMEOUT`]) and no
    /// admission control.
    pub fn new(cas: Arc<dyn Cas>, action_cache: Arc<dyn ActionCache>) -> Arc<Self> {
        Self::build(cas, action_cache, DEFAULT_EXECUTION_TIMEOUT, None)
    }

    /// Construct a scheduler with a custom default per-action timeout. The
    /// REAPI `Action.timeout` field (when set on a per-action basis) still
    /// overrides this default.
    pub fn with_execution_timeout(
        cas: Arc<dyn Cas>,
        action_cache: Arc<dyn ActionCache>,
        default_execution_timeout: Duration,
    ) -> Arc<Self> {
        Self::build(cas, action_cache, default_execution_timeout, None)
    }

    /// Construct a scheduler that performs platform-constraint admission
    /// control against `worker_registry`, using the default timeout.
    pub fn with_worker_registry(
        cas: Arc<dyn Cas>,
        action_cache: Arc<dyn ActionCache>,
        worker_registry: SharedWorkerRegistry,
    ) -> Arc<Self> {
        Self::build(
            cas,
            action_cache,
            DEFAULT_EXECUTION_TIMEOUT,
            Some(worker_registry),
        )
    }

    /// Construct a scheduler with both a custom timeout and admission control.
    pub fn with_registry_and_timeout(
        cas: Arc<dyn Cas>,
        action_cache: Arc<dyn ActionCache>,
        default_execution_timeout: Duration,
        worker_registry: SharedWorkerRegistry,
    ) -> Arc<Self> {
        Self::build(
            cas,
            action_cache,
            default_execution_timeout,
            Some(worker_registry),
        )
    }

    fn build(
        cas: Arc<dyn Cas>,
        action_cache: Arc<dyn ActionCache>,
        default_execution_timeout: Duration,
        worker_registry: Option<SharedWorkerRegistry>,
    ) -> Arc<Self> {
        Arc::new(Self {
            connected: Arc::new(Mutex::new(ConnectedWorkers::new())),
            strategy: Arc::new(SimpleFifo),
            waiters: Mutex::new(HashMap::new()),
            cas,
            action_cache,
            default_execution_timeout,
            worker_registry,
        })
    }

    /// The shared connected-worker registry. `WorkerService.Stream` registers
    /// each worker's job channel here on connect and removes it on disconnect;
    /// `execute` routes jobs through it.
    pub fn connected_workers(&self) -> SharedConnectedWorkers {
        self.connected.clone()
    }

    /// Execute an action: look up the action cache, otherwise enqueue a job
    /// and await the worker's report.
    #[tracing::instrument(
        name = "control::dispatch",
        skip(self),
        fields(
            action_digest = %action_digest,
            skip_cache_lookup,
            cache_hit = tracing::field::Empty,
            exit_code = tracing::field::Empty,
            job_id = tracing::field::Empty,
            worker_id = tracing::field::Empty,
        ),
    )]
    pub async fn execute(
        self: &Arc<Self>,
        action_digest: Digest,
        skip_cache_lookup: bool,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        if !skip_cache_lookup {
            if let Some(cached) = self
                .action_cache
                .get_action_result(&action_digest)
                .await
                .map_err(|e| anyhow!("action cache get: {e}"))?
            {
                tracing::Span::current()
                    .record("cache_hit", true)
                    .record("exit_code", cached.exit_code);
                return Ok(ExecutionOutcome {
                    result: cached,
                    cache_hit: true,
                });
            }
        }

        let action = self
            .fetch_message::<rapi::Action>(&action_digest)
            .await
            .with_context(|| "fetching Action from CAS")?;
        let command_digest_proto = action
            .command_digest
            .as_ref()
            .ok_or_else(|| anyhow!("Action.command_digest missing"))?;
        let command_digest = Digest::new(
            command_digest_proto.hash.clone(),
            command_digest_proto.size_bytes,
        )
        .map_err(|e| anyhow!("invalid command digest: {e}"))?;
        let command = self
            .fetch_message::<rapi::Command>(&command_digest)
            .await
            .with_context(|| "fetching Command from CAS")?;

        // Resolve the action's platform constraints. REAPI v2.2 carries them
        // on `Action.platform`; older clients set the deprecated
        // `Command.platform`, which we still accept as a fallback.
        #[allow(deprecated)]
        let platform = action
            .platform
            .clone()
            .or_else(|| command.platform.clone())
            .unwrap_or_default();

        // Snapshot the registry-eligible worker ids (if a registry is wired
        // in), releasing the registry lock before we touch the connected set —
        // so the two locks are never held at once.
        let eligible_ids: Option<Vec<WorkerId>> = match self.worker_registry.as_ref() {
            Some(registry) => {
                let reg = registry.lock().await;
                let ids = eligible_workers(&reg, Instant::now(), &platform)
                    .map(|(id, _)| id.clone())
                    .collect();
                Some(ids)
            }
            None => None,
        };

        // REAPI `Action.timeout` overrides the scheduler default; treat
        // missing / zero / negative as "use the default".
        let effective_timeout = action
            .timeout
            .as_ref()
            .and_then(|d| {
                let s = d.seconds;
                if s > 0 {
                    Some(Duration::from_secs(s as u64))
                } else {
                    None
                }
            })
            .unwrap_or(self.default_execution_timeout);

        // Pick a worker and claim a job sender + an in-flight slot under one
        // lock on the connected set. Candidates = connected workers, narrowed
        // to the registry-eligible ones when a registry is present.
        let (worker_id, sender) = {
            let mut connected = self.connected.lock().await;
            let candidates: Vec<WorkerId> = match &eligible_ids {
                Some(ids) => ids
                    .iter()
                    .filter(|id| connected.is_connected(id))
                    .cloned()
                    .collect(),
                None => connected.connected_ids().cloned().collect(),
            };
            let Some(chosen) = self.strategy.choose(&candidates, &*connected) else {
                tracing::warn!(
                    action_digest = %action_digest,
                    "no eligible connected worker for action platform; rejecting"
                );
                return Err(ExecutionError::NoEligibleWorker);
            };
            let Some(sender) = connected.sender(&chosen) else {
                // Disconnected between selection and sender clone (racy); fail
                // closed rather than route into the void.
                return Err(ExecutionError::NoEligibleWorker);
            };
            connected.inc_inflight(&chosen);
            (chosen, sender)
        };

        let job_id = JobId::new(uuid::Uuid::new_v4().to_string())
            .map_err(|e| anyhow!("invalid job id: {e}"))?;
        tracing::Span::current()
            .record("job_id", job_id.as_str())
            .record("worker_id", worker_id.as_str());
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(job_id.clone(), tx);

        let job = bv1::Job {
            job_id: job_id.clone().into_string(),
            action_digest: Some(rapi::Digest {
                hash: action_digest.hash().to_string(),
                size_bytes: action_digest.size_bytes(),
            }),
            action: Some(action),
            command: Some(command),
        };
        if sender.send(job).await.is_err() {
            // The worker's channel closed (it disconnected after selection).
            // Drop the waiter and release the in-flight slot.
            self.waiters.lock().await.remove(&job_id);
            self.connected.lock().await.dec_inflight(&worker_id);
            return Err(anyhow!("worker channel closed before dispatch").into());
        }

        // Await the result, then release the in-flight slot on every exit path.
        // The inner `async` block lets `?` / early `return` short-circuit to
        // `outcome` so the decrement below always runs exactly once.
        let outcome: Result<ExecutionOutcome, ExecutionError> = async {
            let report = match tokio::time::timeout(effective_timeout, rx).await {
                Ok(Ok(report)) => report,
                Ok(Err(_)) => {
                    // Waiter dropped without a value (already removed by `report`).
                    return Err(anyhow!("worker did not report result").into());
                }
                Err(_) => {
                    // Reclaim the waiter slot so the map doesn't accumulate
                    // stalled entries; a late report is then discarded on miss.
                    self.waiters.lock().await.remove(&job_id);
                    tracing::warn!(
                        job_id = job_id.as_str(),
                        timeout_secs = effective_timeout.as_secs_f64(),
                        "scheduler: worker did not report within timeout"
                    );
                    return Err(ExecutionError::Timeout(effective_timeout));
                }
            };
            if !report.error_message.is_empty() {
                return Err(anyhow!("worker error: {}", report.error_message).into());
            }
            let result = report
                .result
                .ok_or_else(|| anyhow!("worker reported no ActionResult"))?;
            if result.exit_code == 0 {
                self.action_cache
                    .update_action_result(&action_digest, result.clone())
                    .await
                    .map_err(|e| anyhow!("action cache update: {e}"))?;
            }
            tracing::Span::current()
                .record("cache_hit", false)
                .record("exit_code", result.exit_code);
            Ok(ExecutionOutcome {
                result,
                cache_hit: false,
            })
        }
        .await;
        self.connected.lock().await.dec_inflight(&worker_id);
        outcome
    }

    /// Worker-side entry: receive a job result and wake the matching waiter.
    #[tracing::instrument(name = "scheduler::report", skip(self, result))]
    pub async fn report(&self, result: bv1::JobResult) -> Result<()> {
        let job_id = JobId::new(result.job_id.clone())
            .map_err(|e| anyhow!("invalid job_id in result: {}", e))?;
        let waiter = self.waiters.lock().await.remove(&job_id);
        if let Some(tx) = waiter {
            // If the receiver dropped (e.g. client cancelled), discard the result.
            let _ = tx.send(result);
        }
        Ok(())
    }

    async fn fetch_message<M: Message + Default>(&self, digest: &Digest) -> Result<M> {
        let mut reads = self
            .cas
            .batch_read_blobs(std::slice::from_ref(digest))
            .await
            .map_err(|e| anyhow!("CAS read: {e}"))?;
        let bytes = reads
            .remove(0)
            .map_err(|e| anyhow!("blob {} not in CAS: {e}", digest))?;
        M::decode(bytes.as_ref()).with_context(|| format!("decoding {} from CAS", digest))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use async_trait::async_trait;
    use brokkr_cas::{ActionCache, Cas, CasError};
    use brokkr_common::Digest;
    use brokkr_proto::reapi_v2::ActionResult;
    use bytes::Bytes;

    use super::*;

    /// Mock `Cas` that returns a configurable error for `batch_read_blobs`.
    struct MockCas {
        /// If `true`, `batch_read_blobs` returns `CasError::NotFound` for all digests.
        /// Otherwise it returns `NotFound` for each digest (simulating a true miss).
        force_not_found: bool,
    }

    #[async_trait]
    impl Cas for MockCas {
        async fn find_missing_blobs(&self, _digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
            Ok(vec![])
        }

        async fn batch_read_blobs(
            &self,
            digests: &[Digest],
        ) -> Result<Vec<Result<Bytes, CasError>>, CasError> {
            if self.force_not_found {
                // Return a real NotFound error for each digest.
                Ok(digests
                    .iter()
                    .map(|d| Err(CasError::NotFound(d.clone())))
                    .collect())
            } else {
                // Simulate a real miss — read a blob that was never written.
                Ok(digests
                    .iter()
                    .map(|_| Err(CasError::NotFound(Digest::of(b"missing"))))
                    .collect())
            }
        }

        async fn batch_update_blobs(
            &self,
            _blobs: Vec<(Digest, Bytes)>,
        ) -> Result<Vec<brokkr_cas::traits::UpdateResult>, CasError> {
            Ok(vec![])
        }
    }

    /// Mock `ActionCache` that returns a configurable error for `get_action_result`.
    struct MockActionCache {
        /// If `true`, `get_action_result` returns an `Io` error; otherwise returns `Ok(None)`.
        force_error: bool,
    }

    #[async_trait]
    impl ActionCache for MockActionCache {
        async fn get_action_result(
            &self,
            _action_digest: &Digest,
        ) -> Result<Option<ActionResult>, CasError> {
            if self.force_error {
                Err(CasError::Io(std::io::Error::other("simulated")))
            } else {
                Ok(None)
            }
        }

        async fn update_action_result(
            &self,
            _action_digest: &Digest,
            _result: ActionResult,
        ) -> Result<(), CasError> {
            Ok(())
        }
    }

    /// Verify `report` returns an error when given an empty job_id string.
    #[tokio::test]
    async fn report_rejects_invalid_job_id() {
        let cas = Arc::new(MockCas {
            force_not_found: true,
        });
        let ac = Arc::new(MockActionCache { force_error: false });
        let scheduler = Scheduler::new(cas, ac);

        let result = bv1::JobResult {
            job_id: String::new(), // empty string is invalid for JobId
            result: None,
            cache_hit: false,
            error_message: String::new(),
        };
        let err = scheduler.report(result).await.unwrap_err();
        assert!(
            err.to_string().contains("invalid job_id"),
            "expected 'invalid job_id' in error, got: {err}"
        );
    }

    /// Verify `execute` propagates CAS `NotFound` errors from `fetch_message`
    /// when the action digest is not in the CAS.
    #[tokio::test]
    async fn execute_returns_err_when_action_not_in_cas() {
        let missing_digest = Digest::of(b"action never stored");
        let cas = Arc::new(MockCas {
            force_not_found: true,
        });
        let ac = Arc::new(MockActionCache { force_error: false });
        let scheduler = Scheduler::new(cas, ac);

        let err = scheduler.execute(missing_digest, false).await.unwrap_err();
        // The NotFound error is wrapped by with_context("fetching Action from CAS").
        assert!(
            err.to_string().contains("fetching Action from CAS"),
            "expected 'fetching Action from CAS' in error, got: {err}"
        );
    }

    /// Issue #63 regression: `execute` must not hang forever when no worker
    /// consumes the dispatched job. The scheduler is built with a small
    /// timeout; we feed it a valid Action+Command pair via a real in-memory
    /// CAS, never consume the job queue, and assert that `execute` returns
    /// `ExecutionError::Timeout` within roughly the configured budget.
    #[tokio::test]
    async fn execute_returns_timeout_when_worker_never_reports() {
        use std::time::Instant;

        use brokkr_cas::{Cas as _, InMemoryCas};

        let cas = Arc::new(InMemoryCas::new());
        let ac = Arc::new(MockActionCache { force_error: false });

        // Stage a minimal Action+Command pair so the scheduler gets past
        // the CAS fetches and enqueues a real job.
        let command = rapi::Command {
            arguments: vec!["/bin/echo".to_string(), "hi".to_string()],
            ..Default::default()
        };
        let command_bytes = command.encode_to_vec();
        let command_digest = Digest::of(&command_bytes);
        let action = rapi::Action {
            command_digest: Some(rapi::Digest {
                hash: command_digest.hash().to_string(),
                size_bytes: command_digest.size_bytes(),
            }),
            ..Default::default()
        };
        let action_bytes = action.encode_to_vec();
        let action_digest = Digest::of(&action_bytes);
        cas.batch_update_blobs(vec![
            (action_digest.clone(), Bytes::from(action_bytes)),
            (command_digest, Bytes::from(command_bytes)),
        ])
        .await
        .unwrap();

        let timeout = Duration::from_millis(120);
        let scheduler = Scheduler::with_execution_timeout(cas, ac, timeout);

        // Connect a worker (no registry → any connected worker is a
        // candidate). Hold the receiver so the dispatched job buffers and
        // `execute` awaits a result that never arrives → Timeout.
        let (tx, _held_rx) = tokio::sync::mpsc::channel::<bv1::Job>(8);
        scheduler
            .connected_workers()
            .lock()
            .await
            .connect(WorkerId::new("w1".to_string()).unwrap(), tx);

        let start = Instant::now();
        let err = scheduler.execute(action_digest, true).await.unwrap_err();
        let elapsed = start.elapsed();

        match err {
            ExecutionError::Timeout(d) => assert_eq!(d, timeout),
            other => panic!("expected Timeout, got {other:?}"),
        }
        assert!(
            elapsed >= timeout && elapsed < timeout * 20,
            "elapsed {elapsed:?} should be around the timeout {timeout:?}"
        );

        // Waiter slot must have been reclaimed so the map doesn't grow.
        assert!(scheduler.waiters.lock().await.is_empty());
    }

    /// Verify `execute` propagates action cache get errors.
    #[tokio::test]
    async fn execute_returns_err_when_action_cache_get_fails() {
        let cas = Arc::new(MockCas {
            force_not_found: true,
        });
        let ac = Arc::new(MockActionCache { force_error: true });
        let scheduler = Scheduler::new(cas, ac);

        let err = scheduler
            .execute(Digest::of(b"any action"), false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("action cache get"),
            "expected 'action cache get' in error, got: {err}"
        );
    }

    // --- Admission control (§16 task 2) ---

    /// Stage an Action+Command pair (with `platform` on the Action) into an
    /// in-memory CAS and return the Action digest.
    async fn stage_action(
        cas: &brokkr_cas::InMemoryCas,
        platform: Option<rapi::Platform>,
    ) -> Digest {
        use brokkr_cas::Cas as _;

        let command = rapi::Command {
            arguments: vec!["/bin/echo".to_string(), "hi".to_string()],
            ..Default::default()
        };
        let command_bytes = command.encode_to_vec();
        let command_digest = Digest::of(&command_bytes);
        let action = rapi::Action {
            command_digest: Some(rapi::Digest {
                hash: command_digest.hash().to_string(),
                size_bytes: command_digest.size_bytes(),
            }),
            platform,
            ..Default::default()
        };
        let action_bytes = action.encode_to_vec();
        let action_digest = Digest::of(&action_bytes);
        cas.batch_update_blobs(vec![
            (action_digest.clone(), Bytes::from(action_bytes)),
            (command_digest, Bytes::from(command_bytes)),
        ])
        .await
        .unwrap();
        action_digest
    }

    fn os_platform(value: &str) -> rapi::Platform {
        rapi::Platform {
            properties: vec![rapi::platform::Property {
                name: "os".to_string(),
                value: value.to_string(),
            }],
        }
    }

    /// Register a worker in `registry` with `labels` and connect it to
    /// `scheduler`'s `ConnectedWorkers`, returning the held job receiver. Keep
    /// the receiver alive so the per-worker channel stays open and dispatched
    /// jobs buffer (instead of the sender erroring on a dropped receiver).
    async fn register_and_connect(
        scheduler: &Arc<Scheduler>,
        registry: &SharedWorkerRegistry,
        id: &str,
        labels: &[(&str, &str)],
    ) -> tokio::sync::mpsc::Receiver<bv1::Job> {
        use crate::registry::WorkerCapabilities;

        let wid = WorkerId::new(id.to_string()).unwrap();
        let caps = WorkerCapabilities {
            hostname: "w".to_string(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        registry
            .lock()
            .await
            .register(wid.clone(), caps, Instant::now());
        let (tx, rx) = tokio::sync::mpsc::channel::<bv1::Job>(8);
        scheduler.connected_workers().lock().await.connect(wid, tx);
        rx
    }

    /// No connected worker → routing rejects before dispatch.
    #[tokio::test]
    async fn execute_rejects_when_no_eligible_worker() {
        use crate::registry::WorkerRegistry;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default())); // empty, none connected
        let scheduler =
            Scheduler::with_registry_and_timeout(cas, ac, Duration::from_millis(200), registry);

        let err = scheduler.execute(action_digest, true).await.unwrap_err();
        assert!(
            matches!(err, ExecutionError::NoEligibleWorker),
            "expected NoEligibleWorker, got {err:?}"
        );
    }

    /// A connected worker whose labels don't satisfy the platform is filtered
    /// out → NoEligibleWorker.
    #[tokio::test]
    async fn execute_rejects_when_worker_does_not_match_platform() {
        use crate::registry::WorkerRegistry;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler = Scheduler::with_registry_and_timeout(
            cas,
            ac,
            Duration::from_millis(200),
            registry.clone(),
        );
        let _held = register_and_connect(&scheduler, &registry, "w1", &[("os", "windows")]).await;

        let err = scheduler.execute(action_digest, true).await.unwrap_err();
        assert!(
            matches!(err, ExecutionError::NoEligibleWorker),
            "expected NoEligibleWorker, got {err:?}"
        );
    }

    /// A connected, matching worker → routing dispatches; with no result coming
    /// back the call times out (proving it got past selection rather than being
    /// rejected).
    #[tokio::test]
    async fn execute_passes_routing_with_matching_worker() {
        use crate::registry::WorkerRegistry;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let timeout = Duration::from_millis(120);
        let scheduler = Scheduler::with_registry_and_timeout(cas, ac, timeout, registry.clone());
        let _held = register_and_connect(&scheduler, &registry, "w1", &[("os", "linux")]).await;

        let err = scheduler.execute(action_digest, true).await.unwrap_err();
        assert!(
            matches!(err, ExecutionError::Timeout(_)),
            "expected Timeout (selection passed), got {err:?}"
        );
    }

    /// Two connected matching workers: `SimpleFifo` spreads — in-flight
    /// tracking sends the second job to the idle worker, so each worker's
    /// channel receives exactly one job.
    #[tokio::test]
    async fn execute_spreads_across_two_idle_workers() {
        use crate::registry::WorkerRegistry;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let timeout = Duration::from_millis(150);
        let scheduler = Scheduler::with_registry_and_timeout(cas, ac, timeout, registry.clone());
        let mut rx_a = register_and_connect(&scheduler, &registry, "w-a", &[("os", "linux")]).await;
        let mut rx_b = register_and_connect(&scheduler, &registry, "w-b", &[("os", "linux")]).await;

        // Two concurrent executes; neither worker reports, so both time out.
        // pick+inc-in-flight is one critical section under the connected lock,
        // so the second execute sees the first worker busy and picks the other.
        let (s1, s2) = (scheduler.clone(), scheduler.clone());
        let (d1, d2) = (action_digest.clone(), action_digest);
        let h1 = tokio::spawn(async move { s1.execute(d1, true).await });
        let h2 = tokio::spawn(async move { s2.execute(d2, true).await });
        let _ = h1.await.unwrap();
        let _ = h2.await.unwrap();

        assert!(
            rx_a.try_recv().is_ok(),
            "worker a should have received exactly one job"
        );
        assert!(
            rx_b.try_recv().is_ok(),
            "worker b should have received exactly one job"
        );
    }
}
