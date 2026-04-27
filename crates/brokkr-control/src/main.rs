//! `brokkr-control` daemon entrypoint.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("brokkr-control: phase 0 stub");
    Ok(())
}
