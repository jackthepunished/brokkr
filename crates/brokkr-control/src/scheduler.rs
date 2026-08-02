//! Job scheduler: bridges the REAPI `Execute` RPC (client-facing) to the
//! internal `brokkr.v1.WorkerService.Stream` (worker-facing).
//!
//! Multi-worker dispatch follows ADR 0008: each connected worker has its own
//! job channel ([`crate::scheduling::ConnectedWorkers`]); `execute` filters to
//! the eligible (matching + connected) workers and a pluggable
//! [`crate::scheduling::Strategy`] picks one. Global queueing, leases, and fair
//! scheduling are Phase 4 task 4.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use brokkr_cas::{ActionCache, Cas};
use brokkr_common::{Digest, JobId, TenantId, WorkerId};
use brokkr_proto::brokkr_v1 as bv1;
use brokkr_proto::reapi_v2 as rapi;
use prost::Message;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::Instrument;

use crate::fairqueue::FairQueue;
use crate::lease::LeaseTable;
use crate::locality::LocalityIndex;
use crate::matching::eligible_workers;
use crate::scheduling::{ConnectedWorkers, DecisionContext, JobFacts, SimpleFifo, Strategy};
use crate::worker_service::SharedWorkerRegistry;

/// Maximum dispatch attempts for a job before it is failed, to bound requeue
/// loops when workers repeatedly die mid-job (ADR 0009).
const MAX_ATTEMPTS: u32 = 5;

/// Default ceiling on how long [`Scheduler::execute`] waits for a worker to
/// report a result. Issue #63 — without this the oneshot wait was unbounded
/// and a stalled / crashed worker hung the gRPC caller forever. REAPI's
/// `Action.timeout`, when set, overrides this on a per-action basis.
pub const DEFAULT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Default lease duration: how long a dispatched job may run before its lease
/// expires and it is reassigned (ADR 0009). It is capped by the action timeout
/// and is shorter than the overall execute wait, so a hung-but-connected worker
/// is retried elsewhere before the caller's deadline.
pub const DEFAULT_LEASE_DURATION: Duration = Duration::from_secs(60);

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
    /// The tenant is already at its max-concurrent-jobs quota. Translates to
    /// gRPC `RESOURCE_EXHAUSTED` (code 8) so the client backs off / retries.
    #[error("tenant has reached its concurrent-jobs quota ({0})")]
    QuotaExceeded(usize),
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
    /// Whether this result is now *in* the action cache.
    ///
    /// `false` after a successful execution means the entry could not be
    /// stored because this node is not the metadata leader (I9b / D1): the
    /// build is correct and complete, but an identical action will re-execute
    /// instead of hitting the cache. The service layer passes this to the
    /// client so "ran and cached" and "ran, not cached" are distinguishable —
    /// best-effort must not mean unobservable.
    ///
    /// Always `true` for a cache hit (it is in the cache by definition) and
    /// for a non-zero exit code (failures are deliberately not cached, which
    /// is not a degradation).
    pub result_cached: bool,
}

/// A job waiting in (or re-dispatchable from) the scheduler's pending queue.
/// Carries everything needed to (re-)dispatch it and to size its lease.
#[derive(Clone, Debug)]
struct PendingJob {
    job_id: JobId,
    tenant: TenantId,
    job: bv1::Job,
    platform: rapi::Platform,
    lease_duration: Duration,
    attempts: u32,
    /// Digest of the action being run. Also reachable via
    /// `job.action_digest`, but kept here as the typed newtype so the dispatch
    /// path can build a [`JobFacts`] without re-validating a proto message on
    /// every placement.
    action_digest: Digest,
    /// Digest of the action's input root, when it has one. `None` for an
    /// action with no inputs, or when the proto digest failed validation —
    /// locality is a hint, so an unparseable input root degrades to "no
    /// locality signal" rather than failing the dispatch.
    input_root_digest: Option<Digest>,
}

/// Dispatch state held under a single mutex, so every routing decision (which
/// workers are connected, what is queued, what is leased) is made atomically —
/// there is no inter-lock ordering to get wrong (ADR 0009). The pending queue
/// is a per-tenant fair queue (ADR 0010).
struct Inner {
    connected: ConnectedWorkers,
    pending: FairQueue<PendingJob>,
    leases: LeaseTable<PendingJob>,
    /// Bounded per-worker completion history, the source of the locality
    /// signal a `Strategy` may read (ADR 0014). Lives here rather than beside
    /// `ConnectedWorkers` because it deliberately outlives a worker's
    /// connection: a reconnecting worker still has its inputs on disk.
    locality: LocalityIndex,
    /// Per-tenant count of in-flight jobs (queued + leased), for the
    /// max-concurrent quota (ADR 0010). A job is counted from admission in
    /// `execute` until that call returns (success / timeout / failure).
    tenant_inflight: HashMap<TenantId, usize>,
}

impl Inner {
    /// Current in-flight job count for `tenant`.
    fn tenant_inflight(&self, tenant: &TenantId) -> usize {
        self.tenant_inflight.get(tenant).copied().unwrap_or(0)
    }

    /// Record one more in-flight job for `tenant` (on admission).
    fn inc_tenant(&mut self, tenant: &TenantId) {
        *self.tenant_inflight.entry(tenant.clone()).or_insert(0) += 1;
    }

    /// Record one fewer in-flight job for `tenant` (on completion), removing
    /// the entry at zero so the map doesn't accumulate idle tenants.
    fn dec_tenant(&mut self, tenant: &TenantId) {
        if let Some(n) = self.tenant_inflight.get_mut(tenant) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.tenant_inflight.remove(tenant);
            }
        }
    }
}

