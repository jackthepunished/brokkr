//! Compiles the vendored REAPI + googleapis protos into Rust modules via
//! `tonic-build`. Re-runs only when proto files change.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use a vendored protoc so the build needs no system-installed protobuf
    // compiler. Honor an externally-set $PROTOC if present.
    if std::env::var_os("PROTOC").is_none() {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    }

    let proto_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("protos");

    let protos = [
        "build/bazel/semver/semver.proto",
        "build/bazel/remote/execution/v2/remote_execution.proto",
        "google/bytestream/bytestream.proto",
        "google/longrunning/operations.proto",
        "google/rpc/status.proto",
    ];

    let proto_paths: Vec<PathBuf> = protos.iter().map(|p| proto_root.join(p)).collect();

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&proto_paths, &[proto_root.clone()])?;

    println!("cargo:rerun-if-changed={}", proto_root.display());
    Ok(())
}
