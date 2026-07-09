//! High-level Brokkr client. Wraps REAPI's CAS + Execution into a single
//! "run this command" call.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use brokkr_proto::reapi_v2::{
    self as rapi, action_cache_client::ActionCacheClient, batch_update_blobs_request as bur,
    content_addressable_storage_client::ContentAddressableStorageClient,
    execution_client::ExecutionClient,
};
use bytes::Bytes;
use prost::Message;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

/// A server-reported execution failure carrying the `google.rpc.Status` code.
///
/// Surfaced from both [`check_status`] (the `ExecuteResponse.status` path) and
/// the streamed `Operation` error path, so callers can inspect `code` for
/// retry decisions instead of parsing a message string (issue #62).
#[derive(Debug, Error)]
pub enum ExecuteError {
    /// Server returned a non-OK status.
    #[error("execution failed: {message} (code={code})")]
    Status {
        /// The gRPC / `google.rpc.Status` code (non-zero). Tells the caller
        /// whether the failure is retryable (`RESOURCE_EXHAUSTED`,
        /// `UNAVAILABLE`, `DEADLINE_EXCEEDED`, …).
        code: i32,
        /// The error message from the server.
        message: String,
    },
    /// `ExecuteResponse` had no `ActionResult`.
    #[error("ExecuteResponse missing ActionResult")]
    MissingResult,
}

/// TLS configuration for a [`BrokkrClient`] connection.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// CA certificate to verify the server certificate.
    pub ca_cert: PathBuf,
    /// Client certificate for client authentication (mTLS).
    pub client_cert: Option<PathBuf>,
    /// Client private key for client authentication (mTLS).
    pub client_key: Option<PathBuf>,
}

/// Errors that can occur when connecting a [`BrokkrClient`].
#[derive(Debug, Error)]
pub enum ClientError {
    /// The endpoint URL is invalid.
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
    /// Reading a certificate or key file failed.
    #[error("reading TLS certificate: {0}")]
    CertificateRead(#[from] std::io::Error),
    /// TLS configuration is invalid.
    #[error("TLS configuration: {0}")]
    TlsConfig(String),
    /// Transport-level error connecting to the server.
    #[error("transport error: {0}")]
    Transport(#[from] tonic::transport::Error),
}

/// Client connection to a Brokkr control plane.
#[derive(Clone)]
pub struct BrokkrClient {
    cas: ContentAddressableStorageClient<InterceptedChannel>,
    exec: ExecutionClient<InterceptedChannel>,
    #[allow(dead_code)]
    ac: ActionCacheClient<InterceptedChannel>,
    /// Optional JWT bearer token. When set, every outbound RPC carries an
    /// `authorization: Bearer <token>` header injected by the private
    /// `bearer_interceptor` below. Issued by
    /// [`BrokkrClient::connect_with_bearer`]; absent for the no-auth
    /// constructors (ADR 0011, issue #139).
    bearer: Option<Arc<str>>,
}

/// Channel type carried by the public stubs after the bearer interceptor
/// has been applied. All three stubs share the same shared interceptor
/// (see [`SharedInterceptorAdapter`]) so they all materialise the same
/// `InterceptedService<Channel, SharedInterceptorAdapter>`.
type InterceptedChannel =
    tonic::service::interceptor::InterceptedService<Channel, SharedInterceptorAdapter>;

impl BrokkrClient {
    /// Connect to the control plane at `endpoint` (e.g.
    /// `http://127.0.0.1:7878`).
    #[tracing::instrument(name = "client::connect", skip(endpoint))]
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self, ClientError> {
        let endpoint = endpoint.into();
        let channel = Endpoint::from_shared(endpoint.clone())
            .map_err(|e| {
                ClientError::InvalidEndpoint(format!("invalid endpoint {endpoint:?}: {e}"))
            })?
            .connect()
            .await
            .map_err(ClientError::Transport)?;
        Ok(Self::from_channel(channel, None))
    }

    /// Connect to the control plane at `endpoint` with mTLS authentication.
    #[tracing::instrument(name = "client::connect_with_tls", skip(endpoint, tls_cfg))]
    pub async fn connect_with_tls(
        endpoint: impl Into<String>,
        tls_cfg: TlsConfig,
    ) -> Result<Self, ClientError> {
        let channel = connect_tls(endpoint, &tls_cfg).await?;
        Ok(Self::from_channel(channel, None))
    }

    /// Connect to the control plane at `endpoint` with a JWT bearer token.
    ///
    /// The token is sent in an `authorization: Bearer <token>` header on
    /// every outbound RPC (CAS, Execution, ActionCache). This is the
    /// client-side counterpart to the `auth_interceptor` the control
    /// plane wires up when `--auth-jwt-*` is configured (ADR 0011).
    ///
    /// Note: bearer auth is an *application-layer* identity on top of an
    /// existing transport (plaintext or mTLS). It does not replace TLS;
    /// production deployments should still pass `--tls-ca` (via
    /// [`BrokkrClient::connect_with_tls`]) so the bearer token is not
    /// sent over the wire in cleartext.
    #[tracing::instrument(name = "client::connect_with_bearer", skip(endpoint, bearer))]
    pub async fn connect_with_bearer(
        endpoint: impl Into<String>,
        bearer: impl Into<String>,
    ) -> Result<Self, ClientError> {
        let endpoint = endpoint.into();
        let channel = Endpoint::from_shared(endpoint.clone())
            .map_err(|e| {
                ClientError::InvalidEndpoint(format!("invalid endpoint {endpoint:?}: {e}"))
            })?
            .connect()
            .await
            .map_err(ClientError::Transport)?;
        Ok(Self::from_channel(channel, Some(Arc::from(bearer.into()))))
    }

    /// Connect to the control plane at `endpoint` with both mTLS and a JWT
    /// bearer token. Production target for issue #139: TLS terminates the
    /// network, mTLS establishes the worker's identity, and the bearer
    /// token is only relevant for *client* traffic (a worker does not
    /// have one — that is what the split listener is for).
    #[tracing::instrument(
        name = "client::connect_with_tls_and_bearer",
        skip(endpoint, tls_cfg, bearer)
    )]
    pub async fn connect_with_tls_and_bearer(
        endpoint: impl Into<String>,
        tls_cfg: TlsConfig,
        bearer: impl Into<String>,
    ) -> Result<Self, ClientError> {
        let channel = connect_tls(endpoint, &tls_cfg).await?;
        Ok(Self::from_channel(channel, Some(Arc::from(bearer.into()))))
    }

