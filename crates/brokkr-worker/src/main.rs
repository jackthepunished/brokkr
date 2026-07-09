//! `brokkr-worker` daemon entrypoint.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, Result};
use brokkr_sandbox::{checks, ResourceLimits, Sandbox};
use brokkr_worker::{run_worker, Runner, SandboxRunner, SandboxTemplate, TlsConfig, WorkerConfig};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "brokkr-worker", version, about = "Brokkr worker daemon")]
struct Args {
    /// gRPC endpoint of the brokkr-control **client** port (e.g.
    /// `http://127.0.0.1:7878`). The worker uses this for
    /// `ContentAddressableStorage` calls (stdout/stderr uploads). In
    /// single-port mode this is also where `WorkerService` lives.
    #[arg(long, default_value = "http://127.0.0.1:7878")]
    control: String,

    /// gRPC endpoint of the brokkr-control **worker** port (where
    /// `WorkerService` is served). Defaults to the same scheme and host
    /// as `--control`, with the port bumped by one (e.g.
    /// `http://127.0.0.1:7879` for `--control http://127.0.0.1:7878`).
    /// The control plane only binds a separate worker port when
    /// `--tls-client-ca` is configured; in open / single-port mode
    /// this is ignored. With mTLS, this endpoint MUST be `https://` and
    /// the worker MUST be started with `--client-cert`/`--client-key`.
    #[arg(long)]
    worker_control: Option<String>,

    /// Run the Phase 2 host-compatibility check and exit. Prints a per-probe
    /// checklist and exits 0 iff the sandbox can run on this host (warnings
    /// allowed). See `docs/phase-2-plan.md` §10.3.
    #[arg(long)]
    check_host: bool,

    /// Disable the Phase 2 sandbox. The worker runs actions as plain
    /// child processes — Phase 1 parity. Useful for development hosts
    /// that lack unprivileged user namespaces or cgroup delegation.
    /// Logs a loud warning at startup.
    #[arg(long)]
    no_sandbox: bool,

    /// Path to the `brokkr-sandboxd` runner binary. Defaults to a
    /// lookup adjacent to the worker binary, then `$PATH`.
    #[arg(long)]
    sandbox_runner: Option<PathBuf>,

    /// Per-action cgroup parent. Typically
    /// `/sys/fs/cgroup/brokkr.slice` (created by
    /// `scripts/install-cgroup-slice.sh`) or a systemd-delegated
    /// slice. When omitted, the sandbox runs without resource limits
    /// or accounting.
    #[arg(long)]
    sandbox_cgroup_root: Option<PathBuf>,

    /// Default per-action wall-clock timeout, in seconds. Per-action
    /// `Platform` properties may override this in a later milestone.
    #[arg(long)]
    sandbox_wall_clock_secs: Option<u64>,

    /// Default per-action memory cap, in bytes. `0` = unlimited.
    #[arg(long)]
    sandbox_memory_bytes: Option<u64>,

    /// Default per-action `pids.max`. `0` = unlimited.
    #[arg(long)]
    sandbox_pids_max: Option<u64>,

    /// CA certificate for verifying the control plane server certificate.
    #[arg(long)]
    ca: Option<PathBuf>,

    /// Client certificate for mTLS authentication (requires --client-key).
    #[arg(long, requires = "client_key")]
    client_cert: Option<PathBuf>,

