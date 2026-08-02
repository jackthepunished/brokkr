//! Read-only operator observability (ADR 0012).
//!
//! Serves the `views` read-model over gRPC on a dedicated operator listener.
//! This increment answers from **node-local state only**; cluster-wide
//! aggregation lands separately, behind the same wire types.
//!
//! # Why this is not behind the client auth interceptor
//!
//! ADR 0011's authenticator resolves a bearer token to a `TenantId` and
//! nothing else — there is no scope concept. Mounting this behind it would let
//! any tenant's token enumerate every worker and every other tenant's jobs, a
//! regression against ADR 0010. The *listener* is the boundary instead, and
//! adding a tenant-resolving interceptor here would imply a scope that does
//! not exist.

use std::sync::Arc;

use brokkr_cas::Cas;
use brokkr_proto::brokkr_v1 as bv1;
use brokkr_proto::brokkr_v1::observability_service_server::ObservabilityService as ObservabilityRpc;
use brokkr_raft::RaftHandle;
use tonic::{Request, Response, Status};

use crate::cluster::{ClusterEvent, ClusterSnapshot, SharedSnapshot};
use crate::scheduler::Scheduler;
use crate::views::{
    cas_stats_view, node_view_from_status, policy_view, worker_views, CasStatsView, JobState,
    JobSummary, NodeView, PolicyView, WorkerView,
};
use crate::wasm_strategy::WasmStrategy;
use crate::worker_service::SharedWorkerRegistry;

/// Handles the service reads from.
///
/// Not `Debug`: it holds trait objects over the CAS and the policy engine,
/// neither of which requires `Debug`, and adding that bound to both for a log
/// line would be the tail wagging the dog.
#[derive(Clone)]
pub struct ObservabilityDeps {
    /// This node's Raft id, or a stable local name when Raft is off.
    pub node_id: String,
    /// The address this node advertises.
    pub advertise_addr: String,
    /// Worker registry, for `ListWorkers`.
    pub registry: SharedWorkerRegistry,
    /// Scheduler, for per-worker in-flight counts.
    pub scheduler: Arc<Scheduler>,
    /// This node's CAS.
    pub cas: Arc<dyn Cas>,
    /// The WASM scheduling policy, when one is configured.
    pub policy: Option<Arc<WasmStrategy>>,
    /// Raft handle. `None` with `--raft` off, in which case the node reports
    /// itself as a single member of unknown role.
    pub raft: Option<Arc<RaftHandle>>,
}

/// The gRPC surface over a [`ClusterSnapshot`].
///
/// Reads only; the poller is the sole writer. Serving from the snapshot rather
/// than projecting per request is what keeps peer traffic independent of how
/// many operators are watching.
pub struct ObservabilityService {
    snapshot: SharedSnapshot,
    events: tokio::sync::broadcast::Sender<ClusterEvent>,
}

impl std::fmt::Debug for ObservabilityService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservabilityService")
            .finish_non_exhaustive()
    }
}

impl ObservabilityService {
    /// Serve reads from `snapshot`, and stream deltas from `events`.
    pub fn new(
        snapshot: SharedSnapshot,
        events: tokio::sync::broadcast::Sender<ClusterEvent>,
    ) -> Self {
        Self { snapshot, events }
    }

    /// The current state, as the `Snapshot` event a stream opens with.
    async fn snapshot_event(&self) -> bv1::ClusterEvent {
        let snap = self.snapshot.read().await;
        bv1::ClusterEvent {
            event: Some(bv1::cluster_event::Event::Snapshot(bv1::SnapshotEvent {
                cluster: Some(cluster_to_proto(&snap)),
                workers: snap.workers.iter().map(worker_to_proto).collect(),
                jobs: snap.jobs.iter().map(job_to_proto).collect(),
                policies: snap.policies.iter().map(policy_to_proto).collect(),
                stores: snap.cas.iter().map(cas_to_proto).collect(),
            })),
        }
    }
}

/// This node's own [`NodeView`].
///
/// A free function over the deps rather than a method, because
/// `PeerObservability` projects exactly the same local state and duplicating
/// it would let the two drift.
pub(crate) async fn local_node(deps: &ObservabilityDeps) -> NodeView {
    let Some(raft) = deps.raft.as_ref() else {
        // No Raft: a single member, no leadership to report. Role is `Unknown`
        // rather than `Leader` because nothing elected it, and claiming
        // otherwise would be a lie a later multi-node view could contradict.
        return crate::views::standalone_node_view(&deps.node_id, &deps.advertise_addr);
    };
    match raft.status().await {
        Ok(status) => node_view_from_status(&deps.node_id, &deps.advertise_addr, &status),
        Err(e) => {
            // A node that cannot read its own Raft state is degraded, not
            // absent. Report it unreachable rather than failing the call.
            tracing::warn!(error = %e, "could not read local Raft status");
            crate::views::unreachable_node_view(&deps.node_id, &deps.advertise_addr)
        }
    }
}

