//! FUSE input mount for the worker (Linux-only).
//!
//! See `crates/brokkr-worker/src/fuse/mod.rs` for the high-level
//! design and `docs/phase-3-plan.md` §5.5.1 for the M6b sub-plan.
//!
//! This module wires three layers:
//!
//! 1. **[`super::inode::InodeTable`]** — pre-walked Merkle DAG, all
//!    directory metadata served from RAM.
//! 2. **`BrokkrFs`** — the `fuser::Filesystem` implementation. Its
//!    callbacks are synchronous (called by the kernel via fuser's
//!    dedicated thread); CAS fetches inside `read` use
//!    [`tokio::runtime::Handle::block_on`] on the same tokio
//!    runtime that the rest of the worker uses.
//! 3. **[`InputMount`]** — RAII handle. Holds the
//!    `fuser::BackgroundSession`; dropping the handle unmounts and
//!    cleans the cache directory.
//!
//! Concurrency: per-mount [`tokio::sync::Semaphore`] caps concurrent
//! CAS fetches (default 16). Each file inode has its own
//! [`tokio::sync::OnceCell`] so concurrent reads of the same file
//! coalesce into one fetch.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use brokkr_cas::traits::Cas;
use brokkr_cas::CasError;
use brokkr_common::Digest;
use fuser::{
    BackgroundSession, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    ReplyOpen, Request,
};
use memmap2::Mmap;
use tokio::runtime::Handle;
use tokio::sync::{OnceCell, Semaphore};

use super::inode::{Inode, InodeKind, InodeTable};

const DEFAULT_FETCH_CONCURRENCY: usize = 16;
const TTL: Duration = Duration::from_secs(1);
const GENERATION: Generation = Generation(0);

/// Where to mount, where to cache, what to serve.
#[derive(Debug, Clone)]
pub struct InputMountSpec {
    /// Root of the REAPI input tree to expose.
    pub root_digest: Digest,
    /// Mountpoint directory. Must already exist and be empty.
    pub mountpoint: PathBuf,
    /// Local cache for lazily-fetched file content. Created on
    /// mount if missing; rm-rf'd by `InputMount::drop`.
    pub cache_dir: PathBuf,
}

/// Mount-time failure shapes.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    /// `/dev/fuse` is missing or not accessible.
    #[error("FUSE device unavailable: {0}")]
    Device(String),
    /// Underlying `mount(2)` failed.
    #[error("mount syscall failed: {0}")]
    Mount(io::Error),
    /// Walking the `Directory` Merkle DAG failed.
    #[error("input tree walk failed: {0}")]
    Tree(#[from] CasError),
    /// Mountpoint exists but isn't empty.
    #[error("mountpoint not empty: {0}")]
    Dirty(PathBuf),
    /// Failed to set up the per-mount cache directory.
    #[error("cache directory setup failed: {0}")]
    Cache(io::Error),
    /// Caller invoked [`mount`] outside a tokio runtime context.
    #[error("must be called from within a tokio runtime")]
    NoRuntime,
}

/// Live FUSE mount for one action's input tree.
///
/// Dropping the handle unmounts (via `fuser::BackgroundSession`'s
/// own Drop), waits for the FUSE thread to join, and then rm-rfs
/// the cache directory. If the join reports an error we shell out
/// to `fusermount -uz` (lazy unmount) and log a warning — the
/// cache rm-rf still runs.
pub struct InputMount {
    spec: InputMountSpec,
    session: Option<BackgroundSession>,
}

impl std::fmt::Debug for InputMount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputMount")
            .field("mountpoint", &self.spec.mountpoint)
            .field("cache_dir", &self.spec.cache_dir)
            .field("session_active", &self.session.is_some())
            .finish()
    }
}

impl InputMount {
    /// Mountpoint path, for the sandbox to bind into the action.
    pub fn mountpoint(&self) -> &Path {
        &self.spec.mountpoint
    }

