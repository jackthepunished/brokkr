//! `brokk` — the Brokkr command-line interface.
//!
//! Phase 1: `version`, stub `init`, and `run --command "..."` for the
//! end-to-end happy path.

use anyhow::{anyhow, Context, Result};
use brokkr_sdk::{run_command, BrokkrClient};
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
    /// Run a shell command on the cluster.
    Run {
        /// Control plane endpoint (e.g. `http://127.0.0.1:7878`).
        #[arg(long, default_value = "http://127.0.0.1:7878")]
        control: String,

        /// Skip the action cache lookup and force re-execution.
        #[arg(long)]
        no_cache: bool,

        /// The command to run, including arguments. Pass via repeated
        /// positional args: `brokk run -- echo hello world`.
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
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
        Command::Run {
            control,
            no_cache,
            argv,
        } => run_subcmd(control, no_cache, argv),
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

fn run_subcmd(control: String, no_cache: bool, argv: Vec<String>) -> Result<()> {
    if argv.is_empty() {
        return Err(anyhow!("`brokk run` requires a command"));
    }
    let rt = tokio::runtime::Runtime::new().context("starting tokio runtime")?;
    rt.block_on(async {
        let mut client = BrokkrClient::connect(control).await?;
        let outcome = run_command(&mut client, &argv, no_cache).await?;
        // Forward stdout/stderr verbatim to the user's terminal.
        use std::io::Write as _;
        std::io::stdout().write_all(&outcome.stdout)?;
        std::io::stderr().write_all(&outcome.stderr)?;
        eprintln!(
            "[brokk] exit={} cache_hit={}",
            outcome.exit_code, outcome.cache_hit
        );
        std::process::exit(outcome.exit_code);
    })
}