/// This node's workers.
pub(crate) async fn local_workers(deps: &ObservabilityDeps) -> Vec<WorkerView> {
    let inflight = deps.scheduler.inflight_snapshot().await;
    let reg = deps.registry.lock().await;
    worker_views(&reg, std::time::Instant::now(), &deps.node_id, &|id| {
        inflight.get(id).copied().unwrap_or(0)
    })
}

/// This node's policy state.
pub(crate) fn local_policy(deps: &ObservabilityDeps) -> PolicyView {
    policy_view(deps.policy.as_deref(), &deps.node_id)
}

/// This node's recently completed jobs.
///
/// Unbounded on purpose: the ring's own capacity — set from
/// `--observe-job-history` — is the only limit that should apply here. Passing
/// `DEFAULT_JOB_HISTORY` would silently cap an operator who configured a
/// larger ring at the default, so raising the flag would appear to do nothing.
/// The caller's `limit` is applied later, to the merged cross-node order.
pub(crate) async fn local_jobs(deps: &ObservabilityDeps) -> Vec<JobSummary> {
    deps.scheduler.recent_jobs(None, usize::MAX).await
}

/// This node's CAS size.
pub(crate) async fn local_cas(deps: &ObservabilityDeps) -> CasStatsView {
    match deps.cas.stats().await {
        Ok(stats) => cas_stats_view(stats, &deps.node_id),
        Err(e) => {
            // A CAS that cannot be measured reports zero rather than failing
            // the whole call — the rest of the view is still useful, and an
            // observability API is most needed when something is already wrong.
            tracing::warn!(error = %e, "could not read local CAS stats");
            cas_stats_view(brokkr_cas::CasStats::default(), &deps.node_id)
        }
    }
}

pub(crate) fn node_to_proto(v: &NodeView) -> bv1::NodeInfo {
    bv1::NodeInfo {
        node_id: v.node_id.clone(),
        advertise_addr: v.advertise_addr.clone(),
        role: v.role.as_str().to_string(),
        term: v.term,
        commit_index: v.commit_index,
        last_applied: v.last_applied,
        reachable: v.reachable,
        last_seen_secs: v.last_seen_secs,
    }
}

pub(crate) fn worker_to_proto(v: &WorkerView) -> bv1::WorkerInfo {
    bv1::WorkerInfo {
        worker_id: v.worker_id.clone(),
        hostname: v.hostname.clone(),
        labels: v.labels.clone().into_iter().collect(),
        inflight: v.inflight,
        last_seen_secs: v.last_seen_secs,
        stale: v.stale,
        owning_node: v.owning_node.clone(),
    }
}

pub(crate) fn policy_to_proto(v: &PolicyView) -> bv1::PolicyInfo {
    bv1::PolicyInfo {
        loaded: v.loaded,
        quarantined: v.quarantined,
        decided: v.decided,
        declined: v.declined,
        failures_by_reason: v.failures_by_reason.clone().into_iter().collect(),
        owning_node: v.owning_node.clone(),
    }
}

pub(crate) fn cluster_to_proto(snap: &ClusterSnapshot) -> bv1::ClusterInfo {
    bv1::ClusterInfo {
        nodes: snap.nodes.iter().map(node_to_proto).collect(),
        leader_id: snap.leader_id.clone().unwrap_or_default(),
        // Derived, never hardcoded: a view that claimed health while a node was
        // silent would be lying about the one thing it exists for.
        quorum_healthy: !snap.degraded,
        degraded: snap.degraded,
        as_of_unix_secs: snap
            .as_of
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0),
    }
}

/// Map a delta onto the wire.
pub(crate) fn event_to_proto(e: &ClusterEvent) -> bv1::ClusterEvent {
    use bv1::cluster_event::Event as E;
    let event = match e {
        // `Snapshot` is built by the service, which holds the state; a delta
        // stream never carries one through this path.
        ClusterEvent::Snapshot(_) => return bv1::ClusterEvent { event: None },
        ClusterEvent::NodeUnreachable { node_id } => E::NodeUnreachable(bv1::NodeEvent {
            node_id: node_id.clone(),
        }),
        ClusterEvent::NodeRecovered { node_id } => E::NodeRecovered(bv1::NodeEvent {
            node_id: node_id.clone(),
        }),
        ClusterEvent::WorkerAdded {
            worker_id,
            owning_node,
        } => E::WorkerAdded(bv1::WorkerEvent {
            worker_id: worker_id.clone(),
            owning_node: owning_node.clone(),
        }),
        ClusterEvent::WorkerRemoved {
            worker_id,
            owning_node,
        } => E::WorkerRemoved(bv1::WorkerEvent {
            worker_id: worker_id.clone(),
            owning_node: owning_node.clone(),
        }),
        ClusterEvent::WorkerStale {
            worker_id,
            owning_node,
        } => E::WorkerStale(bv1::WorkerEvent {
            worker_id: worker_id.clone(),
            owning_node: owning_node.clone(),
        }),
        ClusterEvent::PolicyQuarantined { owning_node } => E::PolicyQuarantined(bv1::PolicyEvent {
            owning_node: owning_node.clone(),
        }),
        ClusterEvent::PolicyRecovered { owning_node } => E::PolicyRecovered(bv1::PolicyEvent {
            owning_node: owning_node.clone(),
        }),
        ClusterEvent::LeaderChanged { from, to } => E::LeaderChanged(bv1::LeaderEvent {
            from: from.clone().unwrap_or_default(),
            to: to.clone().unwrap_or_default(),
        }),
    };
    bv1::ClusterEvent { event: Some(event) }
}