    /// Build the three client stubs from `channel`, applying the bearer
    /// interceptor to each when a token is configured.
    fn from_channel(channel: Channel, bearer: Option<Arc<str>>) -> Self {
        // The interceptor is type-erased behind `Box<dyn FnMut + Send>`
        // and shared across all three stubs via `Arc`. The adapter
        // implements tonic's `Interceptor` trait manually because the
        // blanket impl only covers plain `FnMut` closures. We pass a
        // borrowed `Option<&Arc<str>>` so the move into the struct
        // field can happen *after* the closure has captured.
        let interceptor = SharedInterceptorAdapter(make_shared_interceptor(bearer.as_ref()));
        Self {
            cas: ContentAddressableStorageClient::with_interceptor(
                channel.clone(),
                interceptor.clone(),
            ),
            exec: ExecutionClient::with_interceptor(channel.clone(), interceptor.clone()),
            ac: ActionCacheClient::with_interceptor(channel, interceptor),
            bearer,
        }
    }

    /// Returns the bearer token this client is sending, if any. Useful
    /// for tests and for the in-process JWT-client helper used by the
    /// split-port integration suite.
    pub fn bearer(&self) -> Option<&str> {
        self.bearer.as_deref()
    }

    /// Read a single blob by digest. Returns `None` if the digest is
    /// absent. Convenience wrapper around `batch_read_blobs` for the
    /// split-port integration suite — keeping the public SDK surface
    /// minimal means tests reach into `brokkr_cas::Cas` via the on-disk
    /// redb instead. This helper exists because `RedbCas::open` cannot
    /// acquire the database lock while the control plane is running.
    pub async fn find_blob(
        &mut self,
        hash: &str,
        size_bytes: i64,
    ) -> Result<Option<Bytes>, tonic::Status> {
        let digest = rapi::Digest {
            hash: hash.to_string(),
            size_bytes,
        };
        let resp = self
            .cas
            .batch_read_blobs(rapi::BatchReadBlobsRequest {
                instance_name: String::new(),
                digests: vec![digest],
                digest_function: 0,
                acceptable_compressors: vec![],
            })
            .await?
            .into_inner();
        match resp.responses.into_iter().next() {
            Some(r) => {
                if r.status.as_ref().map(|s| s.code != 0).unwrap_or(false) {
                    return Ok(None);
                }
                Ok(Some(Bytes::from(r.data)))
            }
            None => Ok(None),
        }
    }
}

