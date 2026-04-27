//! Protobuf definitions and generated gRPC code for Brokkr.
//!
//! Modules mirror the protobuf package paths so the cross-package `super::…`
//! references emitted by `prost-build` resolve correctly. Convenience
//! re-exports at the bottom give callers shorter paths.

#![allow(missing_docs)] // generated code does not carry doc comments
#![allow(clippy::all)]

pub mod build {
    pub mod bazel {
        pub mod remote {
            pub mod execution {
                pub mod v2 {
                    tonic::include_proto!("build.bazel.remote.execution.v2");
                }
            }
        }
        pub mod semver {
            tonic::include_proto!("build.bazel.semver");
        }
    }
}

pub mod google {
    pub mod bytestream {
        tonic::include_proto!("google.bytestream");
    }
    pub mod longrunning {
        tonic::include_proto!("google.longrunning");
    }
    pub mod rpc {
        tonic::include_proto!("google.rpc");
    }
}

/// Bazel Remote Execution API v2 — convenience alias.
pub use build::bazel::remote::execution::v2 as reapi_v2;
/// Bazel SemVer types.
pub use build::bazel::semver;
/// `google.bytestream` — used for blob streaming alongside REAPI CAS.
pub use google::bytestream;
/// `google.longrunning` — operation tracking for `Execute` RPC.
pub use google::longrunning;
/// `google.rpc` — canonical status types.
pub use google::rpc;
