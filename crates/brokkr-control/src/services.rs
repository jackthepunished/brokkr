//! REAPI service implementations bound to Brokkr storage backends.

use std::sync::Arc;

use brokkr_cas::{ActionCache, Cas};
use brokkr_common::Digest;
use brokkr_proto::reapi_v2::{
    self as rapi, action_cache_server::ActionCache as AcSvc,
    capabilities_server::Capabilities as CapSvc,
    content_addressable_storage_server::ContentAddressableStorage as CasSvc,
    execution_server::Execution as ExecSvc,
};
use bytes::Bytes;
use prost::Message;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

/// REAPI [`ContentAddressableStorage`] service backed by a [`Cas`].
pub struct CasService<C: Cas> {
    backend: Arc<C>,
}

impl<C: Cas> CasService<C> {
    /// Wrap a CAS backend into a tonic service.
    pub fn new(backend: Arc<C>) -> Self {
        Self { backend }
    }
}

fn proto_to_digest(d: &rapi::Digest) -> Result<Digest, Status> {
    Digest::new(d.hash.clone(), d.size_bytes)
        .map_err(|e| Status::invalid_argument(format!("invalid digest: {e}")))
}

fn digest_to_proto(d: &Digest) -> rapi::Digest {
    rapi::Digest {
        hash: d.hash().to_string(),
        size_bytes: d.size_bytes(),
    }
}

