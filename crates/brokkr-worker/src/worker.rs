//! Worker control loop: register, open the bidi stream, then for each job
//! received run the command and report the result.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use brokkr_common::WorkerId;
use brokkr_proto::brokkr_v1::{
    self as bv1, worker_service_client::WorkerServiceClient, worker_stream_message::Payload,
    HeartbeatRequest, JobResult, RegisterWorkerRequest, WorkerHello as ProtoWorkerHello,
    WorkerId as ProtoWorkerId, WorkerStreamMessage,
};
use brokkr_proto::reapi_v2::{
    self as rapi, batch_update_blobs_request as bur,
    content_addressable_storage_client::ContentAddressableStorageClient,
};
use tokio::sync::mpsc;
use tonic::transport::{Channel, Endpoint};
use tracing::Instrument;

use crate::runner::{proto_digest, run_command, RunOutcome, Runner};

/// TLS configuration for connecting to the control plane.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// CA certificate to verify the server certificate.
    pub ca_cert: PathBuf,
    /// Client certificate for client authentication (mTLS).
    pub client_cert: Option<PathBuf>,
    /// Client private key for client authentication (mTLS).
    pub client_key: Option<PathBuf>,
}

/// Worker daemon configuration.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Endpoint of the brokkr-control gRPC server (the **client** port —
    /// CAS / ActionCache / Capabilities / Execution). The worker uses
    /// this to call `ContentAddressableStorage::BatchUpdateBlobs` for
    /// stdout/stderr uploads.
    pub control_endpoint: String,
    /// Endpoint of the brokkr-control gRPC **worker** port
    /// (`WorkerService`). When unset, the worker falls back to
    /// [`Self::control_endpoint`] (single-port mode). In split-port mode
    /// the operator must point this at the worker port and configure
    /// mTLS on both sides; the worker port requires a client TLS
    /// certificate (ADR 0011, issue #139).
    pub worker_endpoint: Option<String>,
    /// Hostname to advertise (informational).
    pub hostname: String,
    /// How to actually execute each action. Defaults to
    /// [`Runner::Plain`] so in-process tests don't need to build the
    /// `brokkr-sandboxd` runner binary; the CLI binary
    /// (`brokkr-worker`) overrides this to [`Runner::Sandboxed`]
    /// unless `--no-sandbox` is passed.
    pub runner: Runner,
    /// TLS configuration for connecting to the control plane.
    pub tls: Option<TlsConfig>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            control_endpoint: "http://127.0.0.1:7878".to_string(),
            worker_endpoint: None,
            hostname: "worker".to_string(),
            runner: Runner::Plain,
            tls: None,
        }
    }
}

/// Build a gRPC channel to the control plane, optionally with TLS.
async fn build_channel(endpoint: String, tls: Option<&TlsConfig>) -> Result<Channel> {
    let endpoint = Endpoint::from_shared(endpoint.clone())
        .with_context(|| format!("invalid endpoint {:?}", endpoint))?;

    let channel = match tls {
        Some(tls_cfg) => {
            use tonic::transport::ClientTlsConfig;

            let ca_pem = tokio::fs::read(&tls_cfg.ca_cert)
                .await
                .context("reading CA certificate")?;
            let tls_config = ClientTlsConfig::new()
                .ca_certificate(tonic::transport::Certificate::from_pem(ca_pem));

            let tls_config = match (&tls_cfg.client_cert, &tls_cfg.client_key) {
                (Some(_), None) | (None, Some(_)) => {
                    anyhow::bail!(
                        "both client_cert and client_key must be provided together for mTLS; \
                         got client_cert={:?}, client_key={:?}",
                        tls_cfg.client_cert,
                        tls_cfg.client_key
                    );
                }
                (Some(cert_path), Some(key_path)) => {
                    let cert_pem = tokio::fs::read(cert_path)
                        .await
                        .context("reading client certificate")?;
                    let key_pem = tokio::fs::read(key_path)
                        .await
                        .context("reading client key")?;
                    let identity = tonic::transport::Identity::from_pem(cert_pem, key_pem);
                    tls_config.identity(identity)
                }
                _ => tls_config,
            };

            endpoint
                .tls_config(tls_config)
                .context("TLS configuration")?
                .connect()
                .await
                .context("connecting to control plane with TLS")?
        }
        None => endpoint
            .connect()
            .await
            .context("connecting to control plane")?,
    };

    Ok(channel)
}

