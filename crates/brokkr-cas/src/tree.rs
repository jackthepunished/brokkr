//! Materialize a CAS-stored input tree to a local directory.
//!
//! Phase 3 / M6a. A REAPI client uploads its input tree as a
//! Merkle DAG of `Directory` protos: each `Directory` enumerates
//! its files (with content digests), subdirectories (recursing),
//! and symlinks. The root of the tree is a single `Digest` that
//! identifies the whole input root.
//!
//! `materialize_tree(&cas, root_digest, target_dir)` walks the
//! DAG starting at `root_digest`, fetches every referenced file
//! from CAS, and writes a faithful copy of the tree to
//! `target_dir`. Symlinks become real symlinks; the executable
//! bit is honoured on Unix.
//!
//! This is the Phase 3 alternative to FUSE materialisation
//! (M6b): the entire tree is staged eagerly on disk before the
//! action runs. A Phase 3 worker can use this today; M6b will
//! add lazy fetching for trees big enough to make eager copying
//! wasteful.
//!
//! ## Tests-only ergonomic surface
//!
//! `build_tree_into` packs a directory tree into CAS and returns
//! the root digest — useful for tests that want to round-trip
//! through materialisation. Real workers build the tree
//! incrementally via the SDK's existing upload path; this helper
//! is only used by `brokkr-cas`'s own integration tests.

use std::path::Path;

use brokkr_common::Digest;
use brokkr_proto::reapi_v2 as rapi;
use bytes::Bytes;
use prost::Message;

use crate::error::CasError;
use crate::traits::Cas;

/// Fetch a `Directory` proto from CAS and decode it.
async fn fetch_directory(cas: &dyn Cas, digest: &Digest) -> Result<rapi::Directory, CasError> {
    let mut results = cas.batch_read_blobs(&[digest.clone()]).await?;
    let bytes = match results.pop() {
        Some(Ok(b)) => b,
        Some(Err(e)) => return Err(e),
        None => return Err(CasError::NotFound(digest.clone())),
    };
    rapi::Directory::decode(bytes.as_ref())
        .map_err(|e| CasError::Other(format!("Directory decode for {digest:?}: {e}")))
}

/// Walk the input tree rooted at `root_digest` and materialise
/// it under `target_dir`. The target directory must already
/// exist; its existing contents are NOT removed (the caller is
/// responsible for the workspace lifecycle, and we want to play
/// nicely with tmpfs mounts whose root we shouldn't touch).
///
/// Errors short-circuit the walk — a partial tree on disk on
/// error is the caller's mess to clean up. Materialise into a
/// scratch directory + rename atomically if you need strict
/// all-or-nothing semantics.
#[tracing::instrument(skip(cas), fields(root = %root_digest))]
pub async fn materialize_tree(
    cas: &dyn Cas,
    root_digest: &Digest,
    target_dir: &Path,
) -> Result<MaterializationStats, CasError> {
    let mut stats = MaterializationStats::default();
    materialize_directory(cas, root_digest, target_dir, &mut stats).await?;
    Ok(stats)
}

/// Per-pass counters surfaced from a successful
/// [`materialize_tree`]. Useful for `/metrics` and for tests
/// that want to assert "exactly N files written".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaterializationStats {
    /// Files written to disk (excludes symlinks).
    pub files: usize,
    /// Subdirectories created.
    pub dirs: usize,
    /// Symlinks created.
    pub symlinks: usize,
    /// Total bytes of file content written.
    pub bytes: u64,
}

/// REAPI v2 §`FileNode.name` / §`DirectoryNode.name` / §`SymlinkNode.name`
/// require a single non-empty path segment: no `/`, no NUL, not `.`
/// or `..`. Returns `None` if the name is safe to join onto a parent
/// directory, or `Some(reason)` describing why it was rejected. The
/// reason string is a `&'static str` so the call site can fold it into
/// a `CasError::Other` without allocating.
///
/// Path-traversal guard for issue #141: `Path::join` is unsafe against
/// attacker input — a `name` of `..` walks out of the base, and an
/// absolute `name` *replaces* the base entirely. A REAPI `Directory`
/// proto is an opaque CAS blob, so any client that can `BatchUpdateBlobs`
/// can plant the payload. The check has to be here, in the decode
/// path; the encode path (`pack_directory` below) reads from a host
/// filesystem and is already safe by construction.
///
/// Note: `..` is rejected as a literal single-segment name (REAPI
/// disallows it). A name like `../escape.txt` is rejected by the
/// slash check first because it's not a single path segment at all —
/// both are rejected, with different reasons.
fn validate_node_name(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("empty");
    }
    if name == "." || name == ".." {
        return Some("dot or dotdot");
    }
    if name.contains('/') {
        return Some("contains '/'");
    }
    if name.contains('\0') {
        return Some("contains NUL");
    }
    None
}

