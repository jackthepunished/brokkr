//! `brokkr.v1.WorkerService` server: registers workers and runs the bidi
//! job-dispatch stream. Phase 1 only supports a single worker at a time.

use std::sync::Arc;

use brokkr_proto::brokkr_v1::{
    self as bv1, worker_service_server::WorkerService, JobAssignment, RegisterWorkerRequest,
    RegisterWorkerResponse, WorkerId, WorkerStreamMessage,
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
        let worker_id = uuid::Uuid::new_v4().to_string();
        Ok(Response::new(RegisterWorkerResponse {
            worker_id: Some(WorkerId { id: worker_id }),
            heartbeat_seconds: 30,
        }))
    }

    type StreamStream = ReceiverStream<Result<JobAssignment, Status>>;
    async fn stream(
        &self,
        request: Request<Streaming<WorkerStreamMessage>>,
    ) -> Result<Response<Self::StreamStream>, Status> {
        let mut inbound = request.into_inner();
        let scheduler = self.scheduler.clone();
        let mut job_rx = scheduler
            .take_receiver()
            .await
            .ok_or_else(|| Status::resource_exhausted("worker stream already claimed"))?;

        let (out_tx, out_rx) = mpsc::channel(4);

        // Inbound pump: read Hello (ignored beyond presence) and JobResults.
        let scheduler_in = scheduler.clone();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                match msg.payload {
                    Some(bv1::worker_stream_message::Payload::Hello(_)) => {
                        tracing::debug!("worker stream: hello received");
                    }
                    Some(bv1::worker_stream_message::Payload::Result(result)) => {
                        scheduler_in.report(result).await;
                    }
                    None => {}
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