pub(crate) fn job_to_proto(v: &JobSummary) -> bv1::JobInfo {
    bv1::JobInfo {
        job_id: v.job_id.clone(),
        tenant: v.tenant.clone(),
        action_digest: v.action_digest.clone(),
        state: v.state.as_str().to_string(),
        worker_id: v.worker_id.clone().unwrap_or_default(),
        exit_code: v.exit_code.unwrap_or(0),
        // proto3 cannot distinguish an unset int32 from 0, and 0 is a
        // meaningful exit code, so presence is carried explicitly.
        has_exit_code: v.exit_code.is_some(),
        owning_node: v.owning_node.clone(),
        completed_at_unix_ms: v.completed_at_unix_ms,
    }
}

pub(crate) fn cas_to_proto(v: &CasStatsView) -> bv1::CasInfo {
    bv1::CasInfo {
        objects: v.objects,
        bytes: v.bytes,
        owning_node: v.owning_node.clone(),
    }
}

#[tonic::async_trait]
impl ObservabilityRpc for ObservabilityService {
    #[tracing::instrument(name = "observability::get_cluster", level = "debug", skip_all)]
    async fn get_cluster(
        &self,
        _request: Request<bv1::GetClusterRequest>,
    ) -> Result<Response<bv1::GetClusterReply>, Status> {
        let snap = self.snapshot.read().await;
        Ok(Response::new(bv1::GetClusterReply {
            cluster: Some(cluster_to_proto(&snap)),
        }))
    }

    #[tracing::instrument(name = "observability::list_workers", level = "debug", skip_all)]
    async fn list_workers(
        &self,
        _request: Request<bv1::ListWorkersRequest>,
    ) -> Result<Response<bv1::ListWorkersReply>, Status> {
        let snap = self.snapshot.read().await;
        Ok(Response::new(bv1::ListWorkersReply {
            workers: snap.workers.iter().map(worker_to_proto).collect(),
        }))
    }

    #[tracing::instrument(name = "observability::get_policy", level = "debug", skip_all)]
    async fn get_policy(
        &self,
        _request: Request<bv1::GetPolicyRequest>,
    ) -> Result<Response<bv1::GetPolicyReply>, Status> {
        let snap = self.snapshot.read().await;
        Ok(Response::new(bv1::GetPolicyReply {
            policies: snap.policies.iter().map(policy_to_proto).collect(),
        }))
    }

    type WatchEventsStream =
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<bv1::ClusterEvent, Status>> + Send>>;

