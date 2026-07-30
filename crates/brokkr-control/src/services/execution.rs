//! REAPI `Execution` service. Uses the scheduler to dispatch actions to a
//! worker and stream back `google.longrunning.Operation` updates.

use std::sync::Arc;

use brokkr_common::TenantId;
use brokkr_proto::reapi_v2::{self as rapi, execution_server::Execution as ExecSvc};
use prost::Message;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::Instrument;

use super::{proto_to_digest, validate_instance_name};
use crate::scheduler::{ExecutionError, Scheduler};

/// Tenant from the `x-brokkr-tenant` request metadata header, defaulting when
/// the header is absent or malformed (ADR 0010). This is client-asserted until
/// auth (plan §16 task 8) makes the identity authoritative.
fn tenant_from_metadata(md: &tonic::metadata::MetadataMap) -> TenantId {
    md.get("x-brokkr-tenant")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| TenantId::new(s.to_string()).ok())
        .unwrap_or_default()
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

/// REAPI `Execution` service. Uses the scheduler to dispatch actions to a
/// worker and stream back `google.longrunning.Operation` updates.
pub struct ExecutionService {
    scheduler: Arc<Scheduler>,
}

impl ExecutionService {
    /// Bind the service to a scheduler.
    pub fn new(scheduler: Arc<Scheduler>) -> Self {
        Self { scheduler }
    }
}

#[tonic::async_trait]
impl ExecSvc for ExecutionService {
    type ExecuteStream = ReceiverStream<Result<brokkr_proto::longrunning::Operation, Status>>;
    async fn execute(
        &self,
        request: Request<rapi::ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        // Prefer the authoritative tenant injected by the auth interceptor
        // (ADR 0011); fall back to the client-asserted header in open mode.
        let tenant = request
            .extensions()
            .get::<TenantId>()
            .cloned()
            .unwrap_or_else(|| tenant_from_metadata(request.metadata()));
        let req = request.into_inner();
        validate_instance_name(&req.instance_name)?;
        let action_digest_proto = req
            .action_digest
            .ok_or_else(|| Status::invalid_argument("missing action_digest"))?;
        let action_digest = proto_to_digest(&action_digest_proto)?;
        let skip_cache_lookup = req.skip_cache_lookup;

        let span = tracing::info_span!(
            "execution::execute",
            action_digest = %action_digest,
            tenant = %tenant,
            skip_cache_lookup,
        );

        let scheduler = self.scheduler.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(4);

        tokio::spawn(
            async move {
                let outcome = scheduler
                    .execute(action_digest, skip_cache_lookup, tenant)
                    .await;
                let op = match outcome {
                    Ok(o) => {
                        // D1 (I9b): a successful action whose cache write was
                        // refused because this node is not the metadata leader
                        // still returns its real result. Say so in the REAPI
                        // `message` field — the client asked for a build and
                        // got one, but it paid full price and the next
                        // identical build will too, and that should not be
                        // discoverable only by reading server logs.
                        let message = if o.result_cached {
                            String::new()
                        } else {
                            "result not cached: this control-plane node is not the \
                             metadata leader; an identical action will re-execute"
                                .to_string()
                        };
                        let resp = rapi::ExecuteResponse {
                            result: Some(o.result),
                            cached_result: o.cache_hit,
                            status: Some(brokkr_proto::rpc::Status::default()),
                            message,
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
                    Err(e) => {
                        // DEADLINE_EXCEEDED for scheduler timeouts (issue
                        // #63); FAILED_PRECONDITION when no worker can run the
                        // action; INTERNAL for everything else. The code lets
                        // clients implement retry policies without parsing the
                        // error string.
                        let code = match &e {
                            ExecutionError::Timeout(_) => 4,
                            ExecutionError::QuotaExceeded(_) => 8,
                            ExecutionError::NoEligibleWorker => 9,
                            ExecutionError::Other(_) => 13,
                        };
                        brokkr_proto::longrunning::Operation {
                            name: format!("operations/{}", uuid::Uuid::new_v4()),
                            done: true,
                            result: Some(brokkr_proto::longrunning::operation::Result::Error(
                                brokkr_proto::rpc::Status {
                                    code,
                                    message: e.to_string(),
                                    details: vec![],
                                },
                            )),
                            ..Default::default()
                        }
                    }
                };
                let _ = tx.send(Ok(op)).await;
            }
            .instrument(span),
        );

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
