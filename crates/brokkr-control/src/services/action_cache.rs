//! REAPI [`ActionCache`] service backed by a [`brokkr_cas::ActionCache`].

use std::sync::Arc;

use brokkr_cas::ActionCache;
use brokkr_proto::reapi_v2::{self as rapi, action_cache_server::ActionCache as AcSvc};
use tonic::{Request, Response, Status};

use brokkr_cas::CasError;

use super::{proto_to_digest, validate_instance_name};

/// Metadata key carrying the Raft leader's identity on a redirect (I8c).
pub const LEADER_HINT_METADATA_KEY: &str = "x-brokkr-leader";

/// Maps a cache-backend failure to gRPC. `NotLeader` (I8c, Raft-backed
/// metadata) becomes `FAILED_PRECONDITION` with the leader's identity in
/// `x-brokkr-leader` metadata so the caller retries against the leader;
/// everything else is `INTERNAL`.
fn cas_status(e: CasError) -> Status {
    match e {
        CasError::NotLeader { leader } => {
            let mut status =
                Status::failed_precondition("not the metadata leader; retry against the leader");
            if let Some(leader) = leader {
                if let Ok(value) = leader.parse() {
                    status
                        .metadata_mut()
                        .insert(LEADER_HINT_METADATA_KEY, value);
                }
            }
            status
        }
        other => Status::internal(other.to_string()),
    }
}

/// REAPI [`ActionCache`] service backed by a [`brokkr_cas::ActionCache`].
pub struct ActionCacheService<A: ActionCache + ?Sized> {
    backend: Arc<A>,
}

impl<A: ActionCache + ?Sized> ActionCacheService<A> {
    /// Wrap an action-cache backend.
    pub fn new(backend: Arc<A>) -> Self {
        Self { backend }
    }
}

#[tonic::async_trait]
impl<A: ActionCache + ?Sized> AcSvc for ActionCacheService<A> {
    async fn get_action_result(
        &self,
        request: Request<rapi::GetActionResultRequest>,
    ) -> Result<Response<rapi::ActionResult>, Status> {
        let span = tracing::info_span!("action_cache::get_action_result");
        let req = request.into_inner();
        validate_instance_name(&req.instance_name)?;
        let digest = proto_to_digest(
            req.action_digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing action_digest"))?,
        )?;
        let _enter = span.enter();
        if let Some(ref d) = req.action_digest {
            tracing::info!(action_digest = %format!("{}/{}", d.hash, d.size_bytes));
        }
        match self
            .backend
            .get_action_result(&digest)
            .await
            .map_err(cas_status)?
        {
            Some(r) => Ok(Response::new(r)),
            None => Err(Status::not_found("no cached action result")),
        }
    }

    async fn update_action_result(
        &self,
        request: Request<rapi::UpdateActionResultRequest>,
    ) -> Result<Response<rapi::ActionResult>, Status> {
        let span = tracing::info_span!("action_cache::update_action_result");
        let req = request.into_inner();
        validate_instance_name(&req.instance_name)?;
        let digest = proto_to_digest(
            req.action_digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing action_digest"))?,
        )?;
        let result = req
            .action_result
            .ok_or_else(|| Status::invalid_argument("missing action_result"))?;
        let _enter = span.enter();
        self.backend
            .update_action_result(&digest, result.clone())
            .await
            .map_err(cas_status)?;
        if let Some(ref d) = req.action_digest {
            tracing::info!(action_digest = %format!("{}/{}", d.hash, d.size_bytes));
        }
        Ok(Response::new(result))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn not_leader_maps_to_failed_precondition_with_a_leader_hint() {
        let status = cas_status(CasError::NotLeader {
            leader: Some("control-1".to_string()),
        });
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            status.metadata().get(LEADER_HINT_METADATA_KEY).unwrap(),
            "control-1"
        );

        // Unknown leader: still a redirectable precondition, no metadata.
        let status = cas_status(CasError::NotLeader { leader: None });
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.metadata().get(LEADER_HINT_METADATA_KEY).is_none());

        // Everything else stays INTERNAL.
        let status = cas_status(CasError::Redb("boom".to_string()));
        assert_eq!(status.code(), tonic::Code::Internal);
    }
}
