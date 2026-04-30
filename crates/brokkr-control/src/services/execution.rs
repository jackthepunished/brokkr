//! REAPI `Execution` service. Uses the scheduler to dispatch actions to a
//! worker and stream back `google.longrunning.Operation` updates.

use std::sync::Arc;

use brokkr_proto::reapi_v2::{self as rapi, execution_server::Execution as ExecSvc};
use prost::Message;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::Instrument;

use super::proto_to_digest;
use crate::scheduler::Scheduler;

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
    ///
    /// The [`Scheduler`] is clonable via `Arc` so multiple gRPC handler threads
    /// can share the same scheduler instance. The service does not take
    /// ownership of the scheduler — it keeps an `Arc<Scheduler>` internally.
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
        let req = request.into_inner();
        let action_digest_proto = req
            .action_digest
            .ok_or_else(|| Status::invalid_argument("missing action_digest"))?;
        let action_digest = proto_to_digest(&action_digest_proto)?;
        let skip_cache_lookup = req.skip_cache_lookup;

        let span = tracing::info_span!(
            "execution::execute",
            action_digest = %action_digest,
            skip_cache_lookup,
        );

        let scheduler = self.scheduler.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(4);

        tokio::spawn(
            async move {
                let outcome = scheduler.execute(action_digest, skip_cache_lookup).await;
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
