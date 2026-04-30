//! REAPI service implementations bound to Brokkr storage backends.
//!
//! Split into one file per service for separation of concerns:
//! - [`cas`] — ContentAddressableStorage
//! - [`action_cache`] — ActionCache
//! - [`capabilities`] — Capabilities
//! - [`execution`] — Execution

use brokkr_common::Digest;
use brokkr_proto::reapi_v2 as rapi;
use tonic::Status;

pub mod action_cache;
pub mod capabilities;
pub mod cas;
pub mod execution;

// Re-export so `crate::services::*` continues to work.
pub use action_cache::ActionCacheService;
pub use capabilities::CapabilitiesService;
pub use cas::CasService;
pub use execution::ExecutionService;

// Shared helpers used across service implementations.
pub(crate) fn proto_to_digest(d: &rapi::Digest) -> Result<Digest, Status> {
    Digest::from_str(&d.hash)
        .map_err(|e| Status::invalid_argument(format!("invalid digest hash: {e}")))
}

pub(crate) fn digest_to_proto(d: &Digest) -> rapi::Digest {
    rapi::Digest {
        hash: d.hash().to_string(),
        size_bytes: d.size_bytes(),
    }
}