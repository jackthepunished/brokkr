//! FUSE-backed lazy input materialisation for the worker (Phase 3 M6b).
//!
//! See `docs/phase-3-plan.md` §5.5 + §5.5.1 for the design. The
//! short story: instead of copying a multi-GiB Bazel input tree to
//! the workspace before every action (the M6a `materialize_tree`
//! path), the worker exposes the tree as a FUSE mount. `getattr` /
//! `lookup` / `readdir` are served from an in-memory inode table
//! built from the `Directory` Merkle DAG; file content is fetched
//! from CAS only on the first `read(2)` and cached locally for the
//! lifetime of the action.
//!
//! ## Module layout
//!
//! * [`inode`]: pure data + DAG-walking builder. Platform-independent
//!   and fully unit-testable without `/dev/fuse`.
//! * [`mount`] (Linux only): the `fuser::Filesystem` implementation,
//!   the [`mount::InputMount`] RAII handle, and the public
//!   [`mount::mount`] entry point. Compiles to a stub on non-Linux
//!   that returns [`mount::MountError::Unsupported`].
//!
//! ## Lifetime
//!
//! One mount per running action. The worker:
//!
//! 1. Builds an [`mount::InputMountSpec`] from the action's input
//!    root digest and a job-scoped scratch directory.
//! 2. Calls [`mount::mount`] (async — pre-walks the DAG).
//! 3. Adds the mountpoint to the sandbox's `ro_binds`.
//! 4. Runs the action.
//! 5. Drops the [`mount::InputMount`] handle — unmount + cache
//!    cleanup happen synchronously in `Drop`.

pub mod inode;

#[cfg(target_os = "linux")]
pub mod mount;

#[cfg(not(target_os = "linux"))]
pub mod mount {
    //! Non-Linux stub. Mounting always fails with
    //! [`MountError::Unsupported`]; everything else compiles so the
    //! rest of the worker crate stays portable for the CLI/SDK
    //! build.
    use std::path::PathBuf;
    use std::sync::Arc;

    use brokkr_cas::traits::Cas;
    use brokkr_common::Digest;

    /// Mount-time failure shapes. See [`mount`].
    #[derive(Debug, thiserror::Error)]
    pub enum MountError {
        /// This platform doesn't support FUSE mounts.
        #[error("FUSE input mount is Linux-only; this host is {0}")]
        Unsupported(&'static str),
    }

    /// Inputs for [`mount`]: where to mount, where to cache, what
    /// to serve.
    #[derive(Debug, Clone)]
    pub struct InputMountSpec {
        /// Root of the REAPI input tree to expose.
        pub root_digest: Digest,
        /// Mountpoint directory. Must exist and be empty.
        pub mountpoint: PathBuf,
        /// Local cache directory for lazily-fetched file content.
        pub cache_dir: PathBuf,
    }

    /// RAII handle. On non-Linux this is uninhabitable.
    #[derive(Debug)]
    pub struct InputMount {
        _never: std::convert::Infallible,
    }

    /// Stub: always returns [`MountError::Unsupported`].
    pub async fn mount(
        _cas: Arc<dyn Cas>,
        _spec: InputMountSpec,
    ) -> Result<InputMount, MountError> {
        Err(MountError::Unsupported(std::env::consts::OS))
    }
}
