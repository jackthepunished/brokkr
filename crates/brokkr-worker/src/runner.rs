//! Phase 1 action runner: spawns the command as a plain child process and
//! captures stdout/stderr/exit. Phase 2 replaces this with the sandboxed
//! variant in `brokkr-sandbox`.

use anyhow::{anyhow, Result};
use brokkr_proto::reapi_v2 as rapi;
use bytes::Bytes;
use sha2::{Digest as _, Sha256};
use tokio::process::Command;

/// Outcome of running a `Command`.
#[derive(Debug)]
pub struct RunOutcome {
    /// Process exit code (negative means killed by signal on Unix).
    pub exit_code: i32,
    /// Captured stdout.
    pub stdout: Bytes,
    /// Captured stderr.
    pub stderr: Bytes,
}

/// Run a REAPI [`Command`](rapi::Command) as a child process.
///
/// Phase 1 ignores `command.environment_variables` overrides beyond merging
/// them into the spawned process; no chdir, no input materialization. The
/// happy path for `brokk run --command "echo hello"` requires nothing more.
pub async fn run_command(command: &rapi::Command) -> Result<RunOutcome> {
    let mut argv = command.arguments.iter();
    let argv0 = argv
        .next()
        .ok_or_else(|| anyhow!("Command.arguments is empty"))?;
    let mut cmd = Command::new(argv0);
    cmd.args(argv);
    for env in &command.environment_variables {
        cmd.env(&env.name, &env.value);
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| anyhow!("spawning {argv0}: {e}"))?;
    let exit_code = output.status.code().unwrap_or(-1);
    Ok(RunOutcome {
        exit_code,
        stdout: Bytes::from(output.stdout),
        stderr: Bytes::from(output.stderr),
    })
}

/// Compute the sha256 digest of `bytes` as a REAPI [`Digest`](rapi::Digest).
pub fn proto_digest(bytes: &[u8]) -> rapi::Digest {
    rapi::Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: bytes.len() as i64,
    }
}
