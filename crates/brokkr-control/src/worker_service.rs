//! `brokkr.v1.WorkerService` server: registers workers and runs the bidi
//! job-dispatch stream. Phase 1 only supports a single worker at a time.

use std::sync::Arc;

use brokkr_common::WorkerId;
use brokkr_proto::brokkr_v1::{
    self as bv1, worker_service_server::WorkerService, JobAssignment, RegisterWorkerRequest,
    RegisterWorkerResponse, WorkerId as ProtoWorkerId, WorkerStreamMessage,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::scheduler::Scheduler;

/// `brokkr.v1.WorkerService` implementation backed by [`Scheduler`].
pub struct WorkerServiceImpl {
    scheduler: Arc<Scheduler>,
}

impl WorkerServiceImpl {
    /// Bind the service to a scheduler.
    pub fn new(scheduler: Arc<Scheduler>) -> Self {
        Self { scheduler }
    }
}

#[tonic::async_trait]
impl WorkerService for WorkerServiceImpl {
    async fn register(
        &self,
        _request: Request<RegisterWorkerRequest>,
    ) -> Result<Response<RegisterWorkerResponse>, Status> {
        let span = tracing::info_span!("worker_service::register");
        let _guard = span.enter();
        let worker_id = WorkerId::new(uuid::Uuid::new_v4().to_string())
            .map_err(|e| Status::internal(format!("invalid worker id: {e}")))?;
        Ok(Response::new(RegisterWorkerResponse {
            worker_id: Some(ProtoWorkerId {
                id: worker_id.into_string(),
            }),
            heartbeat_seconds: 30,
        }))
    }

    type StreamStream = ReceiverStream<Result<JobAssignment, Status>>;
    async fn stream(
        &self,
        request: Request<Streaming<WorkerStreamMessage>>,
    ) -> Result<Response<Self::StreamStream>, Status> {
        let span = tracing::info_span!("worker_service::stream");
        let _guard = span.enter();
        let mut inbound = request.into_inner();
        let scheduler = self.scheduler.clone();
        let mut job_rx = scheduler
            .take_receiver()
            .await
            .ok_or_else(|| Status::resource_exhausted("worker stream already claimed"))?;

        let (out_tx, out_rx) = mpsc::channel(4);

        // Inbound pump: read Hello (ignored beyond presence) and JobResults.
        // Issue #64: previously this loop pattern-matched `Ok(Some(msg))`
        // and silently exited on `Ok(None)` (clean stream end) and on `Err`
        // (transport error, decode failure, peer reset). The worker then
        // appeared to function while its stream was broken. We now log
        // each terminal state at the appropriate level so an operator
        // monitoring the control plane can tell why the loop quit.
        let scheduler_in = scheduler.clone();
        tokio::spawn(async move {
            loop {
                match inbound.message().await {
                    Ok(Some(msg)) => match msg.payload {
                        Some(bv1::worker_stream_message::Payload::Hello(_)) => {
                            tracing::debug!("worker stream: hello received");
                        }
                        Some(bv1::worker_stream_message::Payload::Result(result)) => {
                            if let Err(e) = scheduler_in.report(result).await {
                                tracing::error!(
                                    error = %e,
                                    "invalid job_id in worker result"
                                );
                            }
                        }
                        None => {
                            tracing::warn!(
                                "worker stream: received WorkerStreamMessage with no payload"
                            );
                        }
                    },
                    Ok(None) => {
                        tracing::info!(
                            "worker stream: closed cleanly by the worker — pump exiting"
                        );
                        break;
                    }
                    Err(status) => {
                        tracing::error!(
                            code = ?status.code(),
                            message = status.message(),
                            "worker stream: transport error — pump exiting; \
                             pending jobs for this worker will time out via the \
                             scheduler timeout"
                        );
                        break;
                    }
                }
            }
        });

        // Outbound pump: forward jobs from scheduler to worker.
        tokio::spawn(async move {
            while let Some(job) = job_rx.recv().await {
                if out_tx
                    .send(Ok(JobAssignment { job: Some(job) }))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(out_rx)))
    }
}