impl Inner {
    /// Requeue jobs taken from the lease table (on worker disconnect or lease
    /// expiry) for reassignment, bumping each attempt count. Returns
    /// `(requeued_count, give_up_job_ids)`; the caller fails the give-up jobs'
    /// waiters outside the lock. Jobs go to the front so a reassignment is
    /// retried promptly.
    fn requeue_taken(&mut self, jobs: Vec<(JobId, PendingJob)>) -> (usize, Vec<JobId>) {
        let mut requeued = 0usize;
        let mut give_up: Vec<JobId> = Vec::new();
        for (job_id, mut pj) in jobs {
            pj.attempts += 1;
            if pj.attempts >= MAX_ATTEMPTS {
                give_up.push(job_id);
            } else {
                let tenant = pj.tenant.clone();
                self.pending.push(tenant, pj);
                requeued += 1;
            }
        }
        (requeued, give_up)
    }
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
    /// Per-attempt lease duration (capped by the action timeout). A lease that
    /// expires before the worker reports causes the job to be reassigned.
    lease_duration: Duration,
    /// Optional per-tenant max-concurrent-jobs quota (ADR 0010). `None` =
    /// unlimited. A tenant at its limit gets `QuotaExceeded` at admission.
    max_concurrent_per_tenant: Option<usize>,
    /// Optional worker registry for platform-constraint filtering. When set,
    /// candidates are narrowed to healthy workers whose capabilities satisfy
    /// the action's platform; when `None` (e.g. fixtures that don't exercise
    /// matching), any connected worker is a candidate.
    worker_registry: Option<SharedWorkerRegistry>,
    /// How many completed actions were returned **uncached** because this node
    /// was not the metadata leader (I9b / decision D1).
    ///
    /// The best-effort write is deliberately invisible to the client, which
    /// makes it invisible to the operator too — a cluster whose clients all
    /// land on followers keeps working and simply stops caching. This counter
    /// is how that shows up as a number instead of a mystery, so it is part of
    /// the feature rather than instrumentation bolted on after.
    uncached_results_not_leader: AtomicU64,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
        )
    }

    /// Construct a scheduler with a per-tenant max-concurrent-jobs quota and a
    /// registry (for constraint filtering), using the default timeout +
    /// strategy. `None` means unlimited.
    pub fn with_tenant_quota(
        cas: Arc<dyn Cas>,
        action_cache: Arc<dyn ActionCache>,
        worker_registry: SharedWorkerRegistry,
        max_concurrent_per_tenant: Option<usize>,
    ) -> Arc<Self> {
        Self::build(
            cas,
            action_cache,
            DEFAULT_EXECUTION_TIMEOUT,
            Some(worker_registry),
            Arc::new(SimpleFifo),
            max_concurrent_per_tenant,
        )
    }

    fn build(
        cas: Arc<dyn Cas>,
        action_cache: Arc<dyn ActionCache>,
        default_execution_timeout: Duration,
        worker_registry: Option<SharedWorkerRegistry>,
        strategy: Arc<dyn Strategy>,
        max_concurrent_per_tenant: Option<usize>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                connected: ConnectedWorkers::new(),
                pending: FairQueue::new(),
                leases: LeaseTable::new(),
                tenant_inflight: HashMap::new(),
                locality: LocalityIndex::default(),
            }),
            strategy,
            max_concurrent_per_tenant,
            waiters: Mutex::new(HashMap::new()),
            cas,
            action_cache,
            default_execution_timeout,
            lease_duration: DEFAULT_LEASE_DURATION,
            worker_registry,
            uncached_results_not_leader: AtomicU64::new(0),
        })
    }

    /// How many completed actions this scheduler returned **uncached** because
    /// the node is not the metadata leader (I9b / D1).
    ///
    /// A steadily climbing value means clients are being served by a follower
    /// and every build is paying full execution cost with no cache benefit —
    /// the symptom of a routing problem, not a storage one.
    pub fn uncached_results_not_leader(&self) -> u64 {
        self.uncached_results_not_leader.load(Ordering::Relaxed)
    }

    /// Per-worker in-flight counts for every connected worker (ADR 0012).
    ///
    /// Taken as a snapshot rather than exposing the registry, so the read-model
    /// never holds the dispatch mutex while it projects — `views::worker_views`
    /// takes the counts as a closure for exactly this reason.
    pub async fn inflight_snapshot(&self) -> HashMap<WorkerId, usize> {
        use crate::scheduling::LoadView as _;
        let inner = self.inner.lock().await;
        inner
            .connected
            .connected_ids()
            .map(|id| (id.clone(), inner.connected.inflight(id)))
            .collect()
    }

    /// How many of `worker`'s recent completions used `input_root` (ADR 0014).
    ///
    /// The locality index lives under the dispatch mutex, so this is the only
    /// way to observe it from outside — used by tests and, later, by the
    /// observability read-model. Cheap: one hash lookup plus a scan of a
    /// bounded window.
    pub async fn locality_input_root_hits(&self, worker: &WorkerId, input_root: &Digest) -> u32 {
        use crate::scheduling::LocalityView as _;
        self.inner
            .lock()
            .await
            .locality
            .input_root_hits(worker, input_root)
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
    /// path), bounded by `MAX_ATTEMPTS`; then a dispatch is attempted.
    pub async fn disconnect_worker(self: &Arc<Self>, worker_id: &WorkerId) {
        let (requeued, give_up) = {
            let mut inner = self.inner.lock().await;
            inner.connected.disconnect(worker_id);
            let held = inner.leases.take_worker(worker_id);
            inner.requeue_taken(held)
        };
        self.fail_jobs(give_up).await;
        if requeued > 0 {
            tracing::info!(
                worker_id = %worker_id,
                requeued,
                "worker disconnected; requeued its in-flight job(s) for reassignment"
            );
        }
        self.try_dispatch().await;
    }

    /// Reassign jobs whose lease has expired as of `now` (a worker that is
    /// still connected but went silent), then attempt a dispatch. Split from
    /// [`reap_expired_leases`] so tests can drive expiry with an explicit
    /// instant.
    async fn reap_expired_at(self: &Arc<Self>, now: Instant) {
        let (requeued, give_up) = {
            let mut inner = self.inner.lock().await;
            let expired = inner.leases.take_expired(now);
            inner.requeue_taken(expired)
        };
        self.fail_jobs(give_up).await;
        if requeued > 0 {
            tracing::warn!(requeued, "reassigned job(s) whose lease expired");
            self.try_dispatch().await;
        }
    }

    /// Reassign jobs whose lease has expired as of now. Called on an interval
    /// by [`spawn_lease_reaper`].
    pub async fn reap_expired_leases(self: &Arc<Self>) {
        self.reap_expired_at(Instant::now()).await;
    }

    /// Renew the leases held by `worker_id`, pushing their deadlines out by the
    /// lease window. Called on each heartbeat: a worker that is alive (still
    /// heartbeating) keeps its in-flight job leased rather than having it
    /// expire and be reassigned. A lease therefore expires only when a worker
    /// stops heartbeating (dead / partitioned). Returns the number renewed.
    pub async fn renew_worker_leases(&self, worker_id: &WorkerId) -> usize {
        let new_deadline = Instant::now() + self.lease_duration;
        let mut inner = self.inner.lock().await;
        inner.leases.renew_worker(worker_id, new_deadline)
    }

    /// Fail each job in `job_ids` by dropping its result waiter (its `rx` then
    /// errors → `execute` returns an error). Used when a job exceeds
    /// `MAX_ATTEMPTS` reassignments.
    async fn fail_jobs(&self, job_ids: Vec<JobId>) {
        for job_id in job_ids {
            self.waiters.lock().await.remove(&job_id);
            tracing::error!(job_id = %job_id, "job exceeded max dispatch attempts; failing");
        }
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

                // The fair queue's lowest-start-tag job that has an idle,
                // eligible, connected worker (fair-share dequeue, ADR 0010).
                //
                // Selecting the *slot* must not consult the strategy.
                // `Strategy::choose` is contracted to return `None` iff
                // `candidates` is empty, so "is this slot dispatchable?" is
                // exactly `!candidates.is_empty()` — no policy call needed.
                // Asking the policy per slot made this O(queue²) per drain
                // (the outer `loop` re-runs per placement), which is free for
                // a comparison-based built-in but pathological for a WASM
                // policy called under this very lock (Phase 6, ADR 0014). We
                // therefore pick the winning slot first and call the strategy
                // exactly once, for that slot's candidates.
                let mut best: Option<(usize, Vec<WorkerId>, u64)> = None;
                for slot in inner.pending.slots() {
                    // A slot that cannot beat the incumbent start tag can't
                    // win, so skip building its candidate set at all. `>=`
                    // keeps first-seen-wins on ties, matching the previous
                    // `slot.start < best` comparison.
                    if best
                        .as_ref()
                        .is_some_and(|(_, _, incumbent)| slot.start >= *incumbent)
                    {
                        continue;
                    }
                    let candidates: Vec<WorkerId> = match &reg_guard {
                        Some(reg) => eligible_workers(reg, now, &slot.job.platform)
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
                    if !candidates.is_empty() {
                        best = Some((slot.index, candidates, slot.start));
                    }
                }
                let Some((idx, mut candidates, _)) = best else {
                    return;
                };
                // Sort by worker id, once, for the list that actually reaches
                // the strategy. Both sources iterate a `HashMap`
                // (`WorkerRegistry::workers` and `ConnectedWorkers`), so
                // without this the candidate list arrives in hash order — and
                // that list *is* the index space a WASM policy returns into
                // (ADR 0014). Identical cluster state would then produce
                // different placements run to run, defeating the determinism
                // the policy ABI is built to guarantee. The built-ins are
                // unaffected: they already tie-break on id and their answer
                // never depended on order.
                candidates.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
                // The one strategy call per placement. The context is built
                // from the winning slot only, which is why `FairQueue::get`
                // exists — re-borrowing beats cloning a `PendingJob` (it
                // inlines the whole Action and Command).
                let Some(winner) = inner.pending.get(idx) else {
                    return;
                };
                let facts = JobFacts {
                    tenant: &winner.job.tenant,
                    action_digest: &winner.job.action_digest,
                    input_root_digest: winner.job.input_root_digest.as_ref(),
                    platform: &winner.job.platform,
                };
                let ctx = DecisionContext {
                    loads: &inner.connected,
                    locality: &inner.locality,
                    job: facts,
                };
                let Some(worker_id) = self.strategy.choose_with(&candidates, &ctx) else {
                    // Unreachable for a contract-abiding strategy (`candidates`
                    // is non-empty here). Bail rather than dispatch nowhere.
                    tracing::warn!(
                        candidates = candidates.len(),
                        "strategy returned no worker for a non-empty candidate set"
                    );
                    return;
                };
                let Some(sender) = inner.connected.sender(&worker_id) else {
                    return;
                };
                let Some(pj) = inner.pending.take(idx) else {
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
                        inner.pending.push(pj.tenant.clone(), pj.clone());
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
            tenant = %tenant,
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
        tenant: TenantId,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        if !skip_cache_lookup {
            // Decision D1 applies to the *lookup* as well as the write, and
            // for the same reason. Reads are leader-served (I8c ReadIndex), so
            // on a follower this returns `NotLeader` — but "I cannot check the
            // cache" is a cache **miss**, not a failed build. Failing here
            // would be worse than failing on the write: the action has not even
            // run yet, and the caller asked to execute an action, not to be
            // told which node they happened to reach.
            //
            // Every other error still fails the RPC, exactly as on the write
            // path: a storage fault must not masquerade as a miss and silently
            // re-execute work that was already cached.
            let looked_up = match self.action_cache.get_action_result(&action_digest).await {
                Ok(found) => found,
                Err(brokkr_cas::CasError::NotLeader {
                    leader,
                    leader_addr,
                }) => {
                    tracing::debug!(
                        action_digest = %action_digest,
                        leader = ?leader,
                        leader_addr = ?leader_addr,
                        "cache lookup unavailable on this node (not the metadata leader); \
                         treating as a miss"
                    );
                    None
                }
                Err(e) => return Err(anyhow!("action cache get: {e}").into()),
            };
            if let Some(cached) = looked_up {
                tracing::Span::current()
                    .record("cache_hit", true)
                    .record("exit_code", cached.exit_code);
                return Ok(ExecutionOutcome {
                    result: cached,
                    cache_hit: true,
                    result_cached: true,
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

        // The action's input root, for the locality signal a `Strategy` may
        // read (ADR 0014). A worker that recently ran an action with this same
        // input root probably still has those inputs materialized. This is a
        // hint, not a constraint, so a digest that fails validation degrades to
        // `None` — "no locality signal" — rather than failing the dispatch.
        let input_root = action.input_root_digest.as_ref().and_then(|d| {
            match Digest::new(d.hash.clone(), d.size_bytes) {
                Ok(digest) => Some(digest),
                Err(e) => {
                    // Observable, like every other degrade-to-best-effort path
                    // here (cf. the `NotLeader` cache lookup): a client that
                    // persistently sends a malformed input root silently costs
                    // every locality-aware policy its signal, and a log line is
                    // the only way that surfaces.
                    tracing::debug!(
                        action_digest = %action_digest,
                        error = %e,
                        "invalid input-root digest; no locality signal for this action"
                    );
                    None
                }
            }
        });

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
        let pj = PendingJob {
            job_id: job_id.clone(),
            tenant: tenant.clone(),
            job,
            platform,
            // A single attempt may run up to the action timeout, but no longer
            // than the lease window so a hung worker is retried.
            lease_duration: effective_timeout.min(self.lease_duration),
            attempts: 0,
            action_digest: action_digest.clone(),
            input_root_digest: input_root,
        };
        // Admission: enqueue + count one in-flight for the tenant, unless it is
        // already at its max-concurrent quota (checked and incremented under the
        // same lock so concurrent submits can't both slip past).
        let over_quota = {
            let mut inner = self.inner.lock().await;
            match self.max_concurrent_per_tenant {
                Some(limit) if inner.tenant_inflight(&tenant) >= limit => Some(limit),
                _ => {
                    inner.inc_tenant(&tenant);
                    inner.pending.push(tenant.clone(), pj);
                    None
                }
            }
        };
        if let Some(limit) = over_quota {
            self.waiters.lock().await.remove(&job_id);
            tracing::warn!(tenant = %tenant, limit, "tenant at max-concurrent quota; rejecting");
            return Err(ExecutionError::QuotaExceeded(limit));
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
            let mut result_cached = true;
            if result.exit_code == 0 {
                // Hold the GC coordination barrier (issue #144)
                // across the AC write so an in-process `gc::sweep`
                // cannot delete the blobs this result references
                // after we commit. The guard is dropped when this
                // scope ends; coverage is sufficient because the
                // worker uploaded the CAS blobs over its own
                // gRPC stream before reporting success, and the
                // barrier closes the in-process window between
                // `cas.list_digests()` and `cas.delete_blob(d)`
                // that previously raced with that report.
                let _gc_guard = self
                    .action_cache
                    .gc_window()
                    .await
                    .map_err(|e| anyhow!("gc_window: {e}"))?;
                match self
                    .action_cache
                    .update_action_result(&action_digest, result.clone())
                    .await
                {
                    Ok(()) => {}
                    // Decision D1 (owner, 2026-07-30): the write is
                    // best-effort **on `NotLeader` only**. The action
                    // already ran and the result is correct; failing the
                    // RPC now would throw away completed work and charge
                    // the client for a routing accident. So the build
                    // succeeds uncached.
                    //
                    // No retry: this node is not the leader, and the only
                    // store reachable from here is the same one that just
                    // refused. Forwarding to the leader is a different
                    // design (§VII.2 option (b)), not a retry.
                    Err(brokkr_cas::CasError::NotLeader {
                        leader,
                        leader_addr,
                    }) => {
                        result_cached = false;
                        self.uncached_results_not_leader
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            action_digest = %action_digest,
                            leader = ?leader,
                            leader_addr = ?leader_addr,
                            uncached_total = self
                                .uncached_results_not_leader
                                .load(Ordering::Relaxed),
                            "action ran successfully but was NOT cached: this node is \
                             not the metadata leader; an identical action will \
                             re-execute. Route builds at the leader to restore cache \
                             hits."
                        );
                    }
                    // Everything else still fails the RPC. A storage error
                    // or a throughput limit is a real fault, and letting
                    // the best-effort path swallow it would turn data loss
                    // into a silent cache-miss regression.
                    Err(e) => return Err(anyhow!("action cache update: {e}").into()),
                }
            }
            tracing::Span::current()
                .record("cache_hit", false)
                .record("exit_code", result.exit_code);
            Ok(ExecutionOutcome {
                result,
                cache_hit: false,
                result_cached,
            })
        }
        .await;
        // The job is terminal (completed / timed out / failed) — release the
        // tenant's in-flight slot so it counts against the quota no longer.
        self.inner.lock().await.dec_tenant(&tenant);
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
            // Read the holder before completing, because `complete` removes
            // the lease and with it the worker association.
            let worker = inner.leases.worker_of(&job_id).cloned();
            let completed = inner.leases.complete(&job_id);
            // Record locality only for a lease the worker actually reported.
            // A lease that *expired* is deliberately not recorded: the worker
            // never reported, so it is most likely dead or partitioned and its
            // cache state is unknown.
            if let (Some(w), Some(pj)) = (worker, completed.as_ref()) {
                inner
                    .locality
                    .record(&w, &pj.action_digest, pj.input_root_digest.as_ref());
            }
            completed.is_some()
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

    /// The lease window the scheduler applies to dispatched jobs. Used by
    /// [`spawn_lease_reaper`] to pick a sensible reap interval.
    pub fn lease_duration(&self) -> Duration {
        self.lease_duration
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

/// Spawn a background task that reaps expired job leases every `interval`,
/// reassigning their jobs to other workers (ADR 0009). Mirrors
/// [`crate::worker_service::spawn_eviction_task`]; wire it into the
/// control-plane binary and hold the handle for the server's lifetime.
pub fn spawn_lease_reaper(
    scheduler: Arc<Scheduler>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(
        async move {
            // `tokio::time::interval` panics on a zero period; a zero interval
            // disables the reaper instead of crashing the server.
            if interval == Duration::ZERO {
                tracing::warn!("lease reaper disabled (interval is zero)");
                return;
            }
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // drop the immediate first tick
            loop {
                ticker.tick().await;
                scheduler.reap_expired_leases().await;
            }
        }
        .in_current_span(),
    )
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

    /// `ActionCache` whose *write* fails with a configurable error, so the D1
    /// best-effort path (I9b) can be exercised without a Raft cluster: reads
    /// miss, writes fail the way a follower's would.
    struct WriteFailsActionCache {
        error: fn() -> CasError,
    }

    /// A store where **both** directions fail the way a follower's would:
    /// reads are leader-served (I8c ReadIndex), so a follower cannot answer a
    /// cache lookup either.
    struct FollowerActionCache;

    #[async_trait]
    impl ActionCache for FollowerActionCache {
        async fn get_action_result(
            &self,
            _action_digest: &Digest,
        ) -> Result<Option<ActionResult>, CasError> {
            Err(CasError::NotLeader {
                leader: Some("control-1".to_string()),
                leader_addr: Some("10.0.0.1:7878".to_string()),
            })
        }

        async fn update_action_result(
            &self,
            _action_digest: &Digest,
            _result: ActionResult,
        ) -> Result<(), CasError> {
            Err(CasError::NotLeader {
                leader: Some("control-1".to_string()),
                leader_addr: Some("10.0.0.1:7878".to_string()),
            })
        }
    }

    #[async_trait]
    impl ActionCache for WriteFailsActionCache {
        async fn get_action_result(
            &self,
            _action_digest: &Digest,
        ) -> Result<Option<ActionResult>, CasError> {
            Ok(None)
        }

        async fn update_action_result(
            &self,
            _action_digest: &Digest,
            _result: ActionResult,
        ) -> Result<(), CasError> {
            Err((self.error)())
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

        let err = scheduler
            .execute(missing_digest, false, TenantId::default())
            .await
            .unwrap_err();
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
        let err = scheduler
            .execute(action_digest, true, TenantId::default())
            .await
            .unwrap_err();
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
            .execute(Digest::of(b"any action"), false, TenantId::default())
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

        let err = scheduler
            .execute(action_digest, true, TenantId::default())
            .await
            .unwrap_err();
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

        let err = scheduler
            .execute(action_digest, true, TenantId::default())
            .await
            .unwrap_err();
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

        let err = scheduler
            .execute(action_digest, true, TenantId::default())
            .await
            .unwrap_err();
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
        let h1 = tokio::spawn(async move { s1.execute(d1, true, TenantId::default()).await });
        let h2 = tokio::spawn(async move { s2.execute(d2, true, TenantId::default()).await });
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
        let h1 = tokio::spawn(async move { s1.execute(d1, true, TenantId::default()).await });
        let h2 = tokio::spawn(async move { s2.execute(d2, true, TenantId::default()).await });

        // Each worker receives exactly one job (capacity-1 forces the spread).
        let got_a = recv_within(&mut rx_a, Duration::from_millis(500)).await;
        let got_b = recv_within(&mut rx_b, Duration::from_millis(500)).await;
        assert!(got_a.is_some(), "w-a should have received a job");
        assert!(got_b.is_some(), "w-b should have received a job");

        h1.abort();
        h2.abort();
    }

    /// A `Strategy` that delegates to `SimpleFifo` but counts how many times
    /// it was asked. Used to pin the *complexity* of `try_dispatch`, which is
    /// otherwise invisible: a policy call per pending slot per placement is
    /// behaviourally identical to one call per placement, and only shows up as
    /// latency once the policy is expensive (Phase 6's WASM hook, ADR 0014).
    #[derive(Default)]
    struct CountingStrategy {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl crate::scheduling::Strategy for CountingStrategy {
        fn choose(
            &self,
            candidates: &[WorkerId],
            loads: &dyn crate::scheduling::LoadView,
        ) -> Option<WorkerId> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            crate::scheduling::SimpleFifo.choose(candidates, loads)
        }
    }

    /// The strategy is consulted once per *placement*, and **not at all** for
    /// slots that have no candidate.
    ///
    /// Before this was fixed, `try_dispatch` called `choose` for every slot in
    /// the queue on every pass, purely to decide whether that slot was
    /// dispatchable — but `choose` is contracted to return `None` iff
    /// `candidates` is empty, so emptiness already answers that question. With
    /// Q queued jobs draining one at a time that was O(Q²) calls, all under the
    /// dispatch mutex.
    ///
    /// The shape here makes the difference deterministic rather than
    /// timing-dependent. One worker and three jobs: ADR 0009's capacity-1
    /// leases mean job 1 is placed and jobs 2 and 3 *must* queue behind it —
    /// there is no interleaving in which they don't. Every `try_dispatch` pass
    /// while the worker is busy therefore walks two slots whose candidate sets
    /// are empty:
    ///
    /// - now: exactly **1** call — the single placement. Empty-candidate slots
    ///   cost zero policy calls no matter how many passes run.
    /// - before: 1 call for the placement plus one per queued slot per pass,
    ///   i.e. **≥ 3** and rising with the number of passes.
    #[tokio::test]
    async fn strategy_is_consulted_once_per_placement_and_never_for_empty_slots() {
        use crate::registry::WorkerRegistry;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let counting = Arc::new(CountingStrategy::default());
        let scheduler =
            Scheduler::with_strategy(cas, ac, registry.clone(), counting.clone() as Arc<_>);
        let mut rx = register_and_connect(&scheduler, &registry, "w-a", &[("os", "linux")]).await;

        let mut handles = Vec::new();
        for _ in 0..3 {
            let (s, d) = (scheduler.clone(), action_digest.clone());
            handles.push(tokio::spawn(async move {
                s.execute(d, true, TenantId::default()).await
            }));
        }

        // Exactly one job reaches the worker...
        assert!(
            recv_within(&mut rx, Duration::from_millis(500))
                .await
                .is_some(),
            "the single worker should have received the first job"
        );
        // ...and the other two stay queued, because a capacity-1 lease blocks
        // them. This wait is also what guarantees they have been enqueued and
        // that further dispatch passes have walked over them.
        assert!(
            recv_within(&mut rx, Duration::from_millis(300))
                .await
                .is_none(),
            "the busy worker must not receive a second job (capacity-1 lease)"
        );
        for h in handles {
            h.abort();
        }

        let calls = counting.calls.load(Ordering::Relaxed);
        assert_eq!(
            calls, 1,
            "expected exactly one strategy call for the one placement; got {calls}. \
             More than one means try_dispatch is asking the policy which slots \
             are dispatchable instead of checking whether the candidate set is empty."
        );
    }

    /// An action whose `input_root_digest` is malformed must still dispatch.
    ///
    /// The input root feeds the locality signal a `Strategy` may read (ADR
    /// 0014); it is a hint, not a constraint. A client that sends a digest
    /// failing validation should lose *the hint*, not the build — so this
    /// asserts the job reaches a worker and completes normally.
    #[tokio::test]
    async fn an_invalid_input_root_digest_costs_the_hint_not_the_dispatch() {
        use crate::registry::WorkerRegistry;
        use brokkr_cas::Cas as _;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
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
            platform: Some(os_platform("linux")),
            // Not a valid digest: the hash is not 64 hex characters. Asserted
            // below, so this test cannot start passing for the wrong reason if
            // `Digest::new` ever loosens.
            input_root_digest: Some(rapi::Digest {
                hash: "not-a-digest".to_string(),
                size_bytes: -1,
            }),
            ..Default::default()
        };
        assert!(
            Digest::new("not-a-digest".to_string(), -1).is_err(),
            "the fixture must actually be rejected, or this test proves nothing"
        );

        let action_bytes = action.encode_to_vec();
        let action_digest = Digest::of(&action_bytes);
        cas.batch_update_blobs(vec![
            (action_digest.clone(), Bytes::from(action_bytes)),
            (command_digest, Bytes::from(command_bytes)),
        ])
        .await
        .unwrap();

        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler = Scheduler::with_registry_and_timeout(
            cas,
            ac,
            Duration::from_secs(10),
            registry.clone(),
        );
        let mut rx = register_and_connect(&scheduler, &registry, "w-a", &[("os", "linux")]).await;

        let exec: tokio::task::JoinHandle<Result<ExecutionOutcome, ExecutionError>> = {
            let s = scheduler.clone();
            tokio::spawn(async move { s.execute(action_digest, true, TenantId::default()).await })
        };
        // A malformed input root must not block dispatch.
        let job = recv_within(&mut rx, Duration::from_millis(500))
            .await
            .unwrap();
        scheduler
            .report(bv1::JobResult {
                job_id: job.job_id.clone(),
                result: Some(rapi::ActionResult {
                    exit_code: 0,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        let outcome = exec.await.unwrap().unwrap();
        assert_eq!(outcome.result.exit_code, 0);
    }

    /// A worker's own report populates the locality index; a lease that merely
    /// *expires* does not.
    ///
    /// That asymmetry is the whole point of recording on `report` rather than
    /// on `complete`: an expired lease means the worker never answered, so it
    /// is most likely dead or partitioned and its cache state is unknown.
    /// Recording it would teach the scheduler to prefer a worker that may have
    /// nothing.
    #[tokio::test]
    async fn locality_is_recorded_on_report_and_not_on_lease_expiry() {
        use crate::registry::WorkerRegistry;
        use brokkr_cas::Cas as _;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        // An action with a real input root, so there is something to record.
        let input_root = Digest::of(b"the-input-root");
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
            platform: Some(os_platform("linux")),
            input_root_digest: Some(rapi::Digest {
                hash: input_root.hash().to_string(),
                size_bytes: input_root.size_bytes(),
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

        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler = Scheduler::with_registry_and_timeout(
            cas,
            ac,
            Duration::from_secs(10),
            registry.clone(),
        );
        let wid_a = WorkerId::new("w-a".to_string()).unwrap();
        let mut rx = register_and_connect(&scheduler, &registry, "w-a", &[("os", "linux")]).await;

        assert_eq!(
            scheduler
                .locality_input_root_hits(&wid_a, &input_root)
                .await,
            0,
            "nothing recorded before anything completes"
        );

        // Run one job to completion via a real report.
        let exec: tokio::task::JoinHandle<Result<ExecutionOutcome, ExecutionError>> = {
            let (s, d) = (scheduler.clone(), action_digest.clone());
            tokio::spawn(async move { s.execute(d, true, TenantId::default()).await })
        };
        let job = recv_within(&mut rx, Duration::from_millis(500))
            .await
            .unwrap();
        scheduler
            .report(bv1::JobResult {
                job_id: job.job_id.clone(),
                result: Some(rapi::ActionResult {
                    exit_code: 0,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        exec.await.unwrap().unwrap();

        assert_eq!(
            scheduler
                .locality_input_root_hits(&wid_a, &input_root)
                .await,
            1,
            "a reported completion must warm the worker's locality history"
        );

        // Now dispatch a second job and let its lease expire instead.
        let exec2: tokio::task::JoinHandle<Result<ExecutionOutcome, ExecutionError>> = {
            let (s, d) = (scheduler.clone(), action_digest.clone());
            tokio::spawn(async move { s.execute(d, true, TenantId::default()).await })
        };
        assert!(
            recv_within(&mut rx, Duration::from_millis(500))
                .await
                .is_some(),
            "the second job should have been dispatched"
        );
        // Reap with a deadline far in the future: the lease expires unreported.
        scheduler
            .reap_expired_at(Instant::now() + Duration::from_secs(3600))
            .await;
        exec2.abort();

        assert_eq!(
            scheduler
                .locality_input_root_hits(&wid_a, &input_root)
                .await,
            1,
            "an expired lease must NOT be recorded — the worker never reported, \
             so its cache state is unknown"
        );
    }

    /// Locality survives a disconnect, because a reconnecting worker almost
    /// certainly still has its inputs on disk. Forgetting on disconnect would
    /// discard exactly the signal being collected.
    #[tokio::test]
    async fn locality_survives_a_worker_disconnect() {
        use crate::registry::WorkerRegistry;
        use brokkr_cas::Cas as _;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let input_root = Digest::of(b"sticky-inputs");
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
            platform: Some(os_platform("linux")),
            input_root_digest: Some(rapi::Digest {
                hash: input_root.hash().to_string(),
                size_bytes: input_root.size_bytes(),
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

        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler = Scheduler::with_registry_and_timeout(
            cas,
            ac,
            Duration::from_secs(10),
            registry.clone(),
        );
        let wid_a = WorkerId::new("w-a".to_string()).unwrap();
        let mut rx = register_and_connect(&scheduler, &registry, "w-a", &[("os", "linux")]).await;

        let exec: tokio::task::JoinHandle<Result<ExecutionOutcome, ExecutionError>> = {
            let (s, d) = (scheduler.clone(), action_digest.clone());
            tokio::spawn(async move { s.execute(d, true, TenantId::default()).await })
        };
        let job = recv_within(&mut rx, Duration::from_millis(500))
            .await
            .unwrap();
        scheduler
            .report(bv1::JobResult {
                job_id: job.job_id.clone(),
                result: Some(rapi::ActionResult {
                    exit_code: 0,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        exec.await.unwrap().unwrap();
        assert_eq!(
            scheduler
                .locality_input_root_hits(&wid_a, &input_root)
                .await,
            1
        );

        scheduler.disconnect_worker(&wid_a).await;

        assert_eq!(
            scheduler
                .locality_input_root_hits(&wid_a, &input_root)
                .await,
            1,
            "history must outlive the connection — a reconnecting worker still \
             has its inputs materialized"
        );
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
    /// D1 applies to the cache **lookup** too, and the real-process e2e is
    /// what proved it: a follower-routed `brokk run` died at
    /// `action cache get: not the metadata leader` before the action ever ran.
    /// A lookup that cannot be served is a miss, not a failed build.
    #[tokio::test]
    async fn a_follower_cache_lookup_is_a_miss_not_a_failure() {
        use crate::registry::WorkerRegistry;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler = Scheduler::with_registry_and_timeout(
            cas,
            Arc::new(FollowerActionCache),
            Duration::from_secs(10),
            registry.clone(),
        );
        let mut rx = register_and_connect(&scheduler, &registry, "w-a", &[("os", "linux")]).await;

        // Note: cache lookup is NOT skipped here — that is the whole point.
        let exec = {
            let s = scheduler.clone();
            tokio::spawn(async move { s.execute(action_digest, false, TenantId::default()).await })
        };
        // The action must be dispatched despite the unavailable lookup.
        let job = recv_within(&mut rx, Duration::from_millis(500))
            .await
            .unwrap();
        scheduler
            .report(bv1::JobResult {
                job_id: job.job_id.clone(),
                result: Some(rapi::ActionResult {
                    exit_code: 0,
                    stdout_raw: b"ran anyway".to_vec(),
                    ..Default::default()
                }),
                cache_hit: false,
                error_message: String::new(),
            })
            .await
            .unwrap();

        let outcome = exec.await.unwrap().unwrap();
        assert_eq!(outcome.result.stdout_raw, b"ran anyway");
        assert!(!outcome.cache_hit, "an unavailable lookup is a miss");
        assert!(!outcome.result_cached, "and the result could not be stored");
        assert_eq!(scheduler.uncached_results_not_leader(), 1);
    }

    /// Decision D1 (I9b): on a `--raft` follower the post-execution
    /// action-cache write is refused with `NotLeader`. The action already ran,
    /// so the build must **succeed** with the real result, flagged uncached and
    /// counted — never fail after paying for a sandbox run.
    #[tokio::test]
    async fn not_leader_returns_the_result_uncached_and_counts_it() {
        use crate::registry::WorkerRegistry;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(WriteFailsActionCache {
            error: || CasError::NotLeader {
                leader: Some("control-1".to_string()),
                leader_addr: Some("10.0.0.1:7878".to_string()),
            },
        });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler = Scheduler::with_registry_and_timeout(
            cas,
            ac,
            Duration::from_secs(10),
            registry.clone(),
        );
        let mut rx = register_and_connect(&scheduler, &registry, "w-a", &[("os", "linux")]).await;

        assert_eq!(scheduler.uncached_results_not_leader(), 0);

        let exec = {
            let s = scheduler.clone();
            tokio::spawn(async move { s.execute(action_digest, true, TenantId::default()).await })
        };
        let job = recv_within(&mut rx, Duration::from_millis(500))
            .await
            .unwrap();
        scheduler
            .report(bv1::JobResult {
                job_id: job.job_id.clone(),
                result: Some(rapi::ActionResult {
                    exit_code: 0,
                    stdout_raw: b"hello".to_vec(),
                    ..Default::default()
                }),
                cache_hit: false,
                error_message: String::new(),
            })
            .await
            .unwrap();

        // A NotLeader cache write must not fail the execution.
        let outcome = exec.await.unwrap().unwrap();
        assert_eq!(outcome.result.exit_code, 0);
        assert_eq!(
            outcome.result.stdout_raw, b"hello",
            "the real result reaches the caller"
        );
        assert!(!outcome.cache_hit);
        assert!(
            !outcome.result_cached,
            "the caller must be able to tell this was not cached"
        );
        assert_eq!(
            scheduler.uncached_results_not_leader(),
            1,
            "the degradation is counted, not silent"
        );
    }

    /// The other half of D1, and the part that keeps best-effort from becoming
    /// best-ignored: a cache write failing for any reason *other* than
    /// `NotLeader` is a real fault and must still fail the RPC.
    #[tokio::test]
    async fn a_non_not_leader_cache_write_failure_still_fails_the_execution() {
        use crate::registry::WorkerRegistry;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(WriteFailsActionCache {
            error: || CasError::Redb("disk on fire".to_string()),
        });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler = Scheduler::with_registry_and_timeout(
            cas,
            ac,
            Duration::from_secs(10),
            registry.clone(),
        );
        let mut rx = register_and_connect(&scheduler, &registry, "w-a", &[("os", "linux")]).await;

        let exec = {
            let s = scheduler.clone();
            tokio::spawn(async move { s.execute(action_digest, true, TenantId::default()).await })
        };
        let job = recv_within(&mut rx, Duration::from_millis(500))
            .await
            .unwrap();
        scheduler
            .report(bv1::JobResult {
                job_id: job.job_id.clone(),
                result: Some(rapi::ActionResult {
                    exit_code: 0,
                    ..Default::default()
                }),
                cache_hit: false,
                error_message: String::new(),
            })
            .await
            .unwrap();

        let err = exec.await.unwrap().unwrap_err();
        assert!(
            err.to_string().contains("action cache update"),
            "a storage failure must not be swallowed by the D1 path, got: {err}"
        );
        assert_eq!(
            scheduler.uncached_results_not_leader(),
            0,
            "only NotLeader increments the D1 counter"
        );
    }

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
            tokio::spawn(async move { s.execute(action_digest, true, TenantId::default()).await })
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

    /// Drain one job from whichever of two receivers has it within `budget`.
    async fn recv_either(
        rx_a: &mut tokio::sync::mpsc::Receiver<bv1::Job>,
        rx_b: &mut tokio::sync::mpsc::Receiver<bv1::Job>,
        budget: Duration,
    ) -> Option<bv1::Job> {
        for _ in 0..((budget.as_millis() / 10).max(1)) {
            if let Ok(job) = rx_a.try_recv() {
                return Some(job);
            }
            if let Ok(job) = rx_b.try_recv() {
                return Some(job);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }

    /// An expired lease (a connected-but-silent worker) causes the job to be
    /// requeued and re-dispatched. `reap_expired_at` is driven with a
    /// far-future instant so the lease is unambiguously expired without
    /// sleeping. NOTE: the re-dispatch may land on the *same* worker again — an
    /// expired-but-connected worker is not excluded (proper "reassign
    /// elsewhere" needs lease renewal / tried-worker tracking, a follow-up), so
    /// this asserts the requeue + re-dispatch mechanism, not the target worker.
    #[tokio::test]
    async fn lease_expiry_requeues_and_redispatches_job() {
        use crate::registry::WorkerRegistry;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action_digest = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler = Scheduler::with_registry_and_timeout(
            cas,
            ac,
            Duration::from_secs(10),
            registry.clone(),
        );
        let mut rx_a = register_and_connect(&scheduler, &registry, "w-a", &[("os", "linux")]).await;
        let mut rx_b = register_and_connect(&scheduler, &registry, "w-b", &[("os", "linux")]).await;

        let exec = {
            let s = scheduler.clone();
            tokio::spawn(async move { s.execute(action_digest, true, TenantId::default()).await })
        };

        // First dispatch — learn the job id.
        let first = recv_either(&mut rx_a, &mut rx_b, Duration::from_millis(500))
            .await
            .unwrap();
        let job_id = first.job_id.clone();

        // Force the lease past its deadline; the reaper requeues and
        // re-dispatches the job.
        scheduler
            .reap_expired_at(Instant::now() + Duration::from_secs(3600))
            .await;
        let again = recv_either(&mut rx_a, &mut rx_b, Duration::from_millis(1000))
            .await
            .unwrap();
        assert_eq!(again.job_id, job_id, "expired lease's job is re-dispatched");

        // Report success for the re-dispatched job → execute completes.
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
    }

    /// Stage an Action whose command echoes `arg`, so different `arg`s produce
    /// distinct action digests (identifiable in the worker's job stream).
    async fn stage_action_arg(
        cas: &brokkr_cas::InMemoryCas,
        arg: &str,
        platform: Option<rapi::Platform>,
    ) -> Digest {
        use brokkr_cas::Cas as _;

        let command = rapi::Command {
            arguments: vec!["/bin/echo".to_string(), arg.to_string()],
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

    /// §16 DoD: two tenants submitting concurrently share a single worker
    /// fairly — their jobs interleave rather than one tenant draining first.
    /// Both tenants enqueue (worker registered but not yet connected) so all
    /// six jobs are tagged before any dispatch; then one worker drains them in
    /// fair-queue order, reporting each to free itself.
    #[tokio::test]
    async fn two_tenants_share_a_worker_fairly() {
        use crate::registry::{WorkerCapabilities, WorkerRegistry};

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let plat = os_platform("linux");
        let action_a = stage_action_arg(cas.as_ref(), "a", Some(plat.clone())).await;
        let action_b = stage_action_arg(cas.as_ref(), "b", Some(plat)).await;
        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler = Scheduler::with_registry_and_timeout(
            cas,
            ac,
            Duration::from_secs(10),
            registry.clone(),
        );

        // Register the worker (so admission passes) but DON'T connect it yet —
        // all six jobs queue before any dispatch.
        let wid = WorkerId::new("w".to_string()).unwrap();
        registry.lock().await.register(
            wid.clone(),
            WorkerCapabilities {
                hostname: "w".to_string(),
                labels: [("os".to_string(), "linux".to_string())]
                    .into_iter()
                    .collect(),
            },
            Instant::now(),
        );

        let ta = TenantId::new("tenant-a".to_string()).unwrap();
        let tb = TenantId::new("tenant-b".to_string()).unwrap();
        let mut handles = Vec::new();
        for _ in 0..3 {
            let (s, d, t) = (scheduler.clone(), action_a.clone(), ta.clone());
            handles.push(tokio::spawn(async move { s.execute(d, true, t).await }));
        }
        for _ in 0..3 {
            let (s, d, t) = (scheduler.clone(), action_b.clone(), tb.clone());
            handles.push(tokio::spawn(async move { s.execute(d, true, t).await }));
        }

        // Let all six enqueue, then connect the worker so dispatch drains them.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<bv1::Job>(8);
        scheduler.connect_worker(wid, tx).await;

        // Drive the single worker: recv a job, note its tenant by action
        // digest, report success to free the worker, repeat for all six.
        let mut order = String::new();
        for _ in 0..6 {
            let job = recv_within(&mut rx, Duration::from_millis(1000))
                .await
                .unwrap();
            let digest = job.action_digest.as_ref().unwrap().hash.clone();
            order.push(if digest.as_str() == action_a.hash() {
                'a'
            } else {
                'b'
            });
            scheduler
                .report(bv1::JobResult {
                    job_id: job.job_id,
                    result: Some(rapi::ActionResult {
                        exit_code: 0,
                        ..Default::default()
                    }),
                    cache_hit: false,
                    error_message: String::new(),
                })
                .await
                .unwrap();
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }

        assert_eq!(order.matches('a').count(), 3, "order={order}");
        assert_eq!(order.matches('b').count(), 3, "order={order}");
        // Fair share ⇒ tenants interleave: the first B job dispatches before the
        // last A job (not a strict A-A-A-B-B-B drain).
        let first_b = order.find('b').unwrap();
        let last_a = order.rfind('a').unwrap();
        assert!(
            first_b < last_a,
            "tenants must interleave (fair share), got {order}"
        );
    }

    /// A tenant at its max-concurrent quota gets `QuotaExceeded` for further
    /// submissions while its earlier job is still in flight.
    #[tokio::test]
    async fn over_quota_rejects_additional_jobs() {
        use crate::registry::{WorkerCapabilities, WorkerRegistry};

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler = Scheduler::with_tenant_quota(cas, ac, registry.clone(), Some(1));

        // Register a matching worker so admission's fail-fast passes, but do
        // NOT connect it — the first job stays queued (in-flight) rather than
        // completing.
        registry.lock().await.register(
            WorkerId::new("w".to_string()).unwrap(),
            WorkerCapabilities {
                hostname: "w".to_string(),
                labels: [("os".to_string(), "linux".to_string())]
                    .into_iter()
                    .collect(),
            },
            Instant::now(),
        );

        let tenant = TenantId::new("t".to_string()).unwrap();
        let h1 = {
            let (s, d, t) = (scheduler.clone(), action.clone(), tenant.clone());
            tokio::spawn(async move { s.execute(d, true, t).await })
        };
        // Let the first job enqueue (count one in-flight) before submitting the
        // second.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let err = scheduler
            .execute(action.clone(), true, tenant.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ExecutionError::QuotaExceeded(1)),
            "expected QuotaExceeded(1), got {err:?}"
        );
        h1.abort();
    }

    /// Completing a job frees the tenant's quota slot so it can submit again.
    #[tokio::test]
    async fn completing_a_job_frees_tenant_quota() {
        use crate::registry::WorkerRegistry;

        let cas = Arc::new(brokkr_cas::InMemoryCas::new());
        let action = stage_action(cas.as_ref(), Some(os_platform("linux"))).await;
        let ac = Arc::new(MockActionCache { force_error: false });
        let registry = Arc::new(Mutex::new(WorkerRegistry::default()));
        let scheduler = Scheduler::with_tenant_quota(cas, ac, registry.clone(), Some(1));
        let mut rx = register_and_connect(&scheduler, &registry, "w", &[("os", "linux")]).await;

        let tenant = TenantId::new("t".to_string()).unwrap();
        // Job 1 dispatches to the worker (leased) — in-flight = 1.
        let h1 = {
            let (s, d, t) = (scheduler.clone(), action.clone(), tenant.clone());
            tokio::spawn(async move { s.execute(d, true, t).await })
        };
        let job1 = recv_within(&mut rx, Duration::from_millis(1000))
            .await
            .unwrap();

        // Job 2 (same tenant) is over quota while job 1 is in flight.
        let err = scheduler
            .execute(action.clone(), true, tenant.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ExecutionError::QuotaExceeded(1)),
            "got {err:?}"
        );

        // Complete job 1 → execute returns → quota slot freed.
        scheduler
            .report(bv1::JobResult {
                job_id: job1.job_id,
                result: Some(rapi::ActionResult {
                    exit_code: 0,
                    ..Default::default()
                }),
                cache_hit: false,
                error_message: String::new(),
            })
            .await
            .unwrap();
        h1.await.unwrap().unwrap();

        // Job 3 (same tenant) is now admitted and dispatches to the worker.
        let h3 = {
            let (s, d, t) = (scheduler.clone(), action.clone(), tenant.clone());
            tokio::spawn(async move { s.execute(d, true, t).await })
        };
        let job3 = recv_within(&mut rx, Duration::from_millis(1000)).await;
        assert!(
            job3.is_some(),
            "job after quota freed should be admitted and dispatched"
        );
        h3.abort();
    }
}
