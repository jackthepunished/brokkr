//! Single-node, single-queue job scheduler for Phase 1.
//!
//! Bridges the REAPI `Execute` RPC (client-facing) to the internal
//! `brokkr.v1.WorkerService.Stream` (worker-facing). Multi-worker fan-out and
//! priority queues are Phase 4.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use brokkr_cas::{ActionCache, Cas};
use brokkr_common::{Digest, JobId};
use brokkr_proto::brokkr_v1 as bv1;
use brokkr_proto::reapi_v2 as rapi;
use prost::Message;
use tokio::sync::{mpsc, oneshot, Mutex};

/// Outcome of a scheduled action execution.
#[derive(Debug)]
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
    waiters: Mutex<HashMap<JobId, oneshot::Sender<bv1::JobResult>>>,
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
    #[tracing::instrument(name = "scheduler::take_receiver", skip(self))]
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

        let job_id = JobId::new(uuid::Uuid::new_v4().to_string())
            .map_err(|e| anyhow!("invalid job id: {e}"))?;
        tracing::Span::current().record("job_id", job_id.as_str());
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(job_id.clone(), tx);

        let job = bv1::Job {
            job_id: job_id.clone().into_string(),
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
    #[tracing::instrument(name = "scheduler::report", skip(self, result))]
    pub async fn report(&self, result: bv1::JobResult) -> Result<()> {
        let job_id = JobId::new(result.job_id.clone())
            .map_err(|e| anyhow!("invalid job_id in result: {}", e))?;
        let waiter = self.waiters.lock().await.remove(&job_id);
        if let Some(tx) = waiter {
            // If the receiver dropped (e.g. client cancelled), discard the result.
            let _ = tx.send(result);
        }
        Ok(())
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use async_trait::async_trait;
    use brokkr_cas::{ActionCache, Cas, CasError};
    use brokkr_common::Digest;
    use brokkr_proto::reapi_v2::ActionResult;
    use bytes::Bytes;

    use super::*;

    /// Mock `Cas` that returns a configurable error for `batch_read_blobs`.
    struct MockCas {
        /// If `true`, `batch_read_blobs` returns `CasError::NotFound` for all digests.
        /// Otherwise it returns `NotFound` for each digest (simulating a true miss).
        force_not_found: bool,
    }

    #[async_trait]
    impl Cas for MockCas {
        async fn find_missing_blobs(&self, _digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
            Ok(vec![])
        }

        async fn batch_read_blobs(
            &self,
            digests: &[Digest],
        ) -> Result<Vec<Result<Bytes, CasError>>, CasError> {
            if self.force_not_found {
                // Return a real NotFound error for each digest.
                Ok(digests
                    .iter()
                    .map(|d| Err(CasError::NotFound(d.clone())))
                    .collect())
            } else {
                // Simulate a real miss — read a blob that was never written.
                Ok(digests
                    .iter()
                    .map(|_| Err(CasError::NotFound(Digest::of(b"missing"))))
                    .collect())
            }
        }

        async fn batch_update_blobs(
            &self,
            _blobs: Vec<(Digest, Bytes)>,
        ) -> Result<Vec<brokkr_cas::traits::UpdateResult>, CasError> {
            Ok(vec![])
        }
    }

    /// Mock `ActionCache` that returns a configurable error for `get_action_result`.
    struct MockActionCache {
        /// If `true`, `get_action_result` returns an `Io` error; otherwise returns `Ok(None)`.
        force_error: bool,
    }

    #[async_trait]
    impl ActionCache for MockActionCache {
        async fn get_action_result(
            &self,
            _action_digest: &Digest,
        ) -> Result<Option<ActionResult>, CasError> {
            if self.force_error {
                Err(CasError::Io(std::io::Error::other("simulated")))
            } else {
                Ok(None)
            }
        }

        async fn update_action_result(
            &self,
            _action_digest: &Digest,
            _result: ActionResult,
        ) -> Result<(), CasError> {
            Ok(())
        }
    }

    /// Verify `report` returns an error when given an empty job_id string.
    #[tokio::test]
    async fn report_rejects_invalid_job_id() {
        let cas = Arc::new(MockCas {
            force_not_found: true,
        });
        let ac = Arc::new(MockActionCache { force_error: false });
        let scheduler = Scheduler::new(cas, ac);

        let result = bv1::JobResult {
            job_id: String::new(), // empty string is invalid for JobId
            result: None,
            cache_hit: false,
            error_message: String::new(),
        };
        let err = scheduler.report(result).await.unwrap_err();
        assert!(
            err.to_string().contains("invalid job_id"),
            "expected 'invalid job_id' in error, got: {err}"
        );
    }

    /// Verify `execute` propagates CAS `NotFound` errors from `fetch_message`
    /// when the action digest is not in the CAS.
    #[tokio::test]
    async fn execute_returns_err_when_action_not_in_cas() {
        let missing_digest = Digest::of(b"action never stored");
        let cas = Arc::new(MockCas {
            force_not_found: true,
        });
        let ac = Arc::new(MockActionCache { force_error: false });
        let scheduler = Scheduler::new(cas, ac);

        let err = scheduler.execute(missing_digest, false).await.unwrap_err();
        // The NotFound error is wrapped by with_context("fetching Action from CAS").
        assert!(
            err.to_string().contains("fetching Action from CAS"),
            "expected 'fetching Action from CAS' in error, got: {err}"
        );
    }

    /// Verify `execute` propagates action cache get errors.
    #[tokio::test]
    async fn execute_returns_err_when_action_cache_get_fails() {
        let cas = Arc::new(MockCas {
            force_not_found: true,
        });
        let ac = Arc::new(MockActionCache { force_error: true });
        let scheduler = Scheduler::new(cas, ac);

        let err = scheduler
            .execute(Digest::of(b"any action"), false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("action cache get"),
            "expected 'action cache get' in error, got: {err}"
        );
    }
}
