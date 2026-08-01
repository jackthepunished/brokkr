//! Mount-namespace setup inside the runner.
//!
//! Phase 2 / M3:
//!
//! 1. Make `/` propagation private so any mounts we add don't leak back
//!    to the host (defence in depth — the new mount namespace already
//!    isolates us, but `MS_PRIVATE` defeats slave-propagation tricks).
//! 2. Build the rootfs in a fresh tmpfs on an unpredictably-named
//!    `mkdtemp` directory under `/tmp` (see [`create_bootstrap_root`]).
//! 3. Apply `RootfsSpec.ro_binds` (bind, then remount read-only),
//!    `RootfsSpec.tmpfs`, and `RootfsSpec.symlinks`.
//! 4. `pivot_root` into the new rootfs, detach and `rmdir` the old one.
//!
//! `/sys` and `/dev` are *not* mounted yet — minimal device nodes need
//! either bind-mounts of host nodes or `mknod` (which user namespaces
//! restrict). M8 lights those up. `/proc` is mounted by the PID-1 init
//! in [`super::pidns`] (procfs reflects the *reader's* PID namespace,
//! so the mount has to happen inside the new pidns); we just create
//! the mount point here.

use std::io;
use std::path::{Path, PathBuf};

use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::unistd::pivot_root;

use super::nix_io;
use crate::config::RootfsSpec;