/// The platform-capability labels a worker advertises at registration so the
/// control plane's constraint matcher can place actions on it: `os` and `arch`
/// from the build target. Richer / configurable capabilities (installed tools,
/// GPU, RAM) are a later increment.
fn default_capability_labels() -> std::collections::HashMap<String, String> {
    let mut labels = std::collections::HashMap::new();
    labels.insert("os".to_string(), std::env::consts::OS.to_string());
    labels.insert("arch".to_string(), std::env::consts::ARCH.to_string());
    labels
}

/// Run the worker. Returns when the control plane closes the stream or an
/// unrecoverable error occurs.
#[tracing::instrument(name = "worker::run", skip(cfg))]
pub async fn run_worker(cfg: WorkerConfig) -> Result<()> {
    // Resolve the worker port. When `worker_endpoint` is unset, fall back
    // to `control_endpoint` (single-port mode — the worker port and the
    // client port are the same listener).
    let worker_url = cfg
        .worker_endpoint
        .clone()
        .unwrap_or_else(|| cfg.control_endpoint.clone());

    // Fail fast: the server's worker port requires a client TLS cert
    // (ADR 0011, issue #139). If the operator pointed the worker at an
    // https endpoint but did not configure --client-cert, every
    // Register/Heartbeat/Stream call would fail at the TLS handshake.
    let has_client_cert = cfg
        .tls
        .as_ref()
        .and_then(|t| t.client_cert.as_ref())
        .is_some();
    if worker_url.starts_with("https://") && !has_client_cert {
        anyhow::bail!(
            "control plane worker endpoint is https but no --client-cert/--client-key configured; \
             refusing to start (would fail every Register/Heartbeat/Stream with TLS handshake error — issue #139)"
        );
    }

    // Worker channel → WorkerServiceClient (worker port, mTLS required in
    // production).
    let worker_ch = build_channel(worker_url, cfg.tls.as_ref()).await?;
    let mut wsc = WorkerServiceClient::new(worker_ch);

    // CAS channel → ContentAddressableStorageClient (client port).
    let cas_ch = build_channel(cfg.control_endpoint.clone(), cfg.tls.as_ref()).await?;
    let cas = ContentAddressableStorageClient::new(cas_ch);

    let reg = wsc
        .register(RegisterWorkerRequest {
            hostname: cfg.hostname.clone(),
            labels: default_capability_labels(),
        })
        .await?
        .into_inner();
    let proto_worker_id = reg
        .worker_id
        .ok_or_else(|| anyhow!("control plane returned no worker_id"))?;
    let worker_id = WorkerId::new(proto_worker_id.id.clone())
        .map_err(|e| anyhow!("invalid worker id from control plane: {e}"))?;
    tracing::info!(worker_id = %worker_id, "worker registered");

    // Background heartbeat: prove liveness on the cadence the control plane
    // advertised so the registry doesn't evict us. On `known=false` the
    // control plane has already evicted us (missed heartbeats) — stop pinging.
    let heartbeat_secs = reg.heartbeat_seconds.max(1) as u64;
    let mut hb_client = wsc.clone();
    let hb_worker_id = worker_id.as_str().to_string();
    let heartbeat_task = tokio::spawn(
        async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(heartbeat_secs));
            // Registration already counts as the first heartbeat.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match hb_client
                    .heartbeat(HeartbeatRequest {
                        worker_id: Some(ProtoWorkerId {
                            id: hb_worker_id.clone(),
                        }),
                    })
                    .await
                {
                    Ok(resp) => {
                        if !resp.into_inner().known {
                            tracing::warn!(
                                "control plane no longer recognises this worker; \
                                 stopping heartbeat (re-registration required)"
                            );
                            // TODO(brokkr-410): re-register and re-open the job
                            // stream instead of just stopping the heartbeat loop.
                            break;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "heartbeat RPC failed"),
                }
            }
        }
        .in_current_span(),
    );

    // Outbound channel: hello + job results.
    let (tx, rx) = mpsc::channel::<WorkerStreamMessage>(8);
    tx.send(WorkerStreamMessage {
        payload: Some(Payload::Hello(ProtoWorkerHello {
            worker_id: Some(ProtoWorkerId {
                id: worker_id.as_str().to_string(),
            }),
        })),
    })
    .await
    .map_err(|_| anyhow!("worker stream send failed"))?;

    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut inbound = wsc.stream(outbound).await?.into_inner();

    while let Some(assignment) = inbound.message().await? {
        let Some(job) = assignment.job else { continue };
        let job_id = job.job_id.clone();
        let mut cas_for_job = cas.clone();
        let report = match handle_job(&cfg.runner, &mut cas_for_job, job).await {
            Ok(r) => JobResult {
                job_id: job_id.clone(),
                result: Some(r),
                cache_hit: false,
                error_message: String::new(),
            },
            Err(e) => JobResult {
                job_id: job_id.clone(),
                result: None,
                cache_hit: false,
                error_message: e.to_string(),
            },
        };
        if tx
            .send(WorkerStreamMessage {
                payload: Some(Payload::Result(report)),
            })
            .await
            .is_err()
        {
            break;
        }
    }
    heartbeat_task.abort();
    Ok(())
}