    /// Client private key for mTLS authentication (requires --client-cert).
    #[arg(long, requires = "client_cert")]
    client_key: Option<PathBuf>,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    if args.check_host {
        return run_check_host();
    }
    match tokio::runtime::Runtime::new() {
        Ok(rt) => match rt.block_on(run_daemon(args)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("brokkr-worker: {e:#}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("brokkr-worker: starting tokio runtime: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_daemon(args: Args) -> Result<()> {
    let has_client_creds = args.client_cert.is_some() || args.client_key.is_some();
    if has_client_creds && args.ca.is_none() {
        anyhow::bail!("--client-cert and --client-key require --ca to be provided");
    }
    let runner = build_runner(&args)?;
    let tls = match (&args.ca, &args.client_cert, &args.client_key) {
        (Some(ca), cert, key) => Some(TlsConfig {
            ca_cert: ca.clone(),
            client_cert: cert.clone(),
            client_key: key.clone(),
        }),
        _ => None,
    };
    // Default the worker port to "{scheme}://{host}:{port+1}" derived
    // from --control. The bumped port is only meaningful when the
    // control plane is in split-port mode — i.e. when mTLS is
    // configured (--ca). In open / single-port mode the worker port
    // doesn't exist on the server, so we pass `None` and `run_worker`
    // falls back to `control_endpoint` for `WorkerService` too.
    let worker_endpoint = match args.worker_control {
        Some(s) => Some(s),
        None if args.ca.is_some() => derive_default_worker_endpoint(&args.control),
        None => None,
    };
    let cfg = WorkerConfig {
        control_endpoint: args.control,
        worker_endpoint,
        hostname: hostname_or("worker".to_string()),
        runner,
        tls,
    };
    run_worker(cfg).await
}

/// Bump the port of an `http://host:port` / `https://host:port` URL by
/// one. Returns `None` if the input is not parseable as a URL with a
/// numeric port — `run_worker` will then fall back to
/// `control_endpoint` (single-port mode), and the explicit
/// `https://` fail-fast in `run_worker` will catch any misconfig.
fn derive_default_worker_endpoint(control: &str) -> Option<String> {
    let (scheme, rest) = control.split_once("://")?;
    // Take the authority up to the next `/` (if any).
    let authority = rest.split('/').next().unwrap_or(rest);
    // Split host:port — last ':' to handle bare IPv6.
    let (host, port) = authority.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let bumped = port.checked_add(1)?;
    Some(format!("{scheme}://{host}:{bumped}"))
}

fn build_runner(args: &Args) -> Result<Runner> {
    if args.no_sandbox {
        tracing::warn!(
            "starting with --no-sandbox: actions will run as plain host \
             processes with no isolation. This is Phase 1 parity and is \
             intended for development only."
        );
        return Ok(Runner::Plain);
    }

    // Probe the host before doing anything else; surface a useful
    // error so deployers know to run `scripts/install-cgroup-slice.sh`
    // or check `brokkr-worker --check-host`.
    let report = checks::run();
    if !report.is_functional() {
        return Err(anyhow!(
            "brokkr-worker: host is not sandbox-capable. Run \
             `brokkr-worker --check-host` for details, or pass \
             `--no-sandbox` to bypass the sandbox (Phase 1 fallback)."
        ));
    }

    let sandbox = match &args.sandbox_runner {
        Some(path) => Sandbox::new(path),
        None => Sandbox::with_default_runner()
            .map_err(|e| anyhow!("locating brokkr-sandboxd runner: {e}"))?,
    };
    let sandbox = match &args.sandbox_cgroup_root {
        Some(root) => sandbox.with_cgroup_root(root),
        None => sandbox,
    };

    let mut template = SandboxTemplate::brokkr_default();
    template.limits = ResourceLimits {
        wall_clock_secs: args.sandbox_wall_clock_secs,
        memory_bytes: args.sandbox_memory_bytes.filter(|n| *n != 0),
        pids_max: args.sandbox_pids_max.filter(|n| *n != 0),
        ..Default::default()
    };

    Ok(Runner::Sandboxed(Box::new(SandboxRunner {
        sandbox,
        template,
    })))
}

fn run_check_host() -> ExitCode {
    let report = checks::run();
    print!("{report}");
    if report.is_functional() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn hostname_or(fallback: String) -> String {
    std::env::var("HOSTNAME").unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::derive_default_worker_endpoint;

    #[test]
    fn bumps_port_for_http_url() {
        assert_eq!(
            derive_default_worker_endpoint("http://127.0.0.1:7878"),
            Some("http://127.0.0.1:7879".to_string())
        );
    }

    #[test]
    fn bumps_port_for_https_url() {
        assert_eq!(
            derive_default_worker_endpoint("https://control.example.com:8443"),
            Some("https://control.example.com:8444".to_string())
        );
    }

    #[test]
    fn preserves_path_suffix() {
        assert_eq!(
            derive_default_worker_endpoint("http://127.0.0.1:7878/some/path"),
            Some("http://127.0.0.1:7879".to_string())
        );
    }

    #[test]
    fn returns_none_for_unparseable() {
        assert_eq!(derive_default_worker_endpoint("not-a-url"), None);
    }
}
