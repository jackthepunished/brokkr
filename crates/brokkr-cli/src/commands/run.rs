//! `run` subcommand — execute a shell command locally.

use std::process::{Command, Stdio};

/// Execute a shell command and print its output.
pub fn execute(cmd: &str) -> anyhow::Result<()> {
    let output = Command::new("sh")
        .args(["-c", cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !output.stdout.is_empty() {
        println!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        eprintln!("[stderr] {}", String::from_utf8_lossy(&output.stderr));
    }
    let code = output.status.code().unwrap_or(1);
    std::process::exit(code);
}
