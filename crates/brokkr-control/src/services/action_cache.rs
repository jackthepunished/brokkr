//! REAPI [`ActionCache`] service backed by a [`brokkr_cas::ActionCache`].

use std::sync::Arc;

use brokkr_cas::ActionCache;
use brokkr_proto::reapi_v2::{self as rapi, action_cache_server::ActionCache as AcSvc};
use tonic::{Request, Response, Status};

use super::proto_to_digest;

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
    #[tracing::instrument(
        name = "action_cache::get_action_result",
        skip(self),
        fields(action_digest = ?req.action_digest),
    )]
    async fn get_action_result(
        &self,
        request: Request<rapi::GetActionResultRequest>,
    ) -> Result<Response<rapi::ActionResult>, Status> {
        let req = request.into_inner();
        let d = req
            .action_digest
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing action_digest"))?;
        let digest = proto_to_digest(d)?;
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

    #[tracing::instrument(
        name = "action_cache::update_action_result",
        skip(self),
    )]
    async fn update_action_result(
        &self,
        request: Request<rapi::UpdateActionResultRequest>,
    ) -> Result<Response<rapi::ActionResult>, Status> {
        let req = request.into_inner();
        let d = req
            .action_digest
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing action_digest"))?;
        let digest = proto_to_digest(d)?;
        let result = req
            .action_result
            .ok_or_else(|| Status::invalid_argument("missing action_result"))?;
        self.backend
            .update_action_result(&digest, result.clone())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(result))
    }
}