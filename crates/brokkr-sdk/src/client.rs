//! High-level Brokkr client. Wraps REAPI's CAS + Execution into a single
//! "run this command" call.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use brokkr_proto::reapi_v2::{
    self as rapi, action_cache_client::ActionCacheClient, batch_update_blobs_request as bur,
    content_addressable_storage_client::ContentAddressableStorageClient,
    execution_client::ExecutionClient,
};
use bytes::Bytes;
use prost::Message;
use sha2::{Digest as _, Sha256};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// TLS configuration for mTLS connections.
#[derive(Clone)]
pub struct TlsConfig {
    /// CA certificate used to verify the server's certificate.
    pub ca_cert: PathBuf,
    /// Client certificate to present to the server (for mTLS).
    pub client_cert: Option<PathBuf>,
    /// Client private key (for mTLS).
    pub client_key: Option<PathBuf>,
}

/// Connect with TLS. Pass `tls: None` for plaintext (current default).
async fn connect_tls(endpoint: &str, tls: Option<TlsConfig>) -> Result<Channel> {
    let endpoint = endpoint.trim_end_matches('/');
    let mut builder = Endpoint::from_shared(endpoint.to_string())
        .with_context(|| format!("invalid endpoint {endpoint:?}"))?;

    if let Some(tls_cfg) = tls {
        let ca = Certificate::from_pem(
            std::fs::read_to_string(&tls_cfg.ca_cert)
                .with_context(|| format!("reading CA cert {:?}", tls_cfg.ca_cert))?,
        );
        let mut tls_config = ClientTlsConfig::new()
            // TODO: make domain_name configurable for non-localhost deployments
            .domain_name("localhost")
            .ca_certificate(ca);

        if let (Some(cert_path), Some(key_path)) = (&tls_cfg.client_cert, &tls_cfg.client_key) {
            let cert_pem = std::fs::read_to_string(cert_path)
                .with_context(|| format!("reading client cert {:?}", cert_path))?;
            let key_pem = std::fs::read_to_string(key_path)
                .with_context(|| format!("reading client key {:?}", key_path))?;
            tls_config = tls_config.identity(Identity::from_pem(cert_pem, key_pem));
        }

        builder = builder.tls_config(tls_config).context("configuring TLS")?;
    }

    builder
        .connect()
        .await
        .context("connecting to control plane")
}

/// Client connection to a Brokkr control plane.
#[derive(Clone)]
pub struct BrokkrClient {
    cas: ContentAddressableStorageClient<Channel>,
    exec: ExecutionClient<Channel>,
    #[allow(dead_code)]
    ac: ActionCacheClient<Channel>,
}

impl BrokkrClient {
    /// Connect to the control plane at `endpoint` (e.g.
    /// `http://127.0.0.1:7878`). Uses plaintext; for mTLS use
    /// `BrokkrClient::connect_with_tls`.
    #[tracing::instrument(name = "client::connect", skip(endpoint))]
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        Self::connect_with_tls(endpoint, None).await
    }

    /// Connect with optional TLS configuration.
    #[tracing::instrument(name = "client::connect", skip(endpoint, tls))]
    pub async fn connect_with_tls(
        endpoint: impl Into<String>,
        tls: Option<TlsConfig>,
    ) -> Result<Self> {
        let endpoint = endpoint.into();
        let channel = connect_tls(&endpoint, tls).await?;
        Ok(Self {
            cas: ContentAddressableStorageClient::new(channel.clone()),
            exec: ExecutionClient::new(channel.clone()),
            ac: ActionCacheClient::new(channel),
        })
    }
}

/// Outcome of [`run_command`].
#[derive(Debug)]
pub struct RunOutcome {
    /// Process exit code.
    pub exit_code: i32,
    /// Captured stdout (inline copy from the ActionResult).
    pub stdout: Bytes,
    /// Captured stderr (inline copy from the ActionResult).
    pub stderr: Bytes,
    /// True if the action was served from the action cache without re-running.
    pub cache_hit: bool,
}