// Recursive walker. Boxed because async + recursion isn't a
// thing without it; the alternative is an explicit work stack
// (more code, same behaviour). The recursion depth is bounded
// by the tree's depth — bazel tends to keep that shallow (<20),
// so stack overflow isn't a practical concern.
fn materialize_directory<'a>(
    cas: &'a dyn Cas,
    digest: &'a Digest,
    dir: &'a Path,
    stats: &'a mut MaterializationStats,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), CasError>> + Send + 'a>> {
    Box::pin(async move {
        let directory = fetch_directory(cas, digest).await?;

        for file_node in &directory.files {
            if let Some(reason) = validate_node_name(&file_node.name) {
                return Err(CasError::Other(format!(
                    "FileNode {:?}: invalid name ({reason})",
                    file_node.name
                )));
            }
            let digest = match &file_node.digest {
                Some(d) => Digest::new(d.hash.clone(), d.size_bytes).map_err(|e| {
                    CasError::Other(format!("invalid file digest {}: {e}", file_node.name))
                })?,
                None => {
                    return Err(CasError::Other(format!(
                        "FileNode {:?} missing digest",
                        file_node.name
                    )))
                }
            };
            let path = dir.join(&file_node.name);
            write_file(cas, &digest, &path, file_node.is_executable).await?;
            stats.files += 1;
            stats.bytes = stats.bytes.saturating_add(digest.size_bytes() as u64);
        }

        for symlink in &directory.symlinks {
            if let Some(reason) = validate_node_name(&symlink.name) {
                return Err(CasError::Other(format!(
                    "SymlinkNode {:?}: invalid name ({reason})",
                    symlink.name
                )));
            }
            let path = dir.join(&symlink.name);
            // The target is interpreted as-is per REAPI v2 §SymlinkNode.
            if path.exists() || path.is_symlink() {
                std::fs::remove_file(&path).map_err(CasError::Io)?;
            }
            std::os::unix::fs::symlink(&symlink.target, &path).map_err(CasError::Io)?;
            stats.symlinks += 1;
        }

        for child in &directory.directories {
            if let Some(reason) = validate_node_name(&child.name) {
                return Err(CasError::Other(format!(
                    "DirectoryNode {:?}: invalid name ({reason})",
                    child.name
                )));
            }
            let child_path = dir.join(&child.name);
            std::fs::create_dir_all(&child_path).map_err(CasError::Io)?;
            stats.dirs += 1;
            let child_digest = match &child.digest {
                Some(d) => Digest::new(d.hash.clone(), d.size_bytes).map_err(|e| {
                    CasError::Other(format!("invalid directory digest {}: {e}", child.name))
                })?,
                None => {
                    return Err(CasError::Other(format!(
                        "DirectoryNode {:?} missing digest",
                        child.name
                    )))
                }
            };
            materialize_directory(cas, &child_digest, &child_path, stats).await?;
        }

        Ok(())
    })
}

async fn write_file(
    cas: &dyn Cas,
    digest: &Digest,
    path: &Path,
    is_executable: bool,
) -> Result<(), CasError> {
    let mut results = cas.batch_read_blobs(&[digest.clone()]).await?;
    let bytes = match results.pop() {
        Some(Ok(b)) => b,
        Some(Err(e)) => return Err(e),
        None => return Err(CasError::NotFound(digest.clone())),
    };
    std::fs::write(path, &bytes).map_err(CasError::Io)?;
    if is_executable {
        use std::os::unix::fs::PermissionsExt as _;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(path, perms).map_err(CasError::Io)?;
    }
    Ok(())
}

