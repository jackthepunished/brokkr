//! Mount-namespace setup inside the runner.
//!
//! Phase 2 / M3:
//!
//! 1. Make `/` propagation private so any mounts we add don't leak back
//!    to the host (defence in depth — the new mount namespace already
//!    isolates us, but `MS_PRIVATE` defeats slave-propagation tricks).
//! 2. Build the rootfs in a fresh tmpfs at
//!    `/tmp/brokkr-rootfs-<pid>`.
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

    // 2. Create the new rootfs and mount a tmpfs there. The path lives in
    //    the host's /tmp namespace, which is fine because it's a transient
    //    bootstrap — pivot_root makes it `/`.
    let new_root = PathBuf::from(format!("/tmp/brokkr-rootfs-{}", std::process::id()));
    std::fs::create_dir_all(&new_root)?;
    mount(
        Some("brokkr-rootfs"),
        &new_root,
        Some("tmpfs"),
        MsFlags::empty(),
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
        // existing mount — exactly what we're doing.
        mount(
            None::<&str>,
            &target,
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_REC | MsFlags::MS_RDONLY,
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
            MsFlags::empty(),
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
