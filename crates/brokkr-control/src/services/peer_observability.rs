//! Node-local observability for peer aggregation (ADR 0012).
//!
//! Served on the **Raft peer plane**, where peers are already mutually
//! authenticated by mTLS and their addresses are already published in the
//! cluster configuration. No new credential, and nothing added to the
//! tenant-facing surface.
//!
//! # This service cannot fan out
//!
//! It returns *this node's* state and contains no path that calls a peer. That
//! is the no-recursion guarantee for aggregation, and it is structural on
//! purpose: a "do not fan out" flag can be forgotten, mis-defaulted, or
//! spoofed, whereas a service with no recursion path cannot be made to
//! recurse.
//!
//! The file's brevity is part of the guarantee. If it starts growing, check
//! why.

use brokkr_proto::brokkr_v1 as bv1;
use brokkr_proto::brokkr_v1::peer_observability_server::PeerObservability as PeerObservabilityRpc;
use tonic::{Request, Response, Status};

use super::observability::{
    cas_to_proto, local_cas, local_node, local_policy, local_workers, node_to_proto,
    policy_to_proto, worker_to_proto, ObservabilityDeps,
};

/// Serves one node's own observability state to its Raft peers.
pub struct PeerObservabilityService {
    deps: ObservabilityDeps,
}

impl std::fmt::Debug for PeerObservabilityService {
    // `ObservabilityDeps` is not `Debug` (see its docs); report the identity.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerObservabilityService")
            .field("node_id", &self.deps.node_id)
            .finish_non_exhaustive()
    }
}

impl PeerObservabilityService {
    /// Wrap the handles this service reads from.
    pub fn new(deps: ObservabilityDeps) -> Self {
        Self { deps }
    }
}

#[tonic::async_trait]
impl PeerObservabilityRpc for PeerObservabilityService {
    #[tracing::instrument(
        name = "peer_observability::get_local_state",
        level = "debug",
        skip_all
    )]
    async fn get_local_state(
        &self,
        _request: Request<bv1::GetLocalStateRequest>,
    ) -> Result<Response<bv1::GetLocalStateReply>, Status> {
        Ok(Response::new(bv1::GetLocalStateReply {
            node: Some(node_to_proto(&local_node(&self.deps).await)),
            workers: local_workers(&self.deps)
                .await
                .iter()
                .map(worker_to_proto)
                .collect(),
            policy: Some(policy_to_proto(&local_policy(&self.deps))),
            cas: Some(cas_to_proto(&local_cas(&self.deps).await)),
        }))
    }
}
