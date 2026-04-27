//! `brokkr-worker` daemon entrypoint.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("brokkr-worker: phase 0 stub");
    Ok(())
}
