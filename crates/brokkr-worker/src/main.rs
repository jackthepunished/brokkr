//! `brokkr-worker` daemon entrypoint.

use anyhow::Result;
use brokkr_worker::{run_worker, WorkerConfig};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "brokkr-worker", version, about = "Brokkr worker daemon")]
struct Args {
    /// gRPC endpoint of the brokkr-control server (e.g. `http://127.0.0.1:7878`).
    #[arg(long, default_value = "http://127.0.0.1:7878")]
    control: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let cfg = WorkerConfig {
        control_endpoint: args.control,
        hostname: hostname_or("worker".to_string()),
    };
    run_worker(cfg).await
}

fn hostname_or(fallback: String) -> String {
    std::env::var("HOSTNAME").unwrap_or(fallback)
}