/// Concrete shared-interceptor type used by all three stubs. The closure
/// body is heap-allocated and behind a `std::sync::Mutex` so we can
/// `Clone` it (the gRPC client requires the interceptor to be `Clone`,
/// and a `Box<dyn FnMut>` is `Clone` only when wrapped in `Arc`).
///
/// `std::sync::Mutex` (not `tokio::sync::Mutex`) is intentional: the
/// interceptor runs synchronously inside the request pipeline before the
/// call is dispatched. Holding it for the few microseconds it takes to
/// insert a metadata key is fine, and avoiding an async lock keeps the
/// interceptor compatible with sync `FnMut` semantics.
type SharedInterceptor = Arc<
    std::sync::Mutex<Box<dyn FnMut(Request<()>) -> Result<Request<()>, Status> + Send + 'static>>,
>;

fn make_shared_interceptor(token: Option<&Arc<str>>) -> SharedInterceptor {
    Arc::new(std::sync::Mutex::new(Box::new(bearer_interceptor(
        token.cloned(),
    ))))
}

/// Adapter that turns the boxed, mutex-guarded closure into something
/// tonic's generated `with_interceptor<F: Interceptor>` will accept.
///
/// Tonic only blanket-implements `Interceptor` for `FnMut(Request<()>) -> …`,
/// not for `Arc<Mutex<Box<dyn FnMut …>>>`. This wrapper deref-locks the
/// mutex for the duration of one request and forwards to the closure.
#[derive(Clone)]
struct SharedInterceptorAdapter(SharedInterceptor);

impl tonic::service::Interceptor for SharedInterceptorAdapter {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let mut guard = self
            .0
            .lock()
            .map_err(|_| Status::internal("bearer interceptor mutex poisoned"))?;
        (guard)(request)
    }
}

/// Build a gRPC interceptor that injects `authorization: Bearer <token>`
/// on every outbound request. A no-op when `token` is `None` so the same
/// helper can be used unconditionally in [`BrokkrClient::from_channel`].
///
/// `MetadataValue::from_str` only fails on embedded CR/LF/NUL, which a
/// caller-controlled JWT cannot contain (RFC 7519 §2: JWT is
/// base64url-encoded JSON, no whitespace). If it ever did, the request
/// is cancelled with `INVALID_ARGUMENT` rather than silently dropping
/// the auth header — failing closed is the only safe posture for an
/// auth interceptor.
fn bearer_interceptor(
    token: Option<Arc<str>>,
) -> impl FnMut(Request<()>) -> Result<Request<()>, Status> + Send {
    move |mut req: Request<()>| {
        let Some(token) = token.as_ref() else {
            return Ok(req);
        };
        let value = format!("Bearer {token}");
        match value.parse() {
            Ok(v) => {
                req.metadata_mut().insert("authorization", v);
                Ok(req)
            }
            Err(_) => Err(Status::invalid_argument(
                "bearer token contains characters illegal in HTTP header values",
            )),
        }
    }
}

/// Establish a TLS channel to the control plane.
async fn connect_tls(
    endpoint: impl Into<String>,
    tls_cfg: &TlsConfig,
) -> Result<Channel, ClientError> {
    use tonic::transport::ClientTlsConfig;

    let endpoint = endpoint.into();

    // Reject partial client identity: one of cert/key without the other.
    match (&tls_cfg.client_cert, &tls_cfg.client_key) {
        (Some(_), None) | (None, Some(_)) => {
            return Err(ClientError::TlsConfig(
                "both client_cert and client_key must be provided for client identity".to_string(),
            ));
        }
        _ => {}
    }

    let endpoint = Endpoint::from_shared(endpoint.clone())
        .map_err(|e| ClientError::InvalidEndpoint(format!("invalid endpoint {endpoint:?}: {e}")))?;

    let ca_pem = tokio::fs::read(&tls_cfg.ca_cert)
        .await
        .map_err(ClientError::CertificateRead)?;
    let tls_config =
        ClientTlsConfig::new().ca_certificate(tonic::transport::Certificate::from_pem(ca_pem));

    // If client identity is provided, add it to the TLS config.
    let tls_config = match (&tls_cfg.client_cert, &tls_cfg.client_key) {
        (Some(cert_path), Some(key_path)) => {
            let cert_pem = tokio::fs::read(cert_path)
                .await
                .map_err(ClientError::CertificateRead)?;
            let key_pem = tokio::fs::read(key_path)
                .await
                .map_err(ClientError::CertificateRead)?;
            let identity = tonic::transport::Identity::from_pem(cert_pem, key_pem);
            tls_config.identity(identity)
        }
        _ => tls_config,
    };

    let channel = endpoint
        .tls_config(tls_config)
        .map_err(ClientError::Transport)?
        .connect()
        .await
        .map_err(ClientError::Transport)?;

    Ok(channel)
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

/// Check server-reported status before accessing result.
/// Returns `Ok(ActionResult)` if status is OK or absent (owned copy).
/// Returns `Err(ExecuteError)` if status.code != 0 or result is absent.
pub fn check_status(resp: &rapi::ExecuteResponse) -> Result<rapi::ActionResult, ExecuteError> {
    if let Some(status) = &resp.status {
        if status.code != 0 {
            return Err(ExecuteError::Status {
                code: status.code,
                message: status.message.clone(),
            });
        }
    }
    match resp.result.clone() {
        Some(r) => Ok(r),
        None => Err(ExecuteError::MissingResult),
    }
}

/// Map a streamed `Operation` error `Status` into a structured [`ExecuteError`].
///
/// The `google.rpc.Status.code` is what tells a caller whether the failure is
/// retryable, so it must survive in the error type rather than being flattened
/// into a formatted string (issue #62).
fn operation_error(status: brokkr_proto::rpc::Status) -> ExecuteError {
    ExecuteError::Status {
        code: status.code,
        message: status.message,
    }
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
                let status_code = resp.status.as_ref().map(|s| s.code).unwrap_or(0);
                // Propagate the structured ExecuteError (not a stringified copy)
                // so callers can downcast and inspect the code (issue #62).
                let result = check_status(&resp)?;
                if status_code != 0 {
                    tracing::Span::current().record("exec_status_code", status_code);
                }
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
                return Err(operation_error(s).into());
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::panic
)]
mod tests {
    use super::*;
    use tonic::service::Interceptor as _;

