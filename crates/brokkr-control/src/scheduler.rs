//! Job scheduler: bridges the REAPI `Execute` RPC (client-facing) to the
//! internal `brokkr.v1.WorkerService.Stream` (worker-facing).
//!
//! Multi-worker dispatch follows ADR 0008: each connected worker has its own
//! job channel ([`crate::scheduling::ConnectedWorkers`]); `execute` filters to
//! the eligible (matching + connected) workers and a pluggable
//! [`crate::scheduling::Strategy`] picks one. Global queueing, leases, and fair
//! scheduling are Phase 4 task 4.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use brokkr_cas::{ActionCache, Cas};
use brokkr_common::{Digest, JobId, WorkerId};
use brokkr_proto::brokkr_v1 as bv1;
use brokkr_proto::reapi_v2 as rapi;
use prost::Message;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::lease::LeaseTable;
use crate::matching::eligible_workers;
use crate::scheduling::{ConnectedWorkers, SimpleFifo, Strategy};
use crate::worker_service::SharedWorkerRegistry;

/// Maximum dispatch attempts for a job before it is failed, to bound requeue
/// loops when workers repeatedly die mid-job (ADR 0009).
const MAX_ATTEMPTS: u32 = 5;

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

/// A job waiting in (or re-dispatchable from) the scheduler's pending queue.
/// Carries everything needed to (re-)dispatch it and to size its lease.
#[derive(Clone, Debug)]
struct PendingJob {
    job_id: JobId,
    job: bv1::Job,
    platform: rapi::Platform,
    lease_duration: Duration,
    attempts: u32,
}

/// Dispatch state held under a single mutex, so every routing decision (which
/// workers are connected, what is queued, what is leased) is made atomically —
/// there is no inter-lock ordering to get wrong (ADR 0009).
struct Inner {
    connected: ConnectedWorkers,
    pending: VecDeque<PendingJob>,
    leases: LeaseTable<PendingJob>,
}