#[tracing::instrument(
    name = "worker::run_action",
    skip(runner, cas, job),
    fields(
        job_id = %job.job_id,
        argv0 = tracing::field::Empty,
        exit_code = tracing::field::Empty,
    ),
)]
async fn handle_job(
    runner: &Runner,
    cas: &mut ContentAddressableStorageClient<Channel>,
    job: bv1::Job,
) -> Result<rapi::ActionResult> {
    let command = job.command.ok_or_else(|| anyhow!("Job missing Command"))?;
    if let Some(argv0) = command.arguments.first() {
        tracing::Span::current().record("argv0", argv0.as_str());
    }
    let RunOutcome {
        exit_code,
        stdout,
        stderr,
    } = run_command(runner, &command).await?;
    tracing::Span::current().record("exit_code", exit_code);

    // Phase 1 stdout/stderr policy: upload to CAS and reference by digest;
    // also keep a bounded inline copy on the ActionResult for quick CLI
    // display. (REAPI allows either inline or CAS-only.)
    let stdout_digest = proto_digest(&stdout);
    let stderr_digest = proto_digest(&stderr);
    cas.batch_update_blobs(rapi::BatchUpdateBlobsRequest {
        instance_name: String::new(),
        requests: vec![
            bur::Request {
                digest: Some(stdout_digest.clone()),
                data: stdout.to_vec(),
                compressor: 0,
            },
            bur::Request {
                digest: Some(stderr_digest.clone()),
                data: stderr.to_vec(),
                compressor: 0,
            },
        ],
        digest_function: 0,
    })
    .await?;

    Ok(rapi::ActionResult {
        stdout_raw: stdout.to_vec(),
        stderr_raw: stderr.to_vec(),
        stdout_digest: Some(stdout_digest),
        stderr_digest: Some(stderr_digest),
        exit_code,
        ..Default::default()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn capability_labels_include_os_and_arch() {
        let labels = default_capability_labels();
        assert_eq!(
            labels.get("os").map(String::as_str),
            Some(std::env::consts::OS)
        );
        assert_eq!(
            labels.get("arch").map(String::as_str),
            Some(std::env::consts::ARCH)
        );
        // On the supported worker target these are non-empty.
        assert!(!labels.get("os").unwrap().is_empty());
        assert!(!labels.get("arch").unwrap().is_empty());
    }
}
