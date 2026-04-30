//! `brokkr-worker` daemon entrypoint.

use std::process::ExitCode;

use anyhow::Result;
use brokkr_sandbox::host_check::check_run;
use brokkr_worker::{run_worker, WorkerConfig};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "brokkr-worker", version, about = "Brokkr worker daemon")]
struct Args {
    /// gRPC endpoint of the brokkr-control server (e.g. `http://127.0.0.1:7878`).
    #[arg(long, default_value = "http://127.0.0.1:7878")]
    control: String,

    /// Run the Phase 2 host-compatibility check and exit. Prints a per-probe
    /// checklist and exits 0 iff the sandbox can run on this host (warnings
    /// allowed). See `docs/phase-2-plan.md` §10.3.
    #[arg(long)]
    check_host: bool,
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
    let cfg = WorkerConfig {
        control_endpoint: args.control,
        hostname: hostname_or("worker".to_string()),
    };
    run_worker(cfg).await
}

fn run_check_host() -> ExitCode {
    let report = check_run();
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