/// Multi-worker job scheduler with a global pending queue and job leases
/// (ADR 0008 + ADR 0009).
pub struct Scheduler {
    /// Connected workers + pending queue + active leases, under one lock.
    inner: Mutex<Inner>,
    /// Worker-selection policy (ADR 0008). Defaults to `SimpleFifo`.
    strategy: Arc<dyn Strategy>,
    /// Result waiters, keyed by job id. Survives requeue/reassignment so the
    /// original `execute` caller transparently receives the retried result.
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
        Self::build(
            cas,
            action_cache,
            DEFAULT_EXECUTION_TIMEOUT,
            None,
            Arc::new(SimpleFifo),
        )
    }

    /// Construct a scheduler with a custom default per-action timeout. The
    /// REAPI `Action.timeout` field (when set on a per-action basis) still
    /// overrides this default.
    pub fn with_execution_timeout(
        cas: Arc<dyn Cas>,
        action_cache: Arc<dyn ActionCache>,
        default_execution_timeout: Duration,
    ) -> Arc<Self> {
        Self::build(
            cas,
            action_cache,
            default_execution_timeout,
            None,
            Arc::new(SimpleFifo),
        )
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
            Arc::new(SimpleFifo),
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
            Arc::new(SimpleFifo),
        )
    }

    /// Construct a scheduler with a custom worker-selection [`Strategy`] (and a
    /// registry for constraint filtering), using the default timeout. Lets the
    /// binary pick `SimpleFifo` / `BinPacking` / … at startup.
    pub fn with_strategy(
        cas: Arc<dyn Cas>,
        action_cache: Arc<dyn ActionCache>,
        worker_registry: SharedWorkerRegistry,
        strategy: Arc<dyn Strategy>,
    ) -> Arc<Self> {
        Self::build(
            cas,
            action_cache,
            DEFAULT_EXECUTION_TIMEOUT,
            Some(worker_registry),
            strategy,
        )
    }

    fn build(
        cas: Arc<dyn Cas>,
        action_cache: Arc<dyn ActionCache>,
        default_execution_timeout: Duration,
        worker_registry: Option<SharedWorkerRegistry>,
        strategy: Arc<dyn Strategy>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                connected: ConnectedWorkers::new(),
                pending: VecDeque::new(),
                leases: LeaseTable::new(),
            }),
            strategy,
            waiters: Mutex::new(HashMap::new()),
            cas,
            action_cache,
            default_execution_timeout,
            worker_registry,
        })
    }

    /// Register a connected worker's job channel (called by
    /// `WorkerService.Stream` once the worker's `Hello` arrives), then attempt
    /// a dispatch in case queued work can now be placed.
    pub async fn connect_worker(
        self: &Arc<Self>,
        worker_id: WorkerId,
        job_tx: mpsc::Sender<bv1::Job>,
    ) {
        {
            let mut inner = self.inner.lock().await;
            inner.connected.connect(worker_id, job_tx);
        }
        self.try_dispatch().await;
    }

    /// Deregister a worker whose stream closed. Any job it currently holds is
    /// requeued for reassignment to another worker (the §16 crash-recovery
    /// path), bounded by [`MAX_ATTEMPTS`]; then a dispatch is attempted.
    pub async fn disconnect_worker(self: &Arc<Self>, worker_id: &WorkerId) {
        let (requeued, give_up) = {
            let mut inner = self.inner.lock().await;
            inner.connected.disconnect(worker_id);
            let held = inner.leases.take_worker(worker_id);
            let mut requeued = 0usize;
            let mut give_up: Vec<JobId> = Vec::new();
            for (job_id, mut pj) in held {
                pj.attempts += 1;
                if pj.attempts >= MAX_ATTEMPTS {
                    give_up.push(job_id);
                } else {
                    inner.pending.push_front(pj);
                    requeued += 1;
                }
            }
            (requeued, give_up)
        };
        for job_id in give_up {
            // Too many reassignments — fail the caller by dropping its waiter
            // (its `rx` then errors → `execute` returns an error).
            self.waiters.lock().await.remove(&job_id);
            tracing::error!(job_id = %job_id, "job exceeded max dispatch attempts; failing");
        }
        if requeued > 0 {
            tracing::info!(
                worker_id = %worker_id,
                requeued,
                "worker disconnected; requeued its in-flight job(s) for reassignment"
            );
        }
        self.try_dispatch().await;
    }

    /// Place as many queued jobs as possible onto idle, eligible, connected
    /// workers, leasing each. The decision is made under the inner lock; the
    /// channel send happens outside it.
    async fn try_dispatch(self: &Arc<Self>) {
        loop {
            let now = Instant::now();
            let placed = {
                // Lock order is registry → inner (registry is only ever locked
                // alone elsewhere, so this can't deadlock).
                let reg_guard = match &self.worker_registry {
                    Some(r) => Some(r.lock().await),
                    None => None,
                };
                let mut inner = self.inner.lock().await;

                // First queued job that has an idle, eligible, connected worker.
                let mut found: Option<(usize, WorkerId)> = None;
                for (idx, pj) in inner.pending.iter().enumerate() {
                    let candidates: Vec<WorkerId> = match &reg_guard {
                        Some(reg) => eligible_workers(reg, now, &pj.platform)
                            .map(|(id, _)| id.clone())
                            .filter(|id| {
                                inner.connected.is_connected(id) && !inner.leases.is_worker_busy(id)
                            })
                            .collect(),
                        None => inner
                            .connected
                            .connected_ids()
                            .filter(|id| !inner.leases.is_worker_busy(id))
                            .cloned()
                            .collect(),
                    };
                    if let Some(w) = self.strategy.choose(&candidates, &inner.connected) {
                        found = Some((idx, w));
                        break;
                    }
                }
                let Some((idx, worker_id)) = found else {
                    return;
                };
                let Some(sender) = inner.connected.sender(&worker_id) else {
                    return;
                };
                let Some(pj) = inner.pending.remove(idx) else {
                    return;
                };
                let deadline = now + pj.lease_duration;
                inner
                    .leases
                    .insert(pj.job_id.clone(), worker_id.clone(), deadline, pj.clone());
                (worker_id, sender, pj)
            };

            let (worker_id, sender, pj) = placed;
            tracing::debug!(job_id = %pj.job_id, worker_id = %worker_id, "dispatched job to worker");
            if sender.send(pj.job.clone()).await.is_err() {
                // The worker's channel closed between selection and send. Drop
                // the lease + the dead worker, then requeue (bounded) so another
                // worker can pick the job up.
                let mut pj = pj;
                pj.attempts += 1;
                let give_up = pj.attempts >= MAX_ATTEMPTS;
                {
                    let mut inner = self.inner.lock().await;
                    inner.leases.complete(&pj.job_id);
                    inner.connected.disconnect(&worker_id);
                    if !give_up {
                        inner.pending.push_front(pj.clone());
                    }
                }
                if give_up {
                    self.waiters.lock().await.remove(&pj.job_id);
                    tracing::error!(job_id = %pj.job_id, "job exceeded max dispatch attempts; failing");
                }
            }
        }
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

        // Fail fast when a registry is wired in and *no healthy worker* matches
        // the platform — an action nothing can run should not queue forever.
        // (A matching-but-busy worker still leads to queueing below.)
        if let Some(registry) = self.worker_registry.as_ref() {
            let has_match = {
                let reg = registry.lock().await;
                // Bind so the borrowing iterator temporary drops before `reg`.
                let any = eligible_workers(&reg, Instant::now(), &platform)
                    .next()
                    .is_some();
                any
            };
            if !has_match {
                tracing::warn!(
                    action_digest = %action_digest,
                    "no eligible worker for action platform; rejecting"
                );
                return Err(ExecutionError::NoEligibleWorker);
            }
        }

        // REAPI `Action.timeout` overrides the scheduler default; treat
        // missing / zero / negative as "use the default". This bounds both the
        // caller's wait and each dispatch attempt's lease.
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

        let job_id = JobId::new(uuid::Uuid::new_v4().to_string())
            .map_err(|e| anyhow!("invalid job id: {e}"))?;
        tracing::Span::current().record("job_id", job_id.as_str());

        // Register the result waiter before queueing so a fast
        // dispatch→report can't race ahead of it.
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
        {
            let mut inner = self.inner.lock().await;
            inner.pending.push_back(PendingJob {
                job_id: job_id.clone(),
                job,
                platform,
                lease_duration: effective_timeout,
                attempts: 0,
            });
        }
        self.try_dispatch().await;

        // Await the final result under the overall timeout. Retries
        // (reassignment after a worker dies) keep the same waiter, so this wait
        // spans all attempts. On timeout, drop the job from the queue / lease
        // table and reclaim the waiter.
        let outcome: Result<ExecutionOutcome, ExecutionError> = async {
            let report = match tokio::time::timeout(effective_timeout, rx).await {
                Ok(Ok(report)) => report,
                Ok(Err(_)) => {
                    // Waiter dropped without a value (e.g. exceeded max attempts).
                    return Err(anyhow!("job failed before producing a result").into());
                }
                Err(_) => {
                    self.waiters.lock().await.remove(&job_id);
                    let mut inner = self.inner.lock().await;
                    inner.pending.retain(|p| p.job_id != job_id);
                    inner.leases.complete(&job_id);
                    tracing::warn!(
                        job_id = job_id.as_str(),
                        timeout_secs = effective_timeout.as_secs_f64(),
                        "scheduler: job did not complete within timeout"
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
        outcome
    }

    /// Worker-side entry: a worker reported a job result. Completes the lease
    /// (freeing the worker), wakes the result waiter, and triggers a dispatch
    /// so the now-idle worker can pick up queued work. A result for a job with
    /// no active lease (a late/duplicate report after reassignment or expiry) is
    /// discarded — the at-least-once seam from ADR 0009.
    #[tracing::instrument(name = "scheduler::report", skip(self, result))]
    pub async fn report(self: &Arc<Self>, result: bv1::JobResult) -> Result<()> {
        let job_id = JobId::new(result.job_id.clone())
            .map_err(|e| anyhow!("invalid job_id in result: {}", e))?;
        let known = {
            let mut inner = self.inner.lock().await;
            inner.leases.complete(&job_id).is_some()
        };
        if !known {
            tracing::debug!(
                job_id = job_id.as_str(),
                "discarding late/duplicate worker result (no active lease)"
            );
            return Ok(());
        }
        if let Some(tx) = self.waiters.lock().await.remove(&job_id) {
            // If the receiver dropped (client cancelled), discard the result.
            let _ = tx.send(result);
        }
        // The reporting worker is now idle — try to place more queued work.
        self.try_dispatch().await;
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
            .connect_worker(WorkerId::new("w1".to_string()).unwrap(), tx)
            .await;

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
        scheduler.connect_worker(wid, tx).await;
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
        // Capacity-1 leases mean once the first job leases a worker that worker
        // is busy (excluded from candidates), so the second job lands on the
        // other worker.
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

    /// The scheduler honours its injected strategy (`with_strategy` wiring).
    /// NOTE: under ADR 0009 capacity-1 leases a worker can hold only one job at
    /// a time, so `BinPacking` cannot pack a second job onto a busy worker — it
    /// spreads exactly like `SimpleFifo` here. Packing only differs once a
    /// worker may hold >1 lease (a future per-worker-capacity knob). This test
    /// pins the current (spread) behaviour so that change is a deliberate one.
    #[tokio::test]
    async fn binpacking_with_capacity_one_spreads_like_simplefifo() {
        use crate::registry::WorkerRegistry;
        use crate::scheduling::BinPacking;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler =
            Scheduler::with_strategy(cas, ac, registry.clone(), Arc::new(BinPacking::new(2)));
        let mut rx_a = register_and_connect(&scheduler, &registry, "w-a", &[("os", "linux")]).await;
        let mut rx_b = register_and_connect(&scheduler, &registry, "w-b", &[("os", "linux")]).await;

        let (s1, s2) = (scheduler.clone(), scheduler.clone());
        let (d1, d2) = (action_digest.clone(), action_digest);
        let h1 = tokio::spawn(async move { s1.execute(d1, true).await });
        let h2 = tokio::spawn(async move { s2.execute(d2, true).await });

        // Each worker receives exactly one job (capacity-1 forces the spread).
        let got_a = recv_within(&mut rx_a, Duration::from_millis(500)).await;
        let got_b = recv_within(&mut rx_b, Duration::from_millis(500)).await;
        assert!(got_a.is_some(), "w-a should have received a job");
        assert!(got_b.is_some(), "w-b should have received a job");

        h1.abort();
        h2.abort();
    }

    /// Receive one job from `rx` within `budget`, polling cooperatively. Returns
    /// `None` if nothing arrives in time.
    async fn recv_within(
        rx: &mut tokio::sync::mpsc::Receiver<bv1::Job>,
        budget: Duration,
    ) -> Option<bv1::Job> {
        for _ in 0..((budget.as_millis() / 10).max(1)) {
            if let Ok(job) = rx.try_recv() {
                return Some(job);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        rx.try_recv().ok()
    }

    /// §16 DoD: a worker that dies mid-job → the job is reassigned to another
    /// worker and completes. Submit a job onto worker A, disconnect A
    /// (simulated crash), confirm the job is re-dispatched to B, then have B
    /// report success — `execute` returns `Ok`.
    #[tokio::test]
    async fn disconnect_reassigns_in_flight_job_to_another_worker() {
        use crate::registry::WorkerRegistry;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        // Generous timeout so the reassignment happens well within the budget.
        let scheduler = Scheduler::with_registry_and_timeout(
            cas,
            ac,
            Duration::from_secs(10),
            registry.clone(),
        );
        let mut rx_a = register_and_connect(&scheduler, &registry, "w-a", &[("os", "linux")]).await;
        let mut rx_b = register_and_connect(&scheduler, &registry, "w-b", &[("os", "linux")]).await;

        // Caller submits the action.
        let exec = {
            let s = scheduler.clone();
            tokio::spawn(async move { s.execute(action_digest, true).await })
        };

        // It dispatches to one of the workers. Whichever got it "crashes"; the
        // job must reappear on the other worker.
        let (dead, mut live_rx, first_job) = {
            if let Some(job) = recv_within(&mut rx_a, Duration::from_millis(500)).await {
                (WorkerId::new("w-a".to_string()).unwrap(), rx_b, job)
            } else {
                let job = recv_within(&mut rx_b, Duration::from_millis(500))
                    .await
                    .unwrap();
                (WorkerId::new("w-b".to_string()).unwrap(), rx_a, job)
            }
        };
        let job_id = first_job.job_id.clone();

        // Simulate the crash: the worker's stream closes.
        scheduler.disconnect_worker(&dead).await;

        // The job is reassigned to the surviving worker (same job id).
        let reassigned = recv_within(&mut live_rx, Duration::from_millis(1000))
            .await
            .unwrap();
        assert_eq!(reassigned.job_id, job_id, "same job reassigned");

        // The surviving worker reports success.
        scheduler
            .report(bv1::JobResult {
                job_id: job_id.clone(),
                result: Some(rapi::ActionResult {
                    exit_code: 0,
                    ..Default::default()
                }),
                cache_hit: false,
                error_message: String::new(),
            })
            .await
            .unwrap();

        let outcome = exec.await.unwrap().unwrap();
        assert_eq!(outcome.result.exit_code, 0);
        assert!(!outcome.cache_hit);
    }
}
