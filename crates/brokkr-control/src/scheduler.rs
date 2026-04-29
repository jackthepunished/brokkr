//! Single-node, single-queue job scheduler for Phase 1.
//!
//! Bridges the REAPI `Execute` RPC (client-facing) to the internal
//! `brokkr.v1.WorkerService.Stream` (worker-facing). Multi-worker fan-out and
//! priority queues are Phase 4.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use brokkr_cas::{ActionCache, Cas};
use brokkr_common::Digest;
use brokkr_proto::brokkr_v1 as bv1;
use brokkr_proto::reapi_v2 as rapi;
use prost::Message;
use tokio::sync::{mpsc, oneshot, Mutex};

/// Outcome of a scheduled action execution.
pub struct ExecutionOutcome {
    /// REAPI ActionResult to return to the caller.
    pub result: rapi::ActionResult,
    /// True if the action cache was hit and execution skipped.
    pub cache_hit: bool,
}

/// Single-queue in-process job scheduler.
pub struct Scheduler {
    queue_tx: mpsc::UnboundedSender<bv1::Job>,
    queue_rx: Mutex<Option<mpsc::UnboundedReceiver<bv1::Job>>>,
    waiters: Mutex<HashMap<String, oneshot::Sender<bv1::JobResult>>>,
    cas: Arc<dyn Cas>,
    action_cache: Arc<dyn ActionCache>,
}

impl Scheduler {
    /// Construct a scheduler bound to the given storage backends.
    pub fn new(cas: Arc<dyn Cas>, action_cache: Arc<dyn ActionCache>) -> Arc<Self> {
        let (queue_tx, queue_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            queue_tx,
            queue_rx: Mutex::new(Some(queue_rx)),
            waiters: Mutex::new(HashMap::new()),
            cas,
            action_cache,
        })
    }

    /// Take ownership of the job receiver. Returns `None` after the first call;
    /// only one worker stream is supported in Phase 1.
    pub async fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<bv1::Job>> {
        self.queue_rx.lock().await.take()
    }

    /// Execute an action: look up the action cache, otherwise enqueue a job
    /// and await the worker's report.
    #[tracing::instrument(
        name = "control::dispatch",
        skip(self),
        fields(
            action_digest = %action_digest,
            skip_cache_lookup,
            cache_hit = tracing::field::Empty,
            exit_code = tracing::field::Empty,
            job_id = tracing::field::Empty,
        ),
    )]
    pub async fn execute(
        self: &Arc<Self>,
        action_digest: Digest,
        skip_cache_lookup: bool,
    ) -> Result<ExecutionOutcome> {
        if !skip_cache_lookup {
            if let Some(cached) = self
                .action_cache
                .get_action_result(&action_digest)
                .await
                .map_err(|e| anyhow!("action cache get: {e}"))?
            {
                tracing::Span::current()
                    .record("cache_hit", true)
                    .record("exit_code", cached.exit_code);
                return Ok(ExecutionOutcome {
                    result: cached,
                    cache_hit: true,
                });
            }
        }

        let action = self
            .fetch_message::<rapi::Action>(&action_digest)
            .await
            .with_context(|| "fetching Action from CAS")?;
        let command_digest_proto = action
            .command_digest
            .as_ref()
            .ok_or_else(|| anyhow!("Action.command_digest missing"))?;
        let command_digest = Digest::new(
            command_digest_proto.hash.clone(),
            command_digest_proto.size_bytes,
        )
        .map_err(|e| anyhow!("invalid command digest: {e}"))?;
        let command = self
            .fetch_message::<rapi::Command>(&command_digest)
            .await
            .with_context(|| "fetching Command from CAS")?;

        let job_id = uuid::Uuid::new_v4().to_string();
        tracing::Span::current().record("job_id", job_id.as_str());
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(job_id.clone(), tx);

        let job = bv1::Job {
            job_id: job_id.clone(),
            action_digest: Some(rapi::Digest {
                hash: action_digest.hash().to_string(),
                size_bytes: action_digest.size_bytes(),
            }),
            action: Some(action),
            command: Some(command),
        };
        self.queue_tx
            .send(job)
            .map_err(|_| anyhow!("scheduler queue closed"))?;

        let report = rx
            .await
            .map_err(|_| anyhow!("worker did not report result"))?;
        if !report.error_message.is_empty() {
            return Err(anyhow!("worker error: {}", report.error_message));
        }
        let result = report
            .result
            .ok_or_else(|| anyhow!("worker reported no ActionResult"))?;

        if result.exit_code == 0 {
            self.action_cache
                .update_action_result(&action_digest, result.clone())
                .await
                .map_err(|e| anyhow!("action cache update: {e}"))?;
        }
        tracing::Span::current()
            .record("cache_hit", false)
            .record("exit_code", result.exit_code);
        Ok(ExecutionOutcome {
            result,
            cache_hit: false,
        })
    }

    /// Worker-side entry: receive a job result and wake the matching waiter.
    pub async fn report(&self, result: bv1::JobResult) {
        let waiter = self.waiters.lock().await.remove(&result.job_id);
        if let Some(tx) = waiter {
            // If the receiver dropped (e.g. client cancelled), discard the result.
            let _ = tx.send(result);
        }
    }

    async fn fetch_message<M: Message + Default>(&self, digest: &Digest) -> Result<M> {
        let mut reads = self
            .cas
            .batch_read_blobs(std::slice::from_ref(digest))
            .await
            .map_err(|e| anyhow!("CAS read: {e}"))?;
        let bytes = reads
            .remove(0)
            .map_err(|e| anyhow!("blob {} not in CAS: {e}", digest))?;
        M::decode(bytes.as_ref()).with_context(|| format!("decoding {} from CAS", digest))
    }
}
