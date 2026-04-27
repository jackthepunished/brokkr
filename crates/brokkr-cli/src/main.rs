//! `brokk` — the Brokkr command-line interface.
//!
//! Phase 0 ships only `version` and a stub `init`. Real subcommands
//! (`run`, `build`, `cache`, `worker`, `cluster`, `admin`) land in Phase 1+.

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Top-level CLI entrypoint.
#[derive(Debug, Parser)]
#[command(
    name = "brokk",
    version,
    about = "Brokkr — distributed build & compute grid",
    long_about = None,
    arg_required_else_help = true,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the brokk version, target triple, and embedded git revision.
    Version,
    /// Initialize a Brokkr project in the current directory.
    Init {
        /// Overwrite existing config files.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Version => print_version(),
        Command::Init { force } => init_project(force),
    }
}

fn print_version() -> Result<()> {
    println!(
        "brokk {} ({})\nrustc: {}\ntarget: {}",
        env!("CARGO_PKG_VERSION"),
        option_env!("BROKKR_GIT_SHA").unwrap_or("unknown"),
        env!("BROKKR_RUSTC_VERSION"),
        env!("BROKKR_TARGET_TRIPLE"),
    );
    Ok(())
}

fn init_project(_force: bool) -> Result<()> {
    // TODO(brokkr-cli-init): scaffold a brokk.toml + .brokkrignore + sample
    // workflows. Tracked under Phase 1 task list.
    println!("brokk init: not yet implemented (Phase 0 stub)");
    Ok(())
}
