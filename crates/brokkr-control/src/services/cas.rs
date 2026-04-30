//! REAPI `ContentAddressableStorage` service backed by a [`Cas`].

use std::sync::Arc;

use brokkr_cas::Cas;
use brokkr_proto::reapi_v2::{
    self as rapi, content_addressable_storage_server::ContentAddressableStorage as CasSvc,
};
use bytes::Bytes;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::{digest_to_proto, proto_to_digest};

/// REAPI `ContentAddressableStorage` service backed by a [`Cas`].
pub struct CasService<C: Cas> {
    backend: Arc<C>,
}

impl<C: Cas> CasService<C> {
    /// Wrap a CAS backend into a tonic service.
    ///
    /// The `backend` argument must be an `Arc` wrapper around the concrete CAS
    /// implementation (e.g. [`brokkr_cas::InMemoryCas`]). The `Arc` is required
    /// because the service may clone the handle to spawn per-request background
    /// work.
    pub fn new(backend: Arc<C>) -> Self {
        Self { backend }
    }
}

#[tonic::async_trait]
impl<C: Cas> CasSvc for CasService<C> {
    async fn find_missing_blobs(
        &self,
        request: Request<rapi::FindMissingBlobsRequest>,
    ) -> Result<Response<rapi::FindMissingBlobsResponse>, Status> {
        let span = tracing::info_span!("cas::find_missing_blobs");
        let _enter = span.enter();
        let req = request.into_inner();
        let digests: Vec<super::Digest> = req
            .blob_digests
            .iter()
            .map(proto_to_digest)
            .collect::<Result<_, _>>()?;
        tracing::info!(blob_count = req.blob_digests.len());
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
        let span = tracing::info_span!("cas::batch_update_blobs");
        let _enter = span.enter();
        let req = request.into_inner();
        let request_count = req.requests.len();
        let mut blobs: Vec<(super::Digest, Bytes)> = Vec::with_capacity(request_count);
        for r in req.requests {
            let d = r
                .digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing digest"))?;
            let digest = proto_to_digest(d)?;
            blobs.push((digest, Bytes::from(r.data)));
        }
        tracing::info!(request_count);
        let results = self
            .backend
            .batch_update_blobs(blobs)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        if results.len() != request_count {
            return Err(Status::internal(format!(
                "backend returned {} results for {} requests",
                results.len(),
                request_count
            )));
        }
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
        let span = tracing::info_span!("cas::batch_read_blobs");
        let _enter = span.enter();
        let req = request.into_inner();
        let digest_count = req.digests.len();
        let digests: Vec<super::Digest> = req
            .digests
            .iter()
            .map(proto_to_digest)
            .collect::<Result<_, _>>()?;
        tracing::info!(digest_count);
        let results = self
            .backend
            .batch_read_blobs(&digests)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        if results.len() != digest_count {
            return Err(Status::internal(format!(
                "backend returned {} results for {} digests",
                results.len(),
                digest_count
            )));
        }
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