#[tonic::async_trait]
impl<C: Cas> CasSvc for CasService<C> {
    async fn find_missing_blobs(
        &self,
        request: Request<rapi::FindMissingBlobsRequest>,
    ) -> Result<Response<rapi::FindMissingBlobsResponse>, Status> {
        let req = request.into_inner();
        let digests: Vec<Digest> = req
            .blob_digests
            .iter()
            .map(proto_to_digest)
            .collect::<Result<_, _>>()?;
        let missing = self
            .backend
            .find_missing_blobs(&digests)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(rapi::FindMissingBlobsResponse {
            missing_blob_digests: missing.iter().map(digest_to_proto).collect(),
        }))
    }

    async fn batch_update_blobs(
        &self,
        request: Request<rapi::BatchUpdateBlobsRequest>,
    ) -> Result<Response<rapi::BatchUpdateBlobsResponse>, Status> {
        let req = request.into_inner();
        let mut blobs: Vec<(Digest, Bytes)> = Vec::with_capacity(req.requests.len());
        for r in req.requests {
            let d = r
                .digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing digest"))?;
            let digest = proto_to_digest(d)?;
            blobs.push((digest, Bytes::from(r.data)));
        }
        let results = self
            .backend
            .batch_update_blobs(blobs)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let responses = results
            .into_iter()
            .map(|u| rapi::batch_update_blobs_response::Response {
                digest: Some(digest_to_proto(&u.digest)),
                status: Some(match u.status {
                    Ok(()) => brokkr_proto::rpc::Status {
                        code: 0,
                        message: String::new(),
                        details: vec![],
                    },
                    Err(msg) => brokkr_proto::rpc::Status {
                        // INVALID_ARGUMENT
                        code: 3,
                        message: msg,
                        details: vec![],
                    },
                }),
            })
            .collect();
        Ok(Response::new(rapi::BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        request: Request<rapi::BatchReadBlobsRequest>,
    ) -> Result<Response<rapi::BatchReadBlobsResponse>, Status> {
        let req = request.into_inner();
        let digests: Vec<Digest> = req
            .digests
            .iter()
            .map(proto_to_digest)
            .collect::<Result<_, _>>()?;
        let results = self
            .backend
            .batch_read_blobs(&digests)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let responses = digests
            .into_iter()
            .zip(results)
            .map(|(digest, res)| match res {
                Ok(bytes) => rapi::batch_read_blobs_response::Response {
                    digest: Some(digest_to_proto(&digest)),
                    data: bytes.to_vec(),
                    compressor: 0,
                    status: Some(brokkr_proto::rpc::Status {
                        code: 0,
                        message: String::new(),
                        details: vec![],
                    }),
                },
                Err(_) => rapi::batch_read_blobs_response::Response {
                    digest: Some(digest_to_proto(&digest)),
                    data: vec![],
                    compressor: 0,
                    status: Some(brokkr_proto::rpc::Status {
                        // NOT_FOUND
                        code: 5,
                        message: "blob not found".to_string(),
                        details: vec![],
                    }),
                },
            })
            .collect();
        Ok(Response::new(rapi::BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = ReceiverStream<Result<rapi::GetTreeResponse, Status>>;
    async fn get_tree(
        &self,
        _request: Request<rapi::GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        Err(Status::unimplemented("GetTree not implemented in Phase 1"))
    }

    async fn split_blob(
        &self,
        _request: Request<rapi::SplitBlobRequest>,
    ) -> Result<Response<rapi::SplitBlobResponse>, Status> {
        Err(Status::unimplemented(
            "SplitBlob not implemented in Phase 1",
        ))
    }

    async fn splice_blob(
        &self,
        _request: Request<rapi::SpliceBlobRequest>,
    ) -> Result<Response<rapi::SpliceBlobResponse>, Status> {
        Err(Status::unimplemented(
            "SpliceBlob not implemented in Phase 1",
        ))
    }
}

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

/// REAPI [`Capabilities`] service. Returns a static, Phase-1-appropriate set.
#[derive(Default)]
pub struct CapabilitiesService;

#[tonic::async_trait]
impl CapSvc for CapabilitiesService {
    async fn get_capabilities(
        &self,
        _request: Request<rapi::GetCapabilitiesRequest>,
    ) -> Result<Response<rapi::ServerCapabilities>, Status> {
        let caps = rapi::ServerCapabilities {
            cache_capabilities: Some(rapi::CacheCapabilities {
                digest_functions: vec![rapi::digest_function::Value::Sha256 as i32],
                action_cache_update_capabilities: Some(rapi::ActionCacheUpdateCapabilities {
                    update_enabled: true,
                }),
                max_batch_total_size_bytes: 4 * 1024 * 1024,
                symlink_absolute_path_strategy:
                    rapi::symlink_absolute_path_strategy::Value::Disallowed as i32,
                ..Default::default()
            }),
            execution_capabilities: Some(rapi::ExecutionCapabilities {
                digest_function: rapi::digest_function::Value::Sha256 as i32,
                exec_enabled: false,
                digest_functions: vec![rapi::digest_function::Value::Sha256 as i32],
                ..Default::default()
            }),
            low_api_version: Some(brokkr_proto::semver::SemVer {
                major: 2,
                ..Default::default()
            }),
            high_api_version: Some(brokkr_proto::semver::SemVer {
                major: 2,
                minor: 3,
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok(Response::new(caps))
    }
}

/// REAPI [`Execution`] service. Uses the scheduler to dispatch actions to a
/// worker and stream back `google.longrunning.Operation` updates.
pub struct ExecutionService {
    scheduler: Arc<crate::scheduler::Scheduler>,
}

impl ExecutionService {
    /// Bind the service to a scheduler.
    pub fn new(scheduler: Arc<crate::scheduler::Scheduler>) -> Self {
        Self { scheduler }
    }
}

fn execute_response_to_any(resp: rapi::ExecuteResponse) -> prost_types::Any {
    let mut buf = Vec::with_capacity(resp.encoded_len());
    // ExecuteResponse encoding cannot fail: all fields are owned, no length
    // overflow possible for in-memory bounded payloads we produce.
    let _ = resp.encode(&mut buf);
    prost_types::Any {
        type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse".to_string(),
        value: buf,
    }
}

#[tonic::async_trait]
impl ExecSvc for ExecutionService {
    type ExecuteStream = ReceiverStream<Result<brokkr_proto::longrunning::Operation, Status>>;
    async fn execute(
        &self,
        request: Request<rapi::ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let req = request.into_inner();
        let action_digest_proto = req
            .action_digest
            .ok_or_else(|| Status::invalid_argument("missing action_digest"))?;
        let action_digest = proto_to_digest(&action_digest_proto)?;

        let scheduler = self.scheduler.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(4);

        tokio::spawn(async move {
            let outcome = scheduler
                .execute(action_digest, req.skip_cache_lookup)
                .await;
            let op = match outcome {
                Ok(o) => {
                    let resp = rapi::ExecuteResponse {
                        result: Some(o.result),
                        cached_result: o.cache_hit,
                        status: Some(brokkr_proto::rpc::Status::default()),
                        ..Default::default()
                    };
                    brokkr_proto::longrunning::Operation {
                        name: format!("operations/{}", uuid::Uuid::new_v4()),
                        done: true,
                        result: Some(brokkr_proto::longrunning::operation::Result::Response(
                            execute_response_to_any(resp),
                        )),
                        ..Default::default()
                    }
                }
                Err(e) => brokkr_proto::longrunning::Operation {
                    name: format!("operations/{}", uuid::Uuid::new_v4()),
                    done: true,
                    result: Some(brokkr_proto::longrunning::operation::Result::Error(
                        brokkr_proto::rpc::Status {
                            code: 13,
                            message: e.to_string(),
                            details: vec![],
                        },
                    )),
                    ..Default::default()
                },
            };
            let _ = tx.send(Ok(op)).await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type WaitExecutionStream = ReceiverStream<Result<brokkr_proto::longrunning::Operation, Status>>;
    async fn wait_execution(
        &self,
        _request: Request<rapi::WaitExecutionRequest>,
    ) -> Result<Response<Self::WaitExecutionStream>, Status> {
        Err(Status::unimplemented(
            "WaitExecution not implemented in Phase 1",
        ))
    }
}
