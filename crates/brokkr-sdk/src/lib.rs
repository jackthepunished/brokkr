//! Client SDK for talking to a Brokkr cluster.
//!
//! Wraps the REAPI gRPC services with ergonomic Rust APIs. Used by
//! `brokkr-cli` and embeddable in any Rust application.

#![deny(missing_docs)]

pub mod client;

pub use client::{run_command, BrokkrClient, RunOutcome};
