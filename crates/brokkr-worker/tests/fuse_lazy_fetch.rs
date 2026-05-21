//! M6b end-to-end: mount a CAS-backed tree via FUSE, read a
//! subset of files, assert that CAS was only touched for the
//! files we actually opened.
//!
//! Gated on `target_os = "linux"` and `/dev/fuse` accessibility.
//! `#[ignore]` by default so `cargo test --workspace` on hosts
//! without FUSE (or in containers that lack `/dev/fuse`) skips
//! cleanly. Run explicitly with:
//!
//! ```text
//! cargo test -p brokkr-worker --test fuse_lazy_fetch -- --ignored
//! ```

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::panic
)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use brokkr_cas::traits::{Cas, UpdateResult};
use brokkr_cas::tree::build_tree_into;
use brokkr_cas::{CasError, InMemoryCas};
use brokkr_common::Digest;
use brokkr_worker::fuse::mount::{mount, InputMountSpec};
use bytes::Bytes;

/// Wraps another `Cas` and counts every `batch_read_blobs` call by
/// digest. The integration test asserts that only the digests of
/// the files we actually opened were fetched.
struct CountingCas {
    inner: Arc<dyn Cas>,
    /// Total digests requested across all read calls (sum of slice
    /// lengths). Directory protos count too — we subtract those in
    /// the assertion.
    reads: AtomicUsize,
    /// File-content digests we expect; reads of these increment
    /// `file_reads`.
    file_digests: parking_lot::Mutex<Vec<Digest>>,
    file_reads: AtomicUsize,
}

impl CountingCas {
    fn new(inner: Arc<dyn Cas>, file_digests: Vec<Digest>) -> Self {
        Self {
            inner,
            reads: AtomicUsize::new(0),
            file_digests: parking_lot::Mutex::new(file_digests),
            file_reads: AtomicUsize::new(0),
        }
    }

    fn file_reads(&self) -> usize {
        self.file_reads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Cas for CountingCas {
    async fn find_missing_blobs(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
        self.inner.find_missing_blobs(digests).await
    }

    async fn batch_update_blobs(
        &self,
        blobs: Vec<(Digest, Bytes)>,
    ) -> Result<Vec<UpdateResult>, CasError> {
        self.inner.batch_update_blobs(blobs).await
    }

    async fn batch_read_blobs(
        &self,
        digests: &[Digest],
    ) -> Result<Vec<Result<Bytes, CasError>>, CasError> {
        self.reads.fetch_add(digests.len(), Ordering::SeqCst);
        let tracked = self.file_digests.lock().clone();
        for d in digests {
            if tracked.iter().any(|t| t == d) {
                self.file_reads.fetch_add(1, Ordering::SeqCst);
            }
        }
        self.inner.batch_read_blobs(digests).await
    }

    async fn list_digests(&self) -> Result<Vec<Digest>, CasError> {
        self.inner.list_digests().await
    }

    async fn delete_blob(&self, digest: &Digest) -> Result<(), CasError> {
        self.inner.delete_blob(digest).await
    }
}

fn fuse_available() -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    match std::fs::metadata("/dev/fuse") {
        Ok(meta) => {
            let mode = meta.permissions().mode();
            // Want rw for the test process; if perms look ok but
            // EACCES happens at mount time we'll skip later.
            mode & 0o600 == 0o600
        }
        Err(_) => false,
    }
}

fn write_tree(src: &Path) -> Vec<(String, Vec<u8>)> {
    let files = vec![
        ("alpha.txt".to_string(), b"alpha-contents-aaaaa".to_vec()),
        ("beta.bin".to_string(), b"\x00\x01\x02\x03beta".to_vec()),
        ("gamma.dat".to_string(), b"gamma-payload-xyz".to_vec()),
    ];
    for (name, bytes) in &files {
        std::fs::write(src.join(name), bytes).unwrap();
    }
    files
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires /dev/fuse access; run with --ignored"]
async fn fuse_mount_lazy_fetch() {
    if !fuse_available() {
        eprintln!("skipping: /dev/fuse not accessible to test process");
        return;
    }

    // 1. Build a 3-file tree in CAS.
    let src = tempfile::tempdir().unwrap();
    let files = write_tree(src.path());
    let backing: Arc<dyn Cas> = Arc::new(InMemoryCas::new());
    let root = build_tree_into(backing.as_ref(), src.path()).await.unwrap();

    let file_digests: Vec<Digest> = files.iter().map(|(_, b)| Digest::of(b)).collect();

    let counting: Arc<CountingCas> = Arc::new(CountingCas::new(backing.clone(), file_digests));
    let cas_for_mount: Arc<dyn Cas> = counting.clone();

    // 2. Mount it.
    let mountpoint = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let spec = InputMountSpec {
        root_digest: root,
        mountpoint: mountpoint.path().to_path_buf(),
        cache_dir: cache.path().to_path_buf(),
    };
    let mount_handle = match mount(cas_for_mount, spec).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("skipping: mount failed: {e}");
            return;
        }
    };

    // 3. Read only alpha + gamma. Beta stays untouched.
    let alpha = std::fs::read(mountpoint.path().join("alpha.txt")).unwrap();
    assert_eq!(alpha, b"alpha-contents-aaaaa");
    let gamma = std::fs::read(mountpoint.path().join("gamma.dat")).unwrap();
    assert_eq!(gamma, b"gamma-payload-xyz");

    // 4. Re-reading alpha must NOT cause another CAS fetch.
    let alpha2 = std::fs::read(mountpoint.path().join("alpha.txt")).unwrap();
    assert_eq!(alpha2, b"alpha-contents-aaaaa");

    let file_reads = counting.file_reads();
    assert_eq!(
        file_reads, 2,
        "expected exactly 2 file-content fetches (alpha + gamma), got {file_reads}"
    );

    // 5. Drop unmount cleanly.
    drop(mount_handle);
}
