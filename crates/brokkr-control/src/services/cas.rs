//! REAPI `ContentAddressableStorage` service backed by a [`Cas`].

use std::sync::Arc;

use brokkr_cas::Cas;
use brokkr_proto::reapi_v2::{
    self as rapi, content_addressable_storage_server::ContentAddressableStorage as CasSvc,
};
use bytes::Bytes;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::{digest_to_proto, proto_to_digest, validate_instance_name};

/// REAPI `ContentAddressableStorage` service backed by a [`Cas`].
pub struct CasService<C: Cas> {
    backend: Arc<C>,
}

impl<C: Cas> CasService<C> {
    /// Wrap a CAS backend into a tonic service.
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
        let req = request.into_inner();
        validate_instance_name(&req.instance_name)?;
        let digests: Vec<super::Digest> = req
            .blob_digests
            .iter()
            .map(proto_to_digest)
            .collect::<Result<_, _>>()?;
        let missing = self
            .backend
            .find_missing_blobs(&digests)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let _enter = span.enter();
        tracing::info!(blob_count = req.blob_digests.len());
        Ok(Response::new(rapi::FindMissingBlobsResponse {
            missing_blob_digests: missing.iter().map(digest_to_proto).collect(),
        }))
    }

    async fn batch_update_blobs(
        &self,
        request: Request<rapi::BatchUpdateBlobsRequest>,
    ) -> Result<Response<rapi::BatchUpdateBlobsResponse>, Status> {
        let span = tracing::info_span!("cas::batch_update_blobs");
        let req = request.into_inner();
        validate_instance_name(&req.instance_name)?;
        let request_count = req.requests.len();

        // Verify each entry's declared digest against its bytes *before*
        // we hand the batch to the backend. The backend re-verifies (issue
        // #70 defence-in-depth), but rejecting at the service boundary
        // avoids a spawn_blocking redb txn for known-bad entries and gives
        // the client per-entry feedback in the REAPI-required shape.
        let mut responses: Vec<rapi::batch_update_blobs_response::Response> =
            Vec::with_capacity(request_count);
        let mut blobs: Vec<(super::Digest, Bytes)> = Vec::with_capacity(request_count);
        let mut accepted_indices: Vec<usize> = Vec::with_capacity(request_count);
        for r in req.requests {
            let d = r
                .digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing digest"))?;
            let digest = proto_to_digest(d)?;
            let data = Bytes::from(r.data);
            match digest.verify(data.as_ref()) {
                Ok(()) => {
                    accepted_indices.push(responses.len());
                    responses.push(rapi::batch_update_blobs_response::Response {
                        digest: Some(digest_to_proto(&digest)),
                        // Placeholder; replaced after backend write succeeds.
                        status: None,
                    });
                    blobs.push((digest, data));
                }
                Err(e) => {
                    responses.push(rapi::batch_update_blobs_response::Response {
                        digest: Some(digest_to_proto(&digest)),
                        status: Some(brokkr_proto::rpc::Status {
                            // INVALID_ARGUMENT
                            code: 3,
                            message: format!("digest verification failed: {e}"),
                            details: vec![],
                        }),
                    });
                }
            }
        }

        let results = self
            .backend
            .batch_update_blobs(blobs)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let _enter = span.enter();
        tracing::info!(
            request_count,
            accepted = accepted_indices.len(),
            rejected = request_count - accepted_indices.len(),
        );
        for (idx, u) in accepted_indices.into_iter().zip(results) {
            responses[idx].status = Some(match u.status {
                Ok(()) => brokkr_proto::rpc::Status {
                    code: 0,
                    message: String::new(),
                    details: vec![],
                },
                Err(msg) => brokkr_proto::rpc::Status {
                    // INVALID_ARGUMENT — backend re-verification failed
                    // (size limit, partial write, etc.).
                    code: 3,
                    message: msg,
                    details: vec![],
                },
            });
        }
        Ok(Response::new(rapi::BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        request: Request<rapi::BatchReadBlobsRequest>,
    ) -> Result<Response<rapi::BatchReadBlobsResponse>, Status> {
        let span = tracing::info_span!("cas::batch_read_blobs");
        let req = request.into_inner();
        validate_instance_name(&req.instance_name)?;
        let digest_count = req.digests.len();
        let digests: Vec<super::Digest> = req
            .digests
            .iter()
            .map(proto_to_digest)
            .collect::<Result<_, _>>()?;
        let _enter = span.enter();
        let results = self
            .backend
            .batch_read_blobs(&digests)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        tracing::info!(digest_count);
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use brokkr_cas::InMemoryCas;
    use brokkr_proto::reapi_v2 as rapi;
    use rapi::content_addressable_storage_server::ContentAddressableStorage as _;
    use tonic::{Code, Request};

    use super::CasService;

    fn service() -> CasService<InMemoryCas> {
        CasService::new(Arc::new(InMemoryCas::new()))
    }

    #[tokio::test]
    async fn find_missing_blobs_rejects_named_instance() {
        let req = rapi::FindMissingBlobsRequest {
            instance_name: "tenant-a".to_string(),
            ..Default::default()
        };
        let err = service()
            .find_missing_blobs(Request::new(req))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn find_missing_blobs_accepts_default_instance() {
        // Empty instance_name is the single Phase-1 instance — must succeed.
        let resp = service()
            .find_missing_blobs(Request::new(rapi::FindMissingBlobsRequest::default()))
            .await
            .unwrap();
        assert!(resp.into_inner().missing_blob_digests.is_empty());
    }

    #[tokio::test]
    async fn batch_read_blobs_rejects_named_instance() {
        let req = rapi::BatchReadBlobsRequest {
            instance_name: "tenant-a".to_string(),
            ..Default::default()
        };
        let err = service()
            .batch_read_blobs(Request::new(req))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn batch_update_blobs_rejects_named_instance() {
        let req = rapi::BatchUpdateBlobsRequest {
            instance_name: "tenant-a".to_string(),
            ..Default::default()
        };
        let err = service()
            .batch_update_blobs(Request::new(req))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }
}