/// Run `argv` on the cluster and return its result.
///
/// Builds an `Action` (with empty input root + the given Command), uploads
/// both to CAS, calls `Execute`, and waits for the streamed completion.
#[tracing::instrument(
    name = "client::execute",
    skip(client, argv),
    fields(
        argv_len = argv.len(),
        skip_cache_lookup,
        action_digest = tracing::field::Empty,
        cache_hit = tracing::field::Empty,
        exit_code = tracing::field::Empty,
    ),
)]
pub async fn run_command(
    client: &mut BrokkrClient,
    argv: &[String],
    skip_cache_lookup: bool,
) -> Result<RunOutcome> {
    let command = rapi::Command {
        arguments: argv.to_vec(),
        ..Default::default()
    };
    let command_bytes = command.encode_to_vec();
    let command_digest = digest_of(&command_bytes);

    // Empty input root: a Directory message with no entries.
    let input_root = rapi::Directory::default();
    let input_root_bytes = input_root.encode_to_vec();
    let input_root_digest = digest_of(&input_root_bytes);

    let action = rapi::Action {
        command_digest: Some(command_digest.clone()),
        input_root_digest: Some(input_root_digest.clone()),
        ..Default::default()
    };
    let action_bytes = action.encode_to_vec();
    let action_digest = digest_of(&action_bytes);
    tracing::Span::current().record(
        "action_digest",
        tracing::field::display(format_args!(
            "{}/{}",
            action_digest.hash, action_digest.size_bytes
        )),
    );

    // FindMissingBlobs precheck so cache-hit calls (where Action/Command are
    // already present) skip the BatchUpdateBlobs RPC entirely. Plan §13.7
    // calls for "uploads any missing input blobs".
    let candidates: [(rapi::Digest, Vec<u8>); 3] = [
        (action_digest.clone(), action_bytes),
        (command_digest, command_bytes),
        (input_root_digest, input_root_bytes),
    ];
    let missing_resp = client
        .cas
        .find_missing_blobs(rapi::FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: candidates.iter().map(|(d, _)| d.clone()).collect(),
            digest_function: 0,
        })
        .await?
        .into_inner();
    let missing: std::collections::HashSet<(String, i64)> = missing_resp
        .missing_blob_digests
        .into_iter()
        .map(|d| (d.hash, d.size_bytes))
        .collect();
    let requests: Vec<bur::Request> = candidates
        .into_iter()
        .filter(|(d, _)| missing.contains(&(d.hash.clone(), d.size_bytes)))
        .map(|(d, data)| bur::Request {
            digest: Some(d),
            data,
            compressor: 0,
        })
        .collect();
    if !requests.is_empty() {
        let resp = client
            .cas
            .batch_update_blobs(rapi::BatchUpdateBlobsRequest {
                instance_name: String::new(),
                requests,
                digest_function: 0,
            })
            .await?
            .into_inner();
        // BatchUpdateBlobs may report per-blob failures while the gRPC call
        // itself succeeds. Surface the first such failure as an error so it
        // does not get silently swallowed and resurface later as a confusing
        // "blob not found" during Execute.
        for r in &resp.responses {
            let status = r.status.as_ref();
            if status.map(|s| s.code != 0).unwrap_or(false) {
                let digest = r
                    .digest
                    .as_ref()
                    .map(|d| format!("{}/{}", d.hash, d.size_bytes))
                    .unwrap_or_else(|| "<no digest>".to_string());
                let (code, message) = status
                    .map(|s| (s.code, s.message.as_str()))
                    .unwrap_or((-1, ""));
                return Err(anyhow!(
                    "CAS rejected blob {digest}: code={code} message={message:?}"
                ));
            }
        }
    }

    let mut stream = client
        .exec
        .execute(rapi::ExecuteRequest {
            instance_name: String::new(),
            skip_cache_lookup,
            action_digest: Some(action_digest),
            digest_function: 0,
            ..Default::default()
        })
        .await?
        .into_inner();

    while let Some(op) = stream.message().await? {
        if !op.done {
            continue;
        }
        match op.result {
            Some(brokkr_proto::longrunning::operation::Result::Response(any)) => {
                let resp = rapi::ExecuteResponse::decode(any.value.as_slice())
                    .context("decoding ExecuteResponse")?;
                let result = resp
                    .result
                    .ok_or_else(|| anyhow!("ExecuteResponse missing ActionResult"))?;
                tracing::Span::current()
                    .record("cache_hit", resp.cached_result)
                    .record("exit_code", result.exit_code);
                return Ok(RunOutcome {
                    exit_code: result.exit_code,
                    stdout: Bytes::from(result.stdout_raw),
                    stderr: Bytes::from(result.stderr_raw),
                    cache_hit: resp.cached_result,
                });
            }
            Some(brokkr_proto::longrunning::operation::Result::Error(s)) => {
                return Err(anyhow!("execution failed: {} ({})", s.message, s.code));
            }
            None => {
                return Err(anyhow!("Operation done with no result"));
            }
        }
    }
    Err(anyhow!("control plane closed stream before completion"))
}

fn digest_of(bytes: &[u8]) -> rapi::Digest {
    rapi::Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: bytes.len() as i64,
    }
}
