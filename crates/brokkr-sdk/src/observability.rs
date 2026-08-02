//! Read-only client for `brokkr.v1.ObservabilityService` (ADR 0012).
//!
//! Talks to the control plane's **operator listener**, not the tenant-facing
//! port — see `docs/operations/` and ADR 0012 for why those are separate. The
//! endpoint is whatever `--observe-listen` was set to, which defaults to
//! loopback.
//!
//! Every method is a read. There is nothing here that mutates cluster state,
//! by design.

use brokkr_proto::brokkr_v1 as bv1;
use brokkr_proto::brokkr_v1::observability_service_client::ObservabilityServiceClient;
use futures::Stream;
use tonic::transport::Channel;

use crate::client::ClientError;

/// Connection to a control plane's operator observability listener.
#[derive(Debug, Clone)]
pub struct ObservabilityClient {
    inner: ObservabilityServiceClient<Channel>,
}

impl ObservabilityClient {
    /// Connect to an operator listener, e.g. `http://127.0.0.1:7880`.
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self, ClientError> {
        let endpoint = endpoint.into();
        let channel = Channel::from_shared(endpoint.clone())
            .map_err(|e| ClientError::InvalidEndpoint(format!("{endpoint}: {e}")))?
            .connect()
            .await?;
        Ok(Self {
            inner: ObservabilityServiceClient::new(channel),
        })
    }

    /// Cluster membership, Raft roles, and health.
    ///
    /// Returns `None` only if the server omitted the field, which a
    /// conformant server never does — surfaced rather than substituted so a
    /// protocol mismatch is visible instead of looking like an empty cluster.
    pub async fn get_cluster(&mut self) -> Result<Option<bv1::ClusterInfo>, ClientError> {
        Ok(self
            .inner
            .get_cluster(bv1::GetClusterRequest {})
            .await?
            .into_inner()
            .cluster)
    }

    /// Every worker across every node that answered, each labelled with the
    /// node whose registry holds it.
    pub async fn list_workers(&mut self) -> Result<Vec<bv1::WorkerInfo>, ClientError> {
        Ok(self
            .inner
            .list_workers(bv1::ListWorkersRequest {})
            .await?
            .into_inner()
            .workers)
    }

    /// Recently completed jobs, newest first across the whole cluster.
    ///
    /// `state` is one of `queued`, `running`, `succeeded`, `failed`; anything
    /// else — including empty — means no filter. `limit` of 0 means the
    /// server's default.
    pub async fn list_jobs(
        &mut self,
        state: Option<&str>,
        limit: u32,
    ) -> Result<Vec<bv1::JobInfo>, ClientError> {
        Ok(self
            .inner
            .list_jobs(bv1::ListJobsRequest {
                state_filter: state.unwrap_or_default().to_string(),
                limit,
            })
            .await?
            .into_inner()
            .jobs)
    }

    /// One job by id.
    ///
    /// The history ring is bounded, so a job that has aged out is `NotFound`
    /// rather than an empty reply — "I do not have that job" and "that job had
    /// no data" are different answers.
    pub async fn get_job(
        &mut self,
        job_id: impl Into<String>,
    ) -> Result<bv1::JobInfo, ClientError> {
        self.inner
            .get_job(bv1::GetJobRequest {
                job_id: job_id.into(),
            })
            .await?
            .into_inner()
            .job
            .ok_or_else(|| ClientError::Rpc(tonic::Status::internal("GetJobReply carried no job")))
    }

    /// Scheduling-policy state, **one entry per node**.
    ///
    /// Never a single figure: nodes may have different modules loaded, or
    /// differ in quarantine state, and two nodes disagreeing is information
    /// rather than noise.
    pub async fn get_policy(&mut self) -> Result<Vec<bv1::PolicyInfo>, ClientError> {
        Ok(self
            .inner
            .get_policy(bv1::GetPolicyRequest {})
            .await?
            .into_inner()
            .policies)
    }

    /// CAS size, **one entry per node**.
    ///
    /// Never summed: each node opens its own store, so the same blob on three
    /// nodes is three copies of one blob, and a total would report storage
    /// that does not exist.
    pub async fn get_cas_stats(&mut self) -> Result<Vec<bv1::CasInfo>, ClientError> {
        Ok(self
            .inner
            .get_cas_stats(bv1::GetCasStatsRequest {})
            .await?
            .into_inner()
            .stores)
    }

    /// Subscribe to cluster deltas.
    ///
    /// The **first item is always a full snapshot**, on this and every
    /// reconnect, so a consumer needs no reconciliation logic: replace your
    /// world on each snapshot and apply the deltas between them. If the server
    /// decides you have fallen behind it sends another snapshot rather than
    /// silently skipping deltas.
    pub async fn watch_events(
        &mut self,
    ) -> Result<impl Stream<Item = Result<bv1::ClusterEvent, ClientError>>, ClientError> {
        let stream = self
            .inner
            .watch_events(bv1::WatchEventsRequest {})
            .await?
            .into_inner();
        Ok(futures::StreamExt::map(stream, |item| {
            item.map_err(ClientError::from)
        }))
    }
}
