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
    /// gRPC endpoint of a brokkr-control **client** port (e.g.
    /// `http://127.0.0.1:7878`). The worker uses this for
    /// `ContentAddressableStorage` calls (stdout/stderr uploads). In
    /// single-port mode this is also where `WorkerService` lives.
    ///
    /// **Repeatable** (Phase 5 I9b): name every control-plane node and the
    /// worker survives losing whichever one it is attached to — it rotates to
    /// the next and re-registers. With one endpoint the behaviour is as
    /// before, except that a closed stream reconnects instead of exiting.
    #[arg(long, default_value = "http://127.0.0.1:7878")]
    control: Vec<String>,

    /// gRPC endpoint of the brokkr-control **worker** port (where
    /// `WorkerService` is served). Defaults to the same scheme and host
    /// as `--control`, with the port bumped by one (e.g.
    /// `http://127.0.0.1:7879` for `--control http://127.0.0.1:7878`).
    /// The control plane only binds a separate worker port when
    /// `--tls-client-ca` is configured; in open / single-port mode
    /// this is ignored. With mTLS, this endpoint MUST be `https://` and
    /// the worker MUST be started with `--client-cert`/`--client-key`.
    ///
    /// **Repeatable**, paired positionally with `--control`. Pass either none
    /// (each node's worker port is derived) or exactly as many as `--control`;
    /// a partial list is rejected rather than silently deriving the rest,
    /// because a worker pointed at the wrong port fails every RPC (issue #139).
    #[arg(long)]
    worker_control: Vec<String>,

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
    let control_planes =
        build_control_planes(&args.control, &args.worker_control, args.ca.is_some())?;
    tracing::info!(
        endpoints = control_planes.len(),
        "control-plane endpoints configured"
    );
    let cfg = WorkerConfig {
        control_planes,
        hostname: hostname_or("worker".to_string()),
        runner,
        tls,
    };
    run_worker(cfg).await
}

/// Pair `--control` with `--worker-control` into the endpoint set the worker
/// rotates over (Phase 5 I9b W4).
///
/// The worker port defaults to `{scheme}://{host}:{port+1}` derived from each
/// `--control`. That bump is only meaningful when the control plane is in
/// split-port mode — i.e. when mTLS is configured (`--ca`); in open /
/// single-port mode the worker port does not exist on the server, so the entry
/// carries `None` and `WorkerService` is reached on the client port too.
///
/// A partial `--worker-control` list is an error rather than a
/// derive-the-rest convenience: mixing explicit and derived worker ports is
/// exactly how a worker ends up pointed at a port nobody serves, and issue #139
/// established that failure is silent until every RPC fails.
fn build_control_planes(
    control: &[String],
    worker_control: &[String],
    mtls: bool,
) -> Result<Vec<brokkr_worker::ControlPlane>> {
    if control.is_empty() {
        anyhow::bail!("--control must be given at least once");
    }
    if !worker_control.is_empty() && worker_control.len() != control.len() {
        anyhow::bail!(
            "--worker-control was given {} time(s) but --control {} time(s); pass either none \
             (worker ports are derived) or exactly one per --control, in the same order",
            worker_control.len(),
            control.len()
        );
    }
    Ok(control
        .iter()
        .enumerate()
        .map(|(i, client)| brokkr_worker::ControlPlane {
            client: client.clone(),
            worker: worker_control.get(i).cloned().or_else(|| {
                if mtls {
                    derive_default_worker_endpoint(client)
                } else {
                    None
                }
            }),
        })
        .collect())
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
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::{build_control_planes, derive_default_worker_endpoint};

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

    /// I9b W4: `--control` is repeatable, and worker ports are derived per
    /// node only in mTLS (split-port) mode.
    #[test]
    fn control_planes_pair_positionally_and_derive_only_under_mtls() {
        let three = [
            "http://a:7878".to_string(),
            "http://b:7878".to_string(),
            "http://c:7878".to_string(),
        ];

        // Open / single-port mode: no worker ports exist on the server, so
        // every entry reaches WorkerService on the client port.
        let planes = build_control_planes(&three, &[], false).unwrap();
        assert_eq!(planes.len(), 3);
        assert!(planes.iter().all(|p| p.worker.is_none()));
        assert_eq!(planes[1].worker_url(), "http://b:7878");

        // mTLS: each node's worker port is derived by bumping its own port,
        // per node rather than from the first one.
        let planes = build_control_planes(&three, &[], true).unwrap();
        assert_eq!(planes[0].worker.as_deref(), Some("http://a:7879"));
        assert_eq!(planes[2].worker.as_deref(), Some("http://c:7879"));
        assert_eq!(planes[2].worker_url(), "http://c:7879");

        // Explicit worker ports win, paired by position.
        let explicit = [
            "https://a:9001".to_string(),
            "https://b:9002".to_string(),
            "https://c:9003".to_string(),
        ];
        let planes = build_control_planes(&three, &explicit, true).unwrap();
        assert_eq!(planes[1].worker.as_deref(), Some("https://b:9002"));
    }

    /// A partial `--worker-control` list must be refused: half-explicit,
    /// half-derived is how a worker silently ends up on a port nobody serves
    /// (issue #139).
    #[test]
    fn a_partial_worker_control_list_is_rejected() {
        let two = ["http://a:7878".to_string(), "http://b:7878".to_string()];
        let one = ["http://a:7879".to_string()];
        let err = build_control_planes(&two, &one, true)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--worker-control") && err.contains("--control"),
            "the error must name both flags, got: {err}"
        );

        // And an empty endpoint set is a config error, not a runtime surprise.
        assert!(build_control_planes(&[], &[], false).is_err());
    }
}
