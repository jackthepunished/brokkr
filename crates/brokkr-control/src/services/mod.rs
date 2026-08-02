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
pub mod observability;
pub mod peer_observability;

// Re-export so `crate::services::*` continues to work.
pub use action_cache::ActionCacheService;
pub use capabilities::CapabilitiesService;
pub use cas::{BatchLimits, CasService};
pub use execution::ExecutionService;
pub use observability::{ObservabilityDeps, ObservabilityService};
pub use peer_observability::PeerObservabilityService;

// Shared helpers used across service implementations.
pub(crate) fn proto_to_digest(d: &rapi::Digest) -> Result<Digest, Status> {
    Digest::new(d.hash.clone(), d.size_bytes)
        .map_err(|e| Status::invalid_argument(format!("invalid digest: {e}")))
}

pub(crate) fn digest_to_proto(d: &Digest) -> rapi::Digest {
    rapi::Digest {
        hash: d.hash().to_string(),
        size_bytes: d.size_bytes(),
    }
}

/// Reject any request that targets a *named* REAPI instance.
///
/// Phase 1 serves a single, unnamed instance backed by one global CAS and
/// action cache. The SDK always sends an empty `instance_name`. Accepting a
/// non-empty value would silently serve a caller-named instance out of that
/// one shared store — a latent cross-instance access gap (issue #72). Until
/// multi-tenant routing exists, anything non-empty is rejected as
/// `INVALID_ARGUMENT` rather than served from the default instance.
pub(crate) fn validate_instance_name(instance_name: &str) -> Result<(), Status> {
    if instance_name.is_empty() {
        Ok(())
    } else {
        Err(Status::invalid_argument(format!(
            "unknown instance_name {instance_name:?}: this server has no multi-tenant \
             support yet — only the default (empty) instance is served"
        )))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn empty_instance_name_is_accepted() {
        assert!(validate_instance_name("").is_ok());
    }

    #[test]
    fn named_instance_is_rejected_as_invalid_argument() {
        let err = validate_instance_name("tenant-a").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        // The offending value is echoed back to help a confused client.
        assert!(err.message().contains("tenant-a"));
    }
}
