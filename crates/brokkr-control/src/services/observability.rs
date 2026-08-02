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

use crate::scheduler::Scheduler;
use crate::views::{
    cas_stats_view, node_view_from_status, policy_view, worker_views, CasStatsView, NodeView,
    PolicyView, WorkerView,
};
use crate::wasm_strategy::WasmStrategy;
use crate::worker_service::SharedWorkerRegistry;

/// Handles the service reads from.
///
/// Not `Debug`: it holds trait objects over the CAS and the policy engine,
/// neither of which requires `Debug`, and adding that bound to both for a log
/// line would be the tail wagging the dog.
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

/// The gRPC surface over [`ObservabilityDeps`].
pub struct ObservabilityService {
    deps: ObservabilityDeps,
}

impl std::fmt::Debug for ObservabilityService {
    // `ObservabilityDeps` is not `Debug` (see its docs); report the identity.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservabilityService")
            .field("node_id", &self.deps.node_id)
            .finish_non_exhaustive()
    }
}

impl ObservabilityService {
    /// Wrap the handles this service reads from.
    pub fn new(deps: ObservabilityDeps) -> Self {
        Self { deps }
    }

    /// This node's own [`NodeView`].
    async fn local_node(&self) -> NodeView {
        let Some(raft) = self.deps.raft.as_ref() else {
            // No Raft: a single member, no leadership to report. Role is
            // `Unknown` rather than `Leader` because there is no consensus
            // that elected it, and claiming otherwise would be a lie a
            // three-node view could later contradict.
            return crate::views::standalone_node_view(
                &self.deps.node_id,
                &self.deps.advertise_addr,
            );
        };
        match raft.status().await {
            Ok(status) => {
                node_view_from_status(&self.deps.node_id, &self.deps.advertise_addr, &status)
            }
            Err(e) => {
                // A node that cannot read its own Raft state is degraded, not
                // absent. Report it unreachable rather than failing the call.
                tracing::warn!(error = %e, "could not read local Raft status");
                crate::views::unreachable_node_view(&self.deps.node_id, &self.deps.advertise_addr)
            }
        }
    }

    /// This node's workers.
    async fn local_workers(&self) -> Vec<WorkerView> {
        let inflight = self.deps.scheduler.inflight_snapshot().await;
        let reg = self.deps.registry.lock().await;
        worker_views(&reg, std::time::Instant::now(), &self.deps.node_id, &|id| {
            inflight.get(id).copied().unwrap_or(0)
        })
    }

    /// This node's policy state.
    fn local_policy(&self) -> PolicyView {
        policy_view(self.deps.policy.as_deref(), &self.deps.node_id)
    }

    /// This node's CAS size.
    async fn local_cas(&self) -> CasStatsView {
        match self.deps.cas.stats().await {
            Ok(stats) => cas_stats_view(stats, &self.deps.node_id),
            Err(e) => {
                // A CAS that cannot be measured reports zero rather than
                // failing the whole call — the rest of the view is still
                // useful, and an observability API is most needed when
                // something is already wrong.
                tracing::warn!(error = %e, "could not read local CAS stats");
                cas_stats_view(brokkr_cas::CasStats::default(), &self.deps.node_id)
            }
        }
    }
}

fn node_to_proto(v: &NodeView) -> bv1::NodeInfo {
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

fn worker_to_proto(v: &WorkerView) -> bv1::WorkerInfo {
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

fn policy_to_proto(v: &PolicyView) -> bv1::PolicyInfo {
    bv1::PolicyInfo {
        loaded: v.loaded,
        quarantined: v.quarantined,
        decided: v.decided,
        declined: v.declined,
        failures_by_reason: v.failures_by_reason.clone().into_iter().collect(),
        owning_node: v.owning_node.clone(),
    }
}

fn cas_to_proto(v: &CasStatsView) -> bv1::CasInfo {
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
        let node = self.local_node().await;
        let leader_id = if node.role == crate::views::RaftRole::Leader {
            node.node_id.clone()
        } else {
            String::new()
        };
        // Derived, never hardcoded. `local_node` reports this node unreachable
        // when its own Raft status cannot be read, and a view that then claimed
        // to be healthy would be lying about the one thing it is for.
        let healthy = node.reachable;
        Ok(Response::new(bv1::GetClusterReply {
            cluster: Some(bv1::ClusterInfo {
                nodes: vec![node_to_proto(&node)],
                leader_id,
                quorum_healthy: healthy,
                degraded: !healthy,
                // Local-only: there is no poll, so there is no snapshot time.
                as_of_unix_secs: 0,
            }),
        }))
    }

    #[tracing::instrument(name = "observability::list_workers", level = "debug", skip_all)]
    async fn list_workers(
        &self,
        _request: Request<bv1::ListWorkersRequest>,
    ) -> Result<Response<bv1::ListWorkersReply>, Status> {
        Ok(Response::new(bv1::ListWorkersReply {
            workers: self
                .local_workers()
                .await
                .iter()
                .map(worker_to_proto)
                .collect(),
        }))
    }

    #[tracing::instrument(name = "observability::get_policy", level = "debug", skip_all)]
    async fn get_policy(
        &self,
        _request: Request<bv1::GetPolicyRequest>,
    ) -> Result<Response<bv1::GetPolicyReply>, Status> {
        Ok(Response::new(bv1::GetPolicyReply {
            policies: vec![policy_to_proto(&self.local_policy())],
        }))
    }

    #[tracing::instrument(name = "observability::get_cas_stats", level = "debug", skip_all)]
    async fn get_cas_stats(
        &self,
        _request: Request<bv1::GetCasStatsRequest>,
    ) -> Result<Response<bv1::GetCasStatsReply>, Status> {
        Ok(Response::new(bv1::GetCasStatsReply {
            stores: vec![cas_to_proto(&self.local_cas().await)],
        }))
    }
}
