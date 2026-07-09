//! REAPI [`ActionCache`] service backed by a [`brokkr_cas::ActionCache`].

use std::sync::Arc;

use brokkr_cas::ActionCache;
use brokkr_proto::reapi_v2::{self as rapi, action_cache_server::ActionCache as AcSvc};
use tonic::{Request, Response, Status};

use super::{proto_to_digest, validate_instance_name};

/// REAPI [`ActionCache`] service backed by a [`brokkr_cas::ActionCache`].
pub struct ActionCacheService<A: ActionCache> {
    backend: Arc<A>,
}

impl<A: ActionCache> ActionCacheService<A> {
    /// Wrap an action-cache backend.
    pub fn new(backend: Arc<A>) -> Self {
        Self { backend }
    }
}

#[tonic::async_trait]
impl<A: ActionCache> AcSvc for ActionCacheService<A> {
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
            .map_err(|e| Status::internal(e.to_string()))?
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
            .map_err(|e| Status::internal(e.to_string()))?;
        if let Some(ref d) = req.action_digest {
            tracing::info!(action_digest = %format!("{}/{}", d.hash, d.size_bytes));
        }
        Ok(Response::new(result))
    }
}
