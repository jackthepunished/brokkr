//! `brokkr-control` daemon entrypoint.

use anyhow::Result;
use clap::Parser;
use std::net::SocketAddr;

#[derive(Debug, Parser)]
#[command(
    name = "brokkr-control",
    about = "Brokkr control plane daemon",
    long_about = None,
)]
struct Opts {
    /// Listen address for the gRPC server.
    #[arg(long, default_value = "127.0.0.1:50051")]
    addr: SocketAddr,

    /// Path to CAS storage directory.
    #[arg(long)]
    cas_path: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    tracing_subscriber::fmt::init();
    tracing::info!("brokkr-control: phase 0 stub");
    tracing::info!("listen addr: {}", opts.addr);
    if let Some(path) = &opts.cas_path {
        tracing::info!("cas path: {}", path.display());
    }
    Ok(())
}
