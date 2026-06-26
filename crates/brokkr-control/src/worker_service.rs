//! `brokkr.v1.WorkerService` server: registers workers and runs the bidi
//! job-dispatch stream. Phase 1 only supports a single worker at a time.

use std::sync::Arc;
use std::time::Instant;

use brokkr_common::WorkerId;
use brokkr_proto::brokkr_v1::{
    self as bv1, worker_service_server::WorkerService, HeartbeatRequest, HeartbeatResponse,
    JobAssignment, RegisterWorkerRequest, RegisterWorkerResponse, WorkerId as ProtoWorkerId,
    WorkerStreamMessage,
};
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::registry::{WorkerCapabilities, WorkerRegistry};
use crate::scheduler::Scheduler;

/// Shared, mutex-guarded [`WorkerRegistry`] handle.
///
/// The registry is mutated by the `register` / heartbeat RPC handlers and a
/// background eviction tick, so it lives behind an async `Mutex` shared via
/// `Arc`. Cloning the handle is a cheap `Arc` bump.
pub type SharedWorkerRegistry = Arc<Mutex<WorkerRegistry>>;

/// `brokkr.v1.WorkerService` implementation backed by [`Scheduler`].
pub struct WorkerServiceImpl {
    scheduler: Arc<Scheduler>,
    registry: SharedWorkerRegistry,
}

impl WorkerServiceImpl {
    /// Bind the service to a scheduler, creating a fresh default-policy
    /// [`WorkerRegistry`].
    pub fn new(scheduler: Arc<Scheduler>) -> Self {
        Self::with_registry(scheduler, Arc::new(Mutex::new(WorkerRegistry::default())))
    }

    /// Bind the service to a scheduler and an externally-owned worker
    /// registry, so a background eviction task (or a test) can share the same
    /// handle.
    pub fn with_registry(scheduler: Arc<Scheduler>, registry: SharedWorkerRegistry) -> Self {
        Self {
            scheduler,
            registry,
        }
    }

    /// The shared worker registry handle. Used by the (forthcoming) heartbeat
    /// handler, the eviction tick, and tests.
    pub fn registry(&self) -> SharedWorkerRegistry {
        self.registry.clone()
    }
}