    /// Cache directory holding lazily-fetched file content.
    pub fn cache_dir(&self) -> &Path {
        &self.spec.cache_dir
    }
}

impl Drop for InputMount {
    fn drop(&mut self) {
        // Order matters: unmount first (releases the kernel's hold
        // on the cache files we mmapped), then rm -rf the cache.
        if let Some(session) = self.session.take() {
            // Move umount + join onto a helper thread with a hard
            // timeout. `umount_and_join` blocks until the fuser bg
            // thread exits, which in turn waits for the kernel to
            // close `/dev/fuse` after the unmount syscall. On a
            // misbehaving mount that path can hang indefinitely, so
            // we fall back to `fusermount -uz` (lazy detach) if the
            // helper hasn't returned in 5 s.
            let mountpoint = self.spec.mountpoint.clone();
            let mountpoint_for_log = mountpoint.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let join = std::thread::Builder::new()
                .name("brokkr-fuse-umount".to_string())
                .spawn(move || {
                    let result = session.umount_and_join();
                    let _ = tx.send(result);
                });
            if join.is_err() {
                tracing::warn!(
                    mount = %mountpoint_for_log.display(),
                    "could not spawn unmount helper thread; falling back to lazy unmount"
                );
                lazy_unmount(&mountpoint);
            } else {
                match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(
                            mount = %mountpoint_for_log.display(),
                            error = %e,
                            "FUSE umount_and_join returned an error; lazy unmount fallback"
                        );
                        lazy_unmount(&mountpoint);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        tracing::warn!(
                            mount = %mountpoint_for_log.display(),
                            "FUSE umount_and_join timed out after 5s; lazy unmount fallback"
                        );
                        lazy_unmount(&mountpoint);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        tracing::warn!(
                            mount = %mountpoint_for_log.display(),
                            "FUSE unmount helper disconnected unexpectedly; lazy unmount fallback"
                        );
                        lazy_unmount(&mountpoint);
                    }
                }
            }
        }
        if let Err(e) = std::fs::remove_dir_all(&self.spec.cache_dir) {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::warn!(
                    cache = %self.spec.cache_dir.display(),
                    error = %e,
                    "failed to remove FUSE cache directory on drop"
                );
            }
        }
    }
}

fn lazy_unmount(mountpoint: &Path) {
    match std::process::Command::new("fusermount")
        .arg("-uz")
        .arg(mountpoint)
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::info!(mount = %mountpoint.display(), "lazy unmount completed");
        }
        Ok(out) => {
            tracing::warn!(
                mount = %mountpoint.display(),
                stderr = %String::from_utf8_lossy(&out.stderr),
                "fusermount -uz exited non-zero"
            );
        }
        Err(e) => {
            tracing::warn!(
                mount = %mountpoint.display(),
                error = %e,
                "fusermount -uz failed to spawn"
            );
        }
    }
}

/// Mount the input tree described by `spec`. Returns once the
/// kernel side of the FUSE mount is live; file content is fetched
/// lazily on the first `read(2)`.
#[tracing::instrument(skip(cas), fields(
    root = %spec.root_digest,
    mountpoint = %spec.mountpoint.display(),
))]
pub async fn mount(cas: Arc<dyn Cas>, spec: InputMountSpec) -> Result<InputMount, MountError> {
    validate_mountpoint(&spec.mountpoint)?;
    std::fs::create_dir_all(&spec.cache_dir).map_err(MountError::Cache)?;

    let table = Arc::new(InodeTable::build(cas.as_ref(), &spec.root_digest).await?);
    tracing::debug!(inodes = table.len(), "built inode table for FUSE mount");

    let handle = Handle::try_current().map_err(|_| MountError::NoRuntime)?;
    let semaphore = Arc::new(Semaphore::new(DEFAULT_FETCH_CONCURRENCY));
    let slots: Vec<Arc<OnceCell<Arc<Mmap>>>> = (0..table.len())
        .map(|_| Arc::new(OnceCell::new()))
        .collect();

    let fs = BrokkrFs {
        table,
        cas,
        cache_dir: spec.cache_dir.clone(),
        handle,
        semaphore,
        slots,
    };

    let options = vec![
        MountOption::FSName("brokkr-input".to_string()),
        MountOption::Subtype("brokkrfs".to_string()),
        MountOption::RO,
        MountOption::NoSuid,
        MountOption::NoDev,
    ];

    let mut config = fuser::Config::default();
    config.mount_options = options;
    let session =
        fuser::spawn_mount2(fs, &spec.mountpoint, &config).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => {
                MountError::Device(e.to_string())
            }
            _ => MountError::Mount(e),
        })?;

    Ok(InputMount {
        spec,
        session: Some(session),
    })
}