    /// Stream cluster deltas.
    ///
    /// The resync contract, which is what makes this safe to rely on:
    ///
    /// 1. **On subscribe — and therefore on every reconnect** — a full
    ///    `Snapshot` is sent before any delta. A reconnecting client is in
    ///    exactly the position of a first-time client and is treated
    ///    identically. No sequence numbers, no replay window, no cursor to get
    ///    wrong.
    /// 2. **On lag** — a slow consumer overflowing the bounded buffer — a
    ///    fresh `Snapshot` is sent rather than dropping the client or silently
    ///    skipping deltas. Falling behind is acceptable; *not knowing* you fell
    ///    behind is not.
    ///
    /// A client therefore needs no reconciliation logic: every `Snapshot`
    /// replaces its world, and every delta between two of them is complete.
    #[tracing::instrument(name = "observability::watch_events", level = "debug", skip_all)]
    async fn watch_events(
        &self,
        _request: Request<bv1::WatchEventsRequest>,
    ) -> Result<Response<Self::WatchEventsStream>, Status> {
        // Subscribe *before* taking the initial snapshot, so a delta occurring
        // between the two is buffered rather than lost.
        let mut rx = self.events.subscribe();
        let initial = self.snapshot_event().await;
        let snapshot = self.snapshot.clone();

        let (tx, out) = tokio::sync::mpsc::channel(crate::cluster::EVENT_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            if tx.send(Ok(initial)).await.is_err() {
                return;
            }
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if tx.send(Ok(event_to_proto(&event))).await.is_err() {
                            return; // client went away
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(
                            missed,
                            "observability subscriber fell behind; resending a full snapshot"
                        );
                        let snap = snapshot.read().await;
                        let resync = bv1::ClusterEvent {
                            event: Some(bv1::cluster_event::Event::Snapshot(bv1::SnapshotEvent {
                                cluster: Some(cluster_to_proto(&snap)),
                                workers: snap.workers.iter().map(worker_to_proto).collect(),
                                jobs: snap.jobs.iter().map(job_to_proto).collect(),
                                policies: snap.policies.iter().map(policy_to_proto).collect(),
                                stores: snap.cas.iter().map(cas_to_proto).collect(),
                            })),
                        };
                        drop(snap);
                        if tx.send(Ok(resync)).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(out),
        )))
    }

    #[tracing::instrument(name = "observability::list_jobs", level = "debug", skip_all)]
    async fn list_jobs(
        &self,
        request: Request<bv1::ListJobsRequest>,
    ) -> Result<Response<bv1::ListJobsReply>, Status> {
        let req = request.into_inner();
        // An unrecognised filter is "no filter", not an error: a client from a
        // newer release asking about a state we do not know should get
        // everything rather than a rejection.
        let filter = JobState::from_str_opt(&req.state_filter);
        let limit = if req.limit == 0 {
            crate::views::DEFAULT_JOB_HISTORY
        } else {
            req.limit as usize
        };
        let snap = self.snapshot.read().await;
        // The snapshot is already sorted newest-first across every node, so
        // the limit is applied to the merged order rather than per node.
        Ok(Response::new(bv1::ListJobsReply {
            jobs: snap
                .jobs
                .iter()
                .filter(|j| filter.is_none_or(|f| j.state == f))
                .take(limit)
                .map(job_to_proto)
                .collect(),
        }))
    }

    #[tracing::instrument(name = "observability::get_job", level = "debug", skip_all)]
    async fn get_job(
        &self,
        request: Request<bv1::GetJobRequest>,
    ) -> Result<Response<bv1::GetJobReply>, Status> {
        let job_id = request.into_inner().job_id;
        let snap = self.snapshot.read().await;
        let found = snap.jobs.iter().find(|j| j.job_id == job_id);
        match found {
            Some(j) => Ok(Response::new(bv1::GetJobReply {
                job: Some(job_to_proto(j)),
            })),
            // NotFound rather than an empty reply: "I do not have that job" and
            // "that job had no data" are different answers, and the ring is
            // bounded so a genuinely old job legitimately falls out.
            None => Err(Status::not_found(format!(
                "job {job_id} is not in any node's recent-job history"
            ))),
        }
    }

    #[tracing::instrument(name = "observability::get_cas_stats", level = "debug", skip_all)]
    async fn get_cas_stats(
        &self,
        _request: Request<bv1::GetCasStatsRequest>,
    ) -> Result<Response<bv1::GetCasStatsReply>, Status> {
        let snap = self.snapshot.read().await;
        Ok(Response::new(bv1::GetCasStatsReply {
            stores: snap.cas.iter().map(cas_to_proto).collect(),
        }))
    }
}

/// [`LocalStateSource`](crate::cluster::LocalStateSource) over the handles this
/// node already holds.
///
/// Caches the last CAS measurement so rounds where `refresh_cas` is false cost
/// nothing: `RedbCas` answers by scanning under a throughput permit, and the
/// poller must not take one every tick.
pub struct LocalState {
    deps: ObservabilityDeps,
    last_cas: tokio::sync::Mutex<Option<CasStatsView>>,
}

impl std::fmt::Debug for LocalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalState")
            .field("node_id", &self.deps.node_id)
            .finish_non_exhaustive()
    }
}

impl LocalState {
    /// Build a source over `deps`.
    pub fn new(deps: ObservabilityDeps) -> Self {
        Self {
            deps,
            last_cas: tokio::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl crate::cluster::LocalStateSource for LocalState {
    async fn local_state(&self, refresh_cas: bool) -> crate::cluster::NodeState {
        let mut cached = self.last_cas.lock().await;
        // Measure when asked, or when we have never measured — otherwise the
        // first snapshot would report an empty CAS for a whole cas_interval.
        if refresh_cas || cached.is_none() {
            *cached = Some(local_cas(&self.deps).await);
        }
        let cas = cached
            .clone()
            .unwrap_or_else(|| cas_stats_view(brokkr_cas::CasStats::default(), &self.deps.node_id));
        drop(cached);

        crate::cluster::NodeState {
            node: local_node(&self.deps).await,
            workers: local_workers(&self.deps).await,
            policy: local_policy(&self.deps),
            jobs: local_jobs(&self.deps).await,
            cas,
        }
    }
}
