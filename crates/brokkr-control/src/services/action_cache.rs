//! REAPI [`ActionCache`] service backed by a [`brokkr_cas::ActionCache`].

use std::sync::Arc;

use brokkr_cas::ActionCache;
use brokkr_proto::reapi_v2::{self as rapi, action_cache_server::ActionCache as AcSvc};
use tonic::{Request, Response, Status};

use brokkr_cas::CasError;

use super::{proto_to_digest, validate_instance_name};

/// Metadata key carrying the Raft leader's identity on a redirect (I8c).
pub const LEADER_HINT_METADATA_KEY: &str = "x-brokkr-leader";

/// Metadata key carrying the Raft leader's client-plane address on a redirect
/// (I9b). A node id is not dialable, so without this a client cannot act on
/// [`LEADER_HINT_METADATA_KEY`]; the id is kept alongside it for logging and
/// for the window where the leader's address has not replicated yet.
pub const LEADER_ADDR_METADATA_KEY: &str = "x-brokkr-leader-addr";

/// Maps a cache-backend failure to gRPC. `NotLeader` (I8c, Raft-backed
/// metadata) becomes `FAILED_PRECONDITION` with the leader's identity in
/// `x-brokkr-leader` and, when the cluster has published it, the leader's
/// address in `x-brokkr-leader-addr` — so the caller can redirect to the
/// leader rather than merely learn its name (I9b). Everything else is
/// `INTERNAL`.
fn cas_status(e: CasError) -> Status {
    match e {
        CasError::NotLeader {
            leader,
            leader_addr,
        } => {
            let mut status =
                Status::failed_precondition("not the metadata leader; retry against the leader");
            // Each hint is emitted independently: an id with no address is the
            // normal state between an election and that leader's record
            // committing, and an address that will not parse as a header value
            // must not suppress the id.
            for (key, hint) in [
                (LEADER_HINT_METADATA_KEY, leader),
                (LEADER_ADDR_METADATA_KEY, leader_addr),
            ] {
                if let Some(hint) = hint {
                    match hint.parse() {
                        Ok(value) => {
                            status.metadata_mut().insert(key, value);
                        }
                        Err(_) => tracing::warn!(
                            metadata_key = key,
                            hint = %hint,
                            "leader hint is not a valid gRPC metadata value; omitting it"
                        ),
                    }
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
        // Hold the GC coordination barrier (issue #144) across the
        // AC write so that an in-process `gc::sweep` cannot delete
        // the blobs referenced by this `ActionResult` after this
        // handler commits. The guard is dropped when this handler
        // returns; coverage is sufficient because workers upload
        // CAS blobs over a *separate* gRPC stream before invoking
        // this RPC, and the barrier closes the in-process window
        // that previously raced between `cas.list_digests()` and
        // `cas.delete_blob(d)`.
        let _gc_guard = self
            .backend
            .gc_window()
            .await
            .map_err(|e| Status::internal(format!("gc_window: {e}")))?;
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
        // Id + address: both keys, so the caller can dial the leader directly.
        let status = cas_status(CasError::NotLeader {
            leader: Some("control-1".to_string()),
            leader_addr: Some("10.0.0.1:7878".to_string()),
        });
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            status.metadata().get(LEADER_HINT_METADATA_KEY).unwrap(),
            "control-1"
        );
        assert_eq!(
            status.metadata().get(LEADER_ADDR_METADATA_KEY).unwrap(),
            "10.0.0.1:7878"
        );

        // Unknown leader: still a redirectable precondition, no metadata.
        let status = cas_status(CasError::NotLeader {
            leader: None,
            leader_addr: None,
        });
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.metadata().get(LEADER_HINT_METADATA_KEY).is_none());
        assert!(status.metadata().get(LEADER_ADDR_METADATA_KEY).is_none());

        // Everything else stays INTERNAL.
        let status = cas_status(CasError::Redb("boom".to_string()));
        assert_eq!(status.code(), tonic::Code::Internal);
    }

    /// The election window (I9b): a leader is recognized before its
    /// `cfg/nodes/<id>` record has replicated, so the id is known and the
    /// address is not. The redirect must still be emitted — the client falls
    /// back to its configured endpoints — and must not invent an address.
    #[test]
    fn a_known_leader_with_no_published_address_emits_only_the_id() {
        let status = cas_status(CasError::NotLeader {
            leader: Some("control-2".to_string()),
            leader_addr: None,
        });
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            status.metadata().get(LEADER_HINT_METADATA_KEY).unwrap(),
            "control-2"
        );
        assert!(status.metadata().get(LEADER_ADDR_METADATA_KEY).is_none());
    }

    /// A malformed address must not silently vanish into a successful-looking
    /// redirect with no address: the id still goes out, and the bad value is
    /// simply not emitted as metadata (ASCII-invalid header values cannot be).
    #[test]
    fn an_unparseable_leader_address_still_yields_the_id_hint() {
        let status = cas_status(CasError::NotLeader {
            leader: Some("control-3".to_string()),
            leader_addr: Some("bad\u{7f}value".to_string()),
        });
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            status.metadata().get(LEADER_HINT_METADATA_KEY).unwrap(),
            "control-3"
        );
        assert!(status.metadata().get(LEADER_ADDR_METADATA_KEY).is_none());
    }
}
