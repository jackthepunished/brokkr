//! Worker control loop: register, open the bidi stream, then for each job
//! received run the command and report the result.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use brokkr_common::WorkerId;
use brokkr_proto::brokkr_v1::{
    self as bv1, worker_service_client::WorkerServiceClient, worker_stream_message::Payload,
    JobResult, RegisterWorkerRequest, WorkerHello as ProtoWorkerHello, WorkerId as ProtoWorkerId,
    WorkerStreamMessage,
};
use brokkr_proto::reapi_v2::{
    self as rapi, batch_update_blobs_request as bur,
    content_addressable_storage_client::ContentAddressableStorageClient,
};
use tokio::sync::mpsc;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

use crate::runner::{proto_digest, run_command, RunOutcome};

/// TLS configuration for worker-to-control-plane connections.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// CA certificate used to verify the server's certificate.
    pub ca_cert: PathBuf,
    /// Client certificate (for mTLS). None = server-only TLS.
    pub client_cert: Option<PathBuf>,
    /// Client private key (for mTLS). None = server-only TLS.
    pub client_key: Option<PathBuf>,
}

/// Worker daemon configuration.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Endpoint of the brokkr-control gRPC server.
    pub control_endpoint: String,
    /// Hostname to advertise (informational).
    pub hostname: String,
    /// TLS configuration. None = plaintext.
    pub tls: Option<TlsConfig>,
}

async fn build_channel(endpoint: &str, tls: Option<TlsConfig>) -> Result<Channel> {
    let mut builder = Endpoint::from_shared(endpoint.trim_end_matches('/').to_string())
        .with_context(|| format!("invalid endpoint {:?}", endpoint))?;
    if let Some(tls_cfg) = tls {
        let ca_pem = std::fs::read_to_string(&tls_cfg.ca_cert)
            .with_context(|| format!("reading CA cert {:?}", tls_cfg.ca_cert))?;
        let mut tls_config = ClientTlsConfig::new()
            .domain_name("localhost")
            .ca_certificate(Certificate::from_pem(ca_pem));
        if let (Some(cert_path), Some(key_path)) = (&tls_cfg.client_cert, &tls_cfg.client_key) {
            let cert_pem = std::fs::read_to_string(cert_path)
                .with_context(|| format!("reading client cert {:?}", cert_path))?;
            let key_pem = std::fs::read_to_string(key_path)
                .with_context(|| format!("reading client key {:?}", key_path))?;
            tls_config = tls_config.identity(Identity::from_pem(cert_pem, key_pem));
        }
        builder = builder.tls_config(tls_config)
            .context("configuring TLS")?;
    }
    builder.connect().await.context("connecting to control plane")
}

/// Run the worker. Returns when the control plane closes the stream or an
/// unrecoverable error occurs.
#[tracing::instrument(name = "worker::run", skip(cfg))]
pub async fn run_worker(cfg: WorkerConfig) -> Result<()> {
    let channel = build_channel(&cfg.control_endpoint, cfg.tls.clone()).await?;

    let mut wsc = WorkerServiceClient::new(channel.clone());
    let cas = ContentAddressableStorageClient::new(channel);

    let reg = wsc
        .register(RegisterWorkerRequest {
            hostname: cfg.hostname.clone(),
            labels: Default::default(),
        })
        .await?
        .into_inner();
    let proto_worker_id = reg
        .worker_id
        .ok_or_else(|| anyhow!("control plane returned no worker_id"))?;
    let worker_id = WorkerId::new(proto_worker_id.id.clone())
        .map_err(|e| anyhow!("invalid worker id from control plane: {e}"))?;
    tracing::info!(worker_id = %worker_id, "worker registered");

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
        let report = match handle_job(&mut cas_for_job, job).await {
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
    Ok(())
}

#[tracing::instrument(
    name = "worker::run_action",
    skip(cas, job),
    fields(
        job_id = %job.job_id,
        argv0 = tracing::field::Empty,
        exit_code = tracing::field::Empty,
    ),
)]
async fn handle_job(
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
    } = run_command(&command).await?;
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
