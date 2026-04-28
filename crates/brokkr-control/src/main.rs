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

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_addr_default() {
        let opts = Opts::try_parse_from(["brokkr-control"]).unwrap();
        assert_eq!(opts.addr, "127.0.0.1:50051".parse().unwrap());
        assert!(opts.cas_path.is_none());
    }

    #[test]
    fn parses_custom_addr() {
        let opts = Opts::try_parse_from(["brokkr-control", "--addr", "0.0.0.0:9000"]).unwrap();
        assert_eq!(opts.addr, "0.0.0.0:9000".parse().unwrap());
    }

    #[test]
    fn parses_cas_path() {
        let opts = Opts::try_parse_from(["brokkr-control", "--cas-path", "/tmp/brokkr"]).unwrap();
        assert_eq!(opts.cas_path.as_ref().unwrap().as_os_str(), "/tmp/brokkr");
    }
}