/// Test helper: pack the contents of `source_dir` into `cas` as
/// a REAPI Directory Merkle DAG. Returns the root `Digest`.
///
/// Skips entries that aren't a regular file, directory, or
/// symlink. Symlink targets are stored as-is (relative or
/// absolute) — no canonicalisation.
///
/// Pub for the in-crate `tests/` integration tests; not part of
/// the stable public API.
pub async fn build_tree_into(cas: &dyn Cas, source_dir: &Path) -> Result<Digest, CasError> {
    pack_directory(cas, source_dir).await
}

fn pack_directory<'a>(
    cas: &'a dyn Cas,
    dir: &'a Path,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Digest, CasError>> + Send + 'a>> {
    Box::pin(async move {
        let mut directory = rapi::Directory::default();
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(CasError::Io)?
            .filter_map(|r| r.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = std::fs::symlink_metadata(&path).map_err(CasError::Io)?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                let target = std::fs::read_link(&path).map_err(CasError::Io)?;
                directory.symlinks.push(rapi::SymlinkNode {
                    name,
                    target: target.to_string_lossy().into_owned(),
                    ..Default::default()
                });
            } else if file_type.is_dir() {
                let child_digest = pack_directory(cas, &path).await?;
                directory.directories.push(rapi::DirectoryNode {
                    name,
                    digest: Some(rapi::Digest {
                        hash: child_digest.hash().to_string(),
                        size_bytes: child_digest.size_bytes(),
                    }),
                });
            } else if file_type.is_file() {
                let bytes = std::fs::read(&path).map_err(CasError::Io)?;
                let digest = Digest::of(&bytes);
                cas.batch_update_blobs(vec![(digest.clone(), Bytes::from(bytes))])
                    .await?;
                let is_executable = {
                    use std::os::unix::fs::PermissionsExt as _;
                    metadata.permissions().mode() & 0o111 != 0
                };
                directory.files.push(rapi::FileNode {
                    name,
                    digest: Some(rapi::Digest {
                        hash: digest.hash().to_string(),
                        size_bytes: digest.size_bytes(),
                    }),
                    is_executable,
                    ..Default::default()
                });
            }
        }

        let mut buf = Vec::with_capacity(directory.encoded_len());
        directory
            .encode(&mut buf)
            .map_err(|e| CasError::Other(format!("Directory encode: {e}")))?;
        let digest = Digest::of(&buf);
        cas.batch_update_blobs(vec![(digest.clone(), Bytes::from(buf))])
            .await?;
        Ok(digest)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::in_memory::InMemoryCas;

    #[tokio::test]
    async fn round_trip_empty_directory() {
        let cas = InMemoryCas::new();
        let src = tempfile::tempdir().unwrap();
        let root = build_tree_into(&cas, src.path()).await.unwrap();

        let dst = tempfile::tempdir().unwrap();
        let stats = materialize_tree(&cas, &root, dst.path()).await.unwrap();
        assert_eq!(stats, MaterializationStats::default());
    }

    #[tokio::test]
    async fn round_trip_flat_files() {
        let cas = InMemoryCas::new();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(src.path().join("b.bin"), b"\x00\x01\x02").unwrap();
        let root = build_tree_into(&cas, src.path()).await.unwrap();

        let dst = tempfile::tempdir().unwrap();
        let stats = materialize_tree(&cas, &root, dst.path()).await.unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(stats.dirs, 0);
        assert_eq!(stats.bytes, 8); // 5 + 3
        assert_eq!(std::fs::read(dst.path().join("a.txt")).unwrap(), b"hello");
        assert_eq!(
            std::fs::read(dst.path().join("b.bin")).unwrap(),
            b"\x00\x01\x02"
        );
    }

    #[tokio::test]
    async fn round_trip_nested_tree() {
        let cas = InMemoryCas::new();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("top.txt"), b"top").unwrap();
        let sub = src.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.txt"), b"inner").unwrap();
        let deeper = sub.join("deeper");
        std::fs::create_dir(&deeper).unwrap();
        std::fs::write(deeper.join("bottom.txt"), b"bottom").unwrap();

        let root = build_tree_into(&cas, src.path()).await.unwrap();
        let dst = tempfile::tempdir().unwrap();
        let stats = materialize_tree(&cas, &root, dst.path()).await.unwrap();
        assert_eq!(stats.files, 3);
        assert_eq!(stats.dirs, 2);
        assert_eq!(
            std::fs::read(dst.path().join("sub/deeper/bottom.txt")).unwrap(),
            b"bottom"
        );
    }

    #[tokio::test]
    async fn round_trip_preserves_executable_bit() {
        let cas = InMemoryCas::new();
        let src = tempfile::tempdir().unwrap();
        let script = src.path().join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = build_tree_into(&cas, src.path()).await.unwrap();

        let dst = tempfile::tempdir().unwrap();
        materialize_tree(&cas, &root, dst.path()).await.unwrap();
        let perms = std::fs::metadata(dst.path().join("run.sh"))
            .unwrap()
            .permissions();
        assert_ne!(
            perms.mode() & 0o111,
            0,
            "executable bit lost across materialisation"
        );
    }

    #[tokio::test]
    async fn round_trip_preserves_symlink() {
        let cas = InMemoryCas::new();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("target.txt"), b"linked").unwrap();
        std::os::unix::fs::symlink("target.txt", src.path().join("link")).unwrap();

        let root = build_tree_into(&cas, src.path()).await.unwrap();
        let dst = tempfile::tempdir().unwrap();
        let stats = materialize_tree(&cas, &root, dst.path()).await.unwrap();
        assert_eq!(stats.symlinks, 1);
        let link = dst.path().join("link");
        assert!(link.is_symlink());
        assert_eq!(
            std::fs::read_link(&link).unwrap().to_string_lossy(),
            "target.txt"
        );
    }

    #[tokio::test]
    async fn missing_directory_digest_propagates_not_found() {
        let cas = InMemoryCas::new();
        let bogus = Digest::of(b"not actually in cas");
        let dst = tempfile::tempdir().unwrap();
        let err = materialize_tree(&cas, &bogus, dst.path())
            .await
            .unwrap_err();
        assert!(matches!(err, CasError::NotFound(_)));
    }

    // -----------------------------------------------------------------
    // Path-traversal guard for issue #141. The four tests below hand-
    // build a `Directory` proto with an attacker-controlled `name` on
    // a `FileNode` and assert that `materialize_tree` rejects it
    // before the file is written. The `build_tree_into` helper can't
    // produce these (it reads from a host FS, which only yields single-
    // segment names), so we craft the proto and its blob by hand and
    // push them into the CAS ourselves.
    // -----------------------------------------------------------------

    /// Encode `directory` and store the bytes (and any file blobs it
    /// references) into `cas`, returning the root `Digest` of the
    /// encoded `Directory` proto. Caller passes a fully-formed
    /// `rapi::Directory`; we do not validate the name fields here —
    /// that's what the tests are for.
    async fn store_malicious_directory(cas: &InMemoryCas, directory: &rapi::Directory) -> Digest {
        // Upload every file blob the directory references.
        for file in &directory.files {
            if let Some(d) = &file.digest {
                if let Ok(digest) = Digest::new(d.hash.clone(), d.size_bytes) {
                    // The tests only need the blob to exist in CAS so
                    // `batch_read_blobs` doesn't return NotFound; an
                    // empty body is fine.
                    cas.batch_update_blobs(vec![(digest, Bytes::new())])
                        .await
                        .unwrap();
                }
            }
        }
        let mut buf = Vec::with_capacity(directory.encoded_len());
        directory.encode(&mut buf).unwrap();
        let root = Digest::of(&buf);
        cas.batch_update_blobs(vec![(root.clone(), Bytes::from(buf))])
            .await
            .unwrap();
        root
    }

    /// Build a `FileNode` whose `name` is the attacker's payload. The
    /// blob digest is the digest of an empty body (the test never
    /// reads the bytes — the rejection happens before the file write).
    fn malicious_file_node(name: &str) -> rapi::FileNode {
        let empty = Digest::of(b"");
        rapi::FileNode {
            name: name.to_string(),
            digest: Some(rapi::Digest {
                hash: empty.hash().to_string(),
                size_bytes: empty.size_bytes(),
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn materialize_tree_rejects_dotdot_name() {
        // Issue #141: a FileNode whose name is the literal `..` would
        // walk out of the staging root under `Path::join`. REAPI v2
        // disallows `.` and `..` as single-segment names; reject them
        // before the join so the operator's log line is specific.
        let cas = InMemoryCas::new();
        let mut directory = rapi::Directory::default();
        directory.files.push(malicious_file_node(".."));
        let root = store_malicious_directory(&cas, &directory).await;

        let dst = tempfile::tempdir().unwrap();
        let err = materialize_tree(&cas, &root, dst.path()).await.unwrap_err();
        assert!(
            matches!(err, CasError::Other(ref msg) if msg.contains("dot or dotdot")),
            "expected a dotdot rejection, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn materialize_tree_rejects_dot_name() {
        // `FileNode { name: ".", .. }` is also disallowed by REAPI v2
        // (`.` is a single-segment reference to the current directory;
        // the join is a no-op for the file write but the entry would
        // shadow the staging root in the next read). Reject for
        // consistency with the dotdot guard.
        let cas = InMemoryCas::new();
        let mut directory = rapi::Directory::default();
        directory.files.push(malicious_file_node("."));
        let root = store_malicious_directory(&cas, &directory).await;

        let dst = tempfile::tempdir().unwrap();
        let err = materialize_tree(&cas, &root, dst.path()).await.unwrap_err();
        assert!(
            matches!(err, CasError::Other(ref msg) if msg.contains("dot or dotdot")),
            "expected a dot rejection, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn materialize_tree_rejects_name_with_slash() {
        // Issue #141 reproduction recipe. `Path::join` with a `..`-led
        // name walks out of the base; with an absolute name it
        // *replaces* the base. Either way, the file is written outside
        // the staging directory. Reject at the gate; the reason is
        // "contains '/'" because the name is not a single path
        // segment (REAPI conformance), distinct from the dotdot case.
        let cas = InMemoryCas::new();
        let mut directory = rapi::Directory::default();
        directory.files.push(malicious_file_node("../escape.txt"));
        let root = store_malicious_directory(&cas, &directory).await;

        let dst = tempfile::tempdir().unwrap();
        let err = materialize_tree(&cas, &root, dst.path()).await.unwrap_err();
        assert!(
            matches!(err, CasError::Other(ref msg) if msg.contains("contains '/'")),
            "expected a slash-in-name rejection, got: {err:?}"
        );
        assert!(
            !dst.path().parent().unwrap().join("escape.txt").exists(),
            "staging directory escaped via dotdot-prefixed name"
        );
    }

    #[tokio::test]
    async fn materialize_tree_rejects_absolute_name() {
        // `Path::join("/tmp/staging", "/etc/passwd")` returns
        // `/etc/passwd` — the base is replaced entirely. An attacker
        // who can set `name = "/etc/passwd"` would otherwise overwrite
        // the host file. The slash check rejects before the join.
        let cas = InMemoryCas::new();
        let mut directory = rapi::Directory::default();
        directory.files.push(malicious_file_node("/etc/passwd"));
        let root = store_malicious_directory(&cas, &directory).await;

        let dst = tempfile::tempdir().unwrap();
        let err = materialize_tree(&cas, &root, dst.path()).await.unwrap_err();
        assert!(
            matches!(err, CasError::Other(ref msg) if msg.contains("contains '/'")),
            "expected a slash-in-name rejection, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn materialize_tree_rejects_empty_name() {
        // Empty names are not single segments; `Path::join` with an
        // empty `right` returns the base unchanged, which is a
        // confused-deputy risk (the file would shadow the staging
        // directory itself). Reject at the gate.
        let cas = InMemoryCas::new();
        let mut directory = rapi::Directory::default();
        directory.files.push(malicious_file_node(""));
        let root = store_malicious_directory(&cas, &directory).await;

        let dst = tempfile::tempdir().unwrap();
        let err = materialize_tree(&cas, &root, dst.path()).await.unwrap_err();
        assert!(
            matches!(err, CasError::Other(ref msg) if msg.contains("empty")),
            "expected an empty-name rejection, got: {err:?}"
        );
    }
}
