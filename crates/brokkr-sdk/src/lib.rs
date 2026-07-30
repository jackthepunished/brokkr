//! Client SDK for talking to a Brokkr cluster.
//!
//! Wraps the REAPI gRPC services with ergonomic Rust APIs. Used by
//! `brokkr-cli` and embeddable in any Rust application.

#![deny(missing_docs)]

pub mod client;
pub mod redirect;

pub use client::{
    check_status, run_command, BrokkrClient, ClientError, ExecuteError, RunOutcome, TlsConfig,
};
pub use redirect::{
    classify, hint_to_url, Redirect, LEADER_ADDR_METADATA_KEY, LEADER_HINT_METADATA_KEY,
    MAX_LEADER_HOPS,
};
