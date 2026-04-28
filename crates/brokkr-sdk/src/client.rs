//! High-level Brokkr client. Wraps REAPI's CAS + Execution into a single
//! "run this command" call.

use anyhow::{anyhow, Context, Result};
use brokkr_proto::reapi_v2::{
    self as rapi, action_cache_client::ActionCacheClient, batch_update_blobs_request as bur,
    content_addressable_storage_client::ContentAddressableStorageClient,
    execution_client::ExecutionClient,
};
use bytes::Bytes;
use prost::Message;
use sha2::{Digest as _, Sha256};
use tonic::transport::{Channel, Endpoint};

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
    /// `http://127.0.0.1:7878`).
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let channel = Endpoint::from_shared(endpoint.clone())
            .with_context(|| format!("invalid endpoint {endpoint:?}"))?
            .connect()
            .await
            .context("connecting to control plane")?;
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

    // Upload Action, Command, and the empty Directory to CAS.
    client
        .cas
        .batch_update_blobs(rapi::BatchUpdateBlobsRequest {
            instance_name: String::new(),
            requests: vec![
                bur::Request {
                    digest: Some(action_digest.clone()),
                    data: action_bytes,
                    compressor: 0,
                },
                bur::Request {
                    digest: Some(command_digest),
                    data: command_bytes,
                    compressor: 0,
                },
                bur::Request {
                    digest: Some(input_root_digest),
                    data: input_root_bytes,
                    compressor: 0,
                },
            ],
            digest_function: 0,
        })
        .await?;

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
