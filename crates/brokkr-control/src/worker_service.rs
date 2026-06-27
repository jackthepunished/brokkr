//! `brokkr.v1.WorkerService` server: registers workers and runs the bidi
//! job-dispatch stream. Each connected worker is registered in the scheduler's
//! [`ConnectedWorkers`](crate::scheduling::ConnectedWorkers) (keyed by the
//! `worker_id` from its `Hello`) with its own job channel, so the scheduler can
//! route jobs to a specific worker (ADR 0008).

use std::sync::Arc;
use std::time::{Duration, Instant};

use brokkr_common::WorkerId;
use brokkr_proto::brokkr_v1::{
    self as bv1, worker_service_server::WorkerService, HeartbeatRequest, HeartbeatResponse,
    JobAssignment, RegisterWorkerRequest, RegisterWorkerResponse, WorkerId as ProtoWorkerId,
    WorkerStreamMessage,
};
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::Instrument;

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

    /// The shared worker registry handle. Used by the heartbeat handler, the
    /// eviction tick, and tests.
    pub fn registry(&self) -> SharedWorkerRegistry {
        self.registry.clone()
    }
}

/// Spawn the background liveness reaper: every registry-policy interval it
/// evicts workers that have missed too many heartbeats.
///
/// This is just the periodic driver — the eviction *decision* lives in
/// [`WorkerRegistry::evict_stale`] (unit-tested with an injected clock). Wire
/// this into the control-plane binary; hold the returned handle for the
/// server's lifetime (dropping/aborting it stops the reaper).
pub fn spawn_eviction_task(registry: SharedWorkerRegistry) -> tokio::task::JoinHandle<()> {
    tokio::spawn(
        async move {
            // Tick at the heartbeat interval; the deadline (interval *
            // max_missed) is enforced inside `evict_stale`, so checking once
            // per interval bounds eviction lag to one interval.
            let interval = registry.lock().await.policy().interval;
            // `tokio::time::interval` panics on a zero period; a zero-interval
            // policy disables the reaper rather than crashing the server.
            if interval == Duration::ZERO {
                tracing::warn!("eviction reaper disabled (heartbeat interval is zero)");
                return;
            }
            let mut ticker = tokio::time::interval(interval);
            // Drop the immediate first tick so freshly-registered workers get
            // a full interval before the first sweep.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let evicted = {
                    let mut reg = registry.lock().await;
                    reg.evict_stale(Instant::now())
                };
                if !evicted.is_empty() {
                    tracing::info!(count = evicted.len(), "evicted stale workers");
                }
            }
        }
        .in_current_span(),
    )
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

        // Outbound stream handed back to the worker (JobAssignments). We must
        // return it now, but the worker id is only known from the first inbound
        // message (Hello) — so the per-worker channel registration happens in
        // the spawned pump once Hello arrives.
        let (out_tx, out_rx) = mpsc::channel::<Result<JobAssignment, Status>>(4);

        tokio::spawn(
            async move {
                // The first message must be Hello carrying the worker id.
                let worker_id = match inbound.message().await {
                    Ok(Some(msg)) => match msg.payload {
                        Some(bv1::worker_stream_message::Payload::Hello(hello)) => {
                            match hello.worker_id.and_then(|w| WorkerId::new(w.id).ok()) {
                                Some(id) => id,
                                None => {
                                    tracing::error!(
                                        "worker stream: Hello missing/invalid worker_id — closing"
                                    );
                                    return;
                                }
                            }
                        }
                        _ => {
                            tracing::error!("worker stream: first message was not Hello — closing");
                            return;
                        }
                    },
                    Ok(None) => {
                        tracing::info!("worker stream: closed before Hello");
                        return;
                    }
                    Err(status) => {
                        tracing::error!(
                            code = ?status.code(),
                            "worker stream: transport error before Hello"
                        );
                        return;
                    }
                };
                tracing::info!(worker_id = %worker_id, "worker stream connected");

                // This worker's own job channel. Spawn the outbound pump first,
                // then register with the scheduler — `connect_worker` may
                // immediately dispatch a queued job, and the pump must be ready
                // to forward it.
                let (job_tx, mut job_rx) = mpsc::channel::<bv1::Job>(8);
                let outbound = tokio::spawn(async move {
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
                scheduler.connect_worker(worker_id.clone(), job_tx).await;

                // Inbound pump: process JobResults until the stream ends. Each
                // terminal state is logged (issue #64) so an operator can tell
                // why the pump stopped.
                loop {
                    match inbound.message().await {
                        Ok(Some(msg)) => match msg.payload {
                            Some(bv1::worker_stream_message::Payload::Hello(_)) => {
                                tracing::debug!(worker_id = %worker_id, "duplicate hello ignored");
                            }
                            Some(bv1::worker_stream_message::Payload::Result(result)) => {
                                if let Err(e) = scheduler.report(result).await {
                                    tracing::error!(error = %e, "invalid job_id in worker result");
                                }
                            }
                            None => {
                                tracing::warn!("worker stream: message with no payload");
                            }
                        },
                        Ok(None) => {
                            tracing::info!(worker_id = %worker_id, "worker stream: closed cleanly");
                            break;
                        }
                        Err(status) => {
                            tracing::error!(
                                worker_id = %worker_id,
                                code = ?status.code(),
                                message = status.message(),
                                "worker stream: transport error — pump exiting; in-flight \
                                 jobs for this worker time out via the scheduler timeout"
                            );
                            break;
                        }
                    }
                }

                // Disconnect: deregister and requeue any job this worker held
                // for reassignment to another worker (ADR 0009 crash recovery),
                // then stop the outbound pump (closing the worker's stream).
                scheduler.disconnect_worker(&worker_id).await;
                outbound.abort();
            }
            .in_current_span(),
        );

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

    /// End-to-end through the RPC surface, deterministically (no timers):
    /// register → evict via the shared registry with an injected future
    /// instant → a subsequent heartbeat reports `known=false`. Proves the
    /// register handler, the registry, and the heartbeat handler all share one
    /// source of truth and that eviction is observable to the worker.
    #[tokio::test]
    async fn eviction_is_observable_via_heartbeat() {
        let svc = service();
        let id = svc
            .register(Request::new(RegisterWorkerRequest::default()))
            .await
            .unwrap()
            .into_inner()
            .worker_id
            .unwrap();

        // Force eviction by sweeping with an instant well past the deadline.
        let deadline = svc.registry().lock().await.policy().deadline();
        let future = Instant::now() + deadline + Duration::from_secs(1);
        let evicted = svc.registry().lock().await.evict_stale(future);
        assert_eq!(evicted.len(), 1);

        let resp = svc
            .heartbeat(Request::new(HeartbeatRequest {
                worker_id: Some(id),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!resp.known, "evicted worker should be told to re-register");
    }
}
