//! `brokk` — the Brokkr command-line interface.
//!
//! Phase 0 ships only `version`. Real subcommands
//! (`run`, `init`, `build`, `cache`, `worker`, `cluster`, `admin`) land in Phase 1+.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;

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
    /// Run a shell command locally.
    Run {
        /// The shell command to execute.
        #[arg(long, short = 'c')]
        command: String,
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
        Command::Run { command } => commands::run::execute(&command),
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