fn validate_mountpoint(path: &Path) -> Result<(), MountError> {
    let entries = std::fs::read_dir(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => MountError::Cache(e),
        _ => MountError::Mount(e),
    })?;
    if entries.into_iter().next().is_some() {
        return Err(MountError::Dirty(path.to_path_buf()));
    }
    Ok(())
}

struct BrokkrFs {
    table: Arc<InodeTable>,
    cas: Arc<dyn Cas>,
    cache_dir: PathBuf,
    handle: Handle,
    semaphore: Arc<Semaphore>,
    /// Parallel to `table.inodes` — one slot per inode, used only
    /// for file inodes. Directory and symlink slots stay empty.
    slots: Vec<Arc<OnceCell<Arc<Mmap>>>>,
}

impl BrokkrFs {
    fn slot(&self, ino: u64) -> Option<Arc<OnceCell<Arc<Mmap>>>> {
        if ino == 0 {
            return None;
        }
        self.slots.get((ino - 1) as usize).cloned()
    }
}

impl Filesystem for BrokkrFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.table.lookup(parent.into(), name) {
            Some(child_ino) => match self.table.get(child_ino) {
                Some(inode) => reply.entry(&TTL, &attr_for(inode), GENERATION),
                None => reply.error(Errno::ENOENT),
            },
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.table.get(ino.into()) {
            Some(inode) => reply.attr(&TTL, &attr_for(inode)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.table.get(ino.into()).map(|i| &i.kind) {
            Some(InodeKind::Link { target }) => reply.data(target.as_encoded_bytes()),
            Some(_) => reply.error(Errno::EINVAL),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.table.get(ino.into()).map(|i| &i.kind) {
            Some(InodeKind::Dir { .. }) => reply.opened(FileHandle(0), FopenFlags::empty()),
            Some(_) => reply.error(Errno::ENOTDIR),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino_u64: u64 = ino.into();
        let dir_inode = match self.table.get(ino_u64) {
            Some(i) => i,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let entries = match &dir_inode.kind {
            InodeKind::Dir { entries } => entries,
            _ => {
                reply.error(Errno::ENOTDIR);
                return;
            }
        };
        // Synthesize "." and "..". For the FS root, ".." refers to
        // the root itself (per FUSE convention).
        let mut all: Vec<(u64, FileType, std::ffi::OsString)> =
            Vec::with_capacity(2 + entries.len());
        all.push((ino_u64, FileType::Directory, ".".into()));
        all.push((ino_u64, FileType::Directory, "..".into()));
        for (name, child_ino) in entries {
            let kind = self
                .table
                .get(*child_ino)
                .map(file_type_of)
                .unwrap_or(FileType::RegularFile);
            all.push((*child_ino, kind, name.clone()));
        }
        all.sort_by(|a, b| a.2.cmp(&b.2));

        // `offset` is the index of the *next* entry to return.
        let start = offset as usize;
        for (i, (child_ino, kind, name)) in all.iter().enumerate().skip(start) {
            let next_offset = (i + 1) as u64;
            if reply.add(INodeNo(*child_ino), next_offset, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.table.get(ino.into()).map(|i| &i.kind) {
            Some(InodeKind::File { .. }) => reply.opened(FileHandle(0), FopenFlags::empty()),
            Some(InodeKind::Dir { .. }) => reply.error(Errno::EISDIR),
            Some(InodeKind::Link { .. }) => reply.error(Errno::EINVAL),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let ino_u64: u64 = ino.into();
        let inode = match self.table.get(ino_u64) {
            Some(i) => i,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let (digest, file_size) = match &inode.kind {
            InodeKind::File { digest, size, .. } => (digest.clone(), *size),
            _ => {
                reply.error(Errno::EISDIR);
                return;
            }
        };
        let slot = match self.slot(ino_u64) {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let cas = self.cas.clone();
        let cache_dir = self.cache_dir.clone();
        let sem = self.semaphore.clone();

        let mmap = self.handle.block_on(async move {
            slot.get_or_try_init(|| async {
                let _permit = sem
                    .acquire()
                    .await
                    .map_err(|e| FetchError::Internal(format!("semaphore closed: {e}")))?;
                fetch_and_mmap(cas.as_ref(), &digest, file_size, &cache_dir).await
            })
            .await
            .cloned()
        });

        let mmap = match mmap {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(ino = ino_u64, error = %e, "lazy fetch failed; returning EIO");
                reply.error(Errno::EIO);
                return;
            }
        };

        let data: &[u8] = &mmap;
        let start = (offset as usize).min(data.len());
        let end = start.saturating_add(size as usize).min(data.len());
        reply.data(&data[start..end]);
    }
}

#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error("cas: {0}")]
    Cas(#[from] CasError),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("size mismatch: digest claims {expected} bytes, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("internal: {0}")]
    Internal(String),
}

async fn fetch_and_mmap(
    cas: &dyn Cas,
    digest: &Digest,
    expected_size: u64,
    cache_dir: &Path,
) -> Result<Arc<Mmap>, FetchError> {
    let mut results = cas.batch_read_blobs(std::slice::from_ref(digest)).await?;
    let bytes = match results.pop() {
        Some(Ok(b)) => b,
        Some(Err(e)) => return Err(FetchError::Cas(e)),
        None => return Err(FetchError::Cas(CasError::NotFound(digest.clone()))),
    };
    if bytes.len() as u64 != expected_size {
        return Err(FetchError::SizeMismatch {
            expected: expected_size,
            actual: bytes.len() as u64,
        });
    }
    let path = cache_dir.join(digest.hash());
    std::fs::write(&path, &bytes)?;
    let file = std::fs::File::open(&path)?;
    // SAFETY: cache files are written-once and addressed by their
    // content digest; nothing else writes to `cache_dir` while the
    // mount is live, and the mmap is dropped before `InputMount`'s
    // Drop removes the cache directory.
    #[allow(unsafe_code)]
    let mmap = unsafe { Mmap::map(&file)? };
    Ok(Arc::new(mmap))
}

fn attr_for(inode: &Inode) -> FileAttr {
    let (kind, size, perm, nlink) = match &inode.kind {
        InodeKind::Dir { entries } => (
            FileType::Directory,
            0u64,
            0o555u16,
            (2 + entries.len()) as u32,
        ),
        InodeKind::File { size, exec, .. } => (
            FileType::RegularFile,
            *size,
            if *exec { 0o555 } else { 0o444 },
            1,
        ),
        InodeKind::Link { target } => (
            FileType::Symlink,
            target.as_encoded_bytes().len() as u64,
            0o777,
            1,
        ),
    };
    FileAttr {
        ino: INodeNo(inode.ino),
        size,
        blocks: size.div_ceil(512),
        atime: SystemTime::UNIX_EPOCH,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
        kind,
        perm,
        nlink,
        uid: nix::unistd::getuid().as_raw(),
        gid: nix::unistd::getgid().as_raw(),
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn file_type_of(inode: &Inode) -> FileType {
    match inode.kind {
        InodeKind::Dir { .. } => FileType::Directory,
        InodeKind::File { .. } => FileType::RegularFile,
        InodeKind::Link { .. } => FileType::Symlink,
    }
}