#[tonic::async_trait]
impl WorkerService for WorkerServiceImpl {
    #[tracing::instrument(
        name = "worker_service::register",
        skip(self, request),
        fields(worker_id = tracing::field::Empty),
    )]
    async fn register(
        &self,
        request: Request<RegisterWorkerRequest>,
    ) -> Result<Response<RegisterWorkerResponse>, Status> {
        let req = request.into_inner();
        let worker_id = WorkerId::new(uuid::Uuid::new_v4().to_string())
            .map_err(|e| Status::internal(format!("invalid worker id: {e}")))?;
        tracing::Span::current().record("worker_id", worker_id.as_str());

        let capabilities = WorkerCapabilities {
            hostname: req.hostname,
            labels: req.labels.into_iter().collect(),
        };

        // Advertise the cadence the eviction policy actually expects, so a
        // worker that honours it is never evicted while healthy. Registration
        // counts as the worker's first heartbeat.
        let heartbeat_seconds = {
            let mut registry = self.registry.lock().await;
            registry.register(worker_id.clone(), capabilities, Instant::now());
            registry.policy().interval.as_secs() as u32
        };

        tracing::info!(heartbeat_seconds, "worker registered");
        Ok(Response::new(RegisterWorkerResponse {
            worker_id: Some(ProtoWorkerId {
                id: worker_id.into_string(),
            }),
            heartbeat_seconds,
        }))
    }

    #[tracing::instrument(
        name = "worker_service::heartbeat",
        skip(self, request),
        fields(worker_id = tracing::field::Empty, known = tracing::field::Empty),
    )]
    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let req = request.into_inner();
        let worker_id_proto = req
            .worker_id
            .ok_or_else(|| Status::invalid_argument("HeartbeatRequest.worker_id missing"))?;
        let worker_id = WorkerId::new(worker_id_proto.id)
            .map_err(|e| Status::invalid_argument(format!("invalid worker id: {e}")))?;
        tracing::Span::current().record("worker_id", worker_id.as_str());

        // An unknown worker is not an error — the registry may have evicted it
        // after missed heartbeats, or it never registered. Reply `known=false`
        // so the worker re-registers rather than retrying a dead identity.
        let known = {
            let mut registry = self.registry.lock().await;
            registry
                .record_heartbeat(&worker_id, Instant::now())
                .is_ok()
        };
        tracing::Span::current().record("known", known);
        if !known {
            tracing::warn!("heartbeat from unknown worker; signalling re-register");
        }
        Ok(Response::new(HeartbeatResponse { known }))
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use brokkr_cas::{ActionCache, CasError, InMemoryCas};
    use brokkr_common::Digest;
    use brokkr_proto::reapi_v2::ActionResult;

    use super::*;

    /// Minimal `ActionCache` so we can build a `Scheduler` — the register
    /// handler never touches it, it just has to exist to construct the service.
    struct NoopActionCache;

    #[async_trait]
    impl ActionCache for NoopActionCache {
        async fn get_action_result(&self, _d: &Digest) -> Result<Option<ActionResult>, CasError> {
            Ok(None)
        }
        async fn update_action_result(
            &self,
            _d: &Digest,
            _r: ActionResult,
        ) -> Result<(), CasError> {
            Ok(())
        }
    }

    fn service() -> WorkerServiceImpl {
        let scheduler = Scheduler::new(Arc::new(InMemoryCas::new()), Arc::new(NoopActionCache));
        WorkerServiceImpl::new(scheduler)
    }

    #[tokio::test]
    async fn register_persists_capabilities_into_registry() {
        let svc = service();
        let mut labels = HashMap::new();
        labels.insert("os".to_string(), "linux".to_string());
        labels.insert("arch".to_string(), "x86_64".to_string());

        let resp = svc
            .register(Request::new(RegisterWorkerRequest {
                hostname: "smith-01".to_string(),
                labels,
            }))
            .await
            .unwrap()
            .into_inner();

        // A valid, non-empty id is returned, and the advertised heartbeat
        // matches the registry's default policy interval (5s).
        let id_str = resp.worker_id.unwrap().id;
        assert!(!id_str.is_empty());
        assert_eq!(resp.heartbeat_seconds, 5);

        // The worker is now recorded with its declared capabilities.
        let worker_id = WorkerId::new(id_str).unwrap();
        let registry = svc.registry();
        let guard = registry.lock().await;
        let record = guard.get(&worker_id).unwrap();
        assert_eq!(record.capabilities.hostname, "smith-01");
        assert_eq!(
            record.capabilities.labels.get("os").map(String::as_str),
            Some("linux")
        );
        assert_eq!(
            record.capabilities.labels.get("arch").map(String::as_str),
            Some("x86_64")
        );
        assert_eq!(guard.len(), 1);
    }

    #[tokio::test]
    async fn each_register_gets_a_distinct_id() {
        let svc = service();
        let r1 = svc
            .register(Request::new(RegisterWorkerRequest::default()))
            .await
            .unwrap()
            .into_inner();
        let r2 = svc
            .register(Request::new(RegisterWorkerRequest::default()))
            .await
            .unwrap()
            .into_inner();
        assert_ne!(r1.worker_id.unwrap().id, r2.worker_id.unwrap().id);
        assert_eq!(svc.registry().lock().await.len(), 2);
    }

    #[tokio::test]
    async fn heartbeat_after_register_is_known() {
        let svc = service();
        let id = svc
            .register(Request::new(RegisterWorkerRequest::default()))
            .await
            .unwrap()
            .into_inner()
            .worker_id
            .unwrap();

        let resp = svc
            .heartbeat(Request::new(HeartbeatRequest {
                worker_id: Some(id),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.known);
    }

    #[tokio::test]
    async fn heartbeat_for_unknown_worker_is_not_known() {
        let svc = service();
        let resp = svc
            .heartbeat(Request::new(HeartbeatRequest {
                worker_id: Some(ProtoWorkerId {
                    id: "never-registered".to_string(),
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!resp.known);
    }

    #[tokio::test]
    async fn heartbeat_with_missing_worker_id_is_invalid_argument() {
        let svc = service();
        let status = svc
            .heartbeat(Request::new(HeartbeatRequest { worker_id: None }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }
}