/// Build the sandbox rootfs in a tmpfs and `pivot_root` into it.
///
/// On entry, the runner must already be in its own mount namespace (see
/// [`super::userns`]). On exit, `/` is the sandbox rootfs and the host's
/// original mount tree is unreachable.
pub(super) fn setup_rootfs(spec: &RootfsSpec) -> io::Result<()> {
    // 1. Make root recursively private so nothing we mount escapes.
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(nix_io)?;

    // 2. Create the new rootfs and mount a tmpfs there. The mount point is a
    //    freshly-created, unpredictably-named directory on the host's /tmp;
    //    it's a transient bootstrap — pivot_root makes it `/`.
    let new_root = create_bootstrap_root()?;
    // NOSUID + NODEV harden the sandbox root: the action has no legitimate
    // need for setuid bits or device nodes on the tmpfs (NOEXEC is *not* set —
    // build actions execute binaries from the rootfs).
    mount(
        Some("brokkr-rootfs"),
        &new_root,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("size=64M,mode=0755"),
    )
    .map_err(nix_io)?;

    // 3. Apply each ro_bind: mkdir target, bind, remount read-only.
    for (host, sandbox) in &spec.ro_binds {
        if !host.exists() {
            // Skip silently. The worker's default allowlist may be
            // optimistic about /lib64 etc. that don't exist on every host.
            continue;
        }
        let target = inside(&new_root, sandbox);
        ensure_target_dir(host, &target)?;
        mount(
            Some(host),
            &target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(nix_io)?;
        // A second `mount` with `MS_REMOUNT | MS_BIND | MS_RDONLY` flips
        // the bind read-only. The mount(2) man page documents that
        // "fs-independent flags" only take effect on a remount of an
        // existing mount — exactly what we're doing. NOSUID + NODEV are
        // applied on the same remount as defence in depth against a
        // setuid binary or device node reachable through a host bind.
        mount(
            None::<&str>,
            &target,
            None::<&str>,
            MsFlags::MS_REMOUNT
                | MsFlags::MS_BIND
                | MsFlags::MS_REC
                | MsFlags::MS_RDONLY
                | MsFlags::MS_NOSUID
                | MsFlags::MS_NODEV,
            None::<&str>,
        )
        .map_err(nix_io)?;
    }

    // 4. tmpfs mounts (e.g. /tmp, /work, /etc).
    for (path, size) in &spec.tmpfs {
        let target = inside(&new_root, path);
        std::fs::create_dir_all(&target)?;
        let opts = format!("size={size},mode=0755");
        mount(
            Some("brokkr-tmpfs"),
            &target,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some(opts.as_str()),
        )
        .map_err(nix_io)?;
    }

    // 4b. Always create /proc inside the rootfs as a mount point — the
    //     init child mounts procfs onto it from inside the new PID
    //     namespace.
    {
        let proc_dir = inside(&new_root, Path::new("/proc"));
        std::fs::create_dir_all(&proc_dir)?;
    }

    // 5. Symlinks (e.g. /bin → /usr/bin) inside the tmpfs root. These have
    //    to be created *after* the targets exist (so the symlink resolves
    //    correctly post-pivot) and *before* pivot_root, while we still
    //    have the new_root prefix.
    for (link, target) in &spec.symlinks {
        let link_path = inside(&new_root, link);
        if let Some(parent) = link_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // If something already exists at link_path (e.g. the mkdir we
        // implicitly did for ro_binds), skip. Otherwise create.
        if !link_path.exists() {
            std::os::unix::fs::symlink(target, &link_path)?;
        }
    }

    // 6. pivot_root into the new rootfs.
    std::env::set_current_dir(&new_root)?;
    let old_root = PathBuf::from("old_root");
    if !old_root.exists() {
        std::fs::create_dir(&old_root)?;
    }
    pivot_root(".", &old_root).map_err(nix_io)?;
    std::env::set_current_dir("/")?;
    umount2("/old_root", MntFlags::MNT_DETACH).map_err(nix_io)?;
    std::fs::remove_dir("/old_root")?;

    Ok(())
}

/// Create the transient bootstrap directory the sandbox rootfs is mounted on.
///
/// `mkdtemp` picks a random suffix and creates the directory atomically with
/// `0700` permissions (`O_EXCL` semantics). This closes a `/tmp` symlink race:
/// the previous implementation used a PID-derived name (`brokkr-rootfs-<pid>`)
/// created with `create_dir_all`, so a local user who predicted the PID could
/// pre-create that path as a symlink and redirect the subsequent tmpfs mount.
fn create_bootstrap_root() -> io::Result<PathBuf> {
    nix::unistd::mkdtemp("/tmp/brokkr-rootfs-XXXXXX").map_err(nix_io)
}

/// Treat a sandbox path as relative to `new_root`. Both `/etc` and `etc`
/// resolve to `new_root/etc`.
fn inside(new_root: &Path, sandbox_path: &Path) -> PathBuf {
    let stripped = sandbox_path.strip_prefix("/").unwrap_or(sandbox_path);
    new_root.join(stripped)
}

/// Make sure `target` exists as the right kind of node so a bind-mount
/// can succeed: a directory if the host source is a directory, an empty
/// file otherwise. Bind mounts onto a missing path fail with `ENOENT`.
fn ensure_target_dir(host: &Path, target: &Path) -> io::Result<()> {
    let metadata = std::fs::metadata(host)?;
    if metadata.is_dir() {
        std::fs::create_dir_all(target)?;
    } else {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !target.exists() {
            std::fs::File::create(target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The bootstrap rootfs directory must be unpredictable, freshly created,
    /// and private — the properties that defeat a `/tmp` symlink race.
    #[test]
    #[allow(clippy::expect_used)]
    fn bootstrap_root_is_unique_private_and_a_real_dir() {
        let a = create_bootstrap_root().expect("mkdtemp a");
        let b = create_bootstrap_root().expect("mkdtemp b");

        // Distinct, random names — not a fixed/PID-derived path an attacker
        // could predict and pre-create.
        assert_ne!(a, b, "two bootstrap roots must not collide");

        for p in [&a, &b] {
            let meta = std::fs::symlink_metadata(p).expect("stat bootstrap root");
            // A real directory we created — not a pre-existing symlink that a
            // race winner planted and mkdtemp followed.
            assert!(meta.file_type().is_dir(), "{p:?} must be a directory");
            assert!(
                !meta.file_type().is_symlink(),
                "{p:?} must not be a symlink"
            );
            // mkdtemp creates with 0700, so no other local user can enter it.
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o700,
                "{p:?} must be 0700"
            );
        }

        let _ = std::fs::remove_dir(&a);
        let _ = std::fs::remove_dir(&b);
    }
}