    #[test]
    fn operation_error_surfaces_code_for_retry_inspection() {
        let status = brokkr_proto::rpc::Status {
            code: 8, // RESOURCE_EXHAUSTED
            message: "quota exceeded".to_string(),
            details: vec![],
        };
        let ExecuteError::Status { code, message } = operation_error(status) else {
            panic!("expected Status variant");
        };
        assert_eq!(code, 8);
        assert_eq!(message, "quota exceeded");
    }

    #[test]
    fn execute_error_code_survives_anyhow_boxing() {
        // run_command returns anyhow::Result, so a caller recovers the code by
        // downcasting the boxed error rather than parsing the message.
        let err: anyhow::Error = ExecuteError::Status {
            code: 14, // UNAVAILABLE
            message: "try again".to_string(),
        }
        .into();
        assert!(matches!(
            err.downcast_ref::<ExecuteError>(),
            Some(ExecuteError::Status { code: 14, .. })
        ));
    }

    #[test]
    fn bearer_interceptor_injects_authorization_header_when_token_present() {
        let mut interceptor = bearer_interceptor(Some(Arc::from("abc.def.ghi")));
        let req = Request::new(());
        let req = interceptor(req).expect("interceptor accepts a valid JWT");
        let auth = req
            .metadata()
            .get("authorization")
            .expect("authorization header is set");
        assert_eq!(auth.to_str().unwrap(), "Bearer abc.def.ghi");
    }

    #[test]
    fn bearer_interceptor_is_a_noop_when_token_absent() {
        let mut interceptor = bearer_interceptor(None);
        let req = Request::new(());
        let req = interceptor(req).expect("interceptor accepts the no-auth case");
        assert!(
            req.metadata().get("authorization").is_none(),
            "no header should be set when the client was constructed without a token"
        );
    }

    #[test]
    fn bearer_interceptor_rejects_tokens_with_illegal_header_chars() {
        // A stray CR would let an attacker break out of the header value.
        // The interceptor must refuse the request rather than silently
        // dropping the auth header.
        let mut interceptor = bearer_interceptor(Some(Arc::from("ok\r\nX-Evil: 1")));
        let result = interceptor(Request::new(()));
        assert!(result.is_err(), "illegal header bytes must abort the RPC");
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn shared_interceptor_adapter_injects_header() {
        // The path used by `BrokkrClient::from_channel` — the closure
        // is wrapped in an Arc<Mutex<Box<dyn FnMut ...>>> and the
        // adapter implements `Interceptor` manually because tonic's
        // blanket impl only covers plain `FnMut` closures.
        let token = mint_jwt_via_helper();
        let mut adapter =
            SharedInterceptorAdapter(make_shared_interceptor(Some(&Arc::from(token.as_str()))));
        let req = adapter
            .call(Request::new(()))
            .expect("adapter accepts a valid JWT");
        let auth = req
            .metadata()
            .get("authorization")
            .expect("authorization header is set via the adapter");
        let s = auth.to_str().unwrap();
        assert!(s.starts_with("Bearer "), "got {s:?}");
        assert_eq!(s, format!("Bearer {token}"));
    }

    fn mint_jwt_via_helper() -> String {
        // Just any valid string — we don't actually validate it server-side
        // here, only check the header insertion.
        "header.payload.sig".to_string()
    }
}
