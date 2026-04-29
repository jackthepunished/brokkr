//! User-namespace setup inside the runner.
//!
//! Phase 2 / M3 uses the simplest unprivileged form: a single mapping of
//! length 1 (`0 <host_uid> 1`), so the runner is UID 0 *inside* the
//! sandbox and the worker's real UID *outside*. This is enough to
//! synthesise the `CAP_SYS_ADMIN` we need for `mount(2)` and `pivot_root(2)`
//! without requiring `newuidmap` / `/etc/subuid`.
//!
//! Sequencing nuances captured here:
//!
//! - `unshare(CLONE_NEWUSER | CLONE_NEWNS)` in a single call. The kernel
//!   creates the user namespace first, so the new mount namespace is born
//!   with the caller already privileged in it (per `clone(2)`).
//! - `/proc/self/setgroups` must be written `deny` *before* `gid_map` is
//!   writable under an unprivileged user namespace. Forgetting this is
//!   the most common failure mode.
//! - With a length-1 map, the mapping shape `0 <host_uid> 1` is the only
//!   form an unprivileged process is allowed to write directly; a wider
//!   range needs `newuidmap` / `newgidmap` with `/etc/subuid` entries.

use std::fs::OpenOptions;
use std::io::{self, Write};

use nix::sched::{unshare, CloneFlags};

use super::nix_io;

/// Single-mapping spec for the new user namespace.
#[derive(Debug, Clone, Copy)]
pub(super) struct UidGidMap {
    /// Outside (host) UID — typically the current runner's real UID.
    pub host_uid: u32,
    /// Outside (host) GID — typically the current runner's real GID.
    pub host_gid: u32,
}

/// Enter a fresh user namespace plus a fresh mount namespace, then write
/// the uid / gid maps so the runner has full capabilities inside both.
pub(super) fn setup_user_and_mount_namespaces(map: UidGidMap) -> io::Result<()> {
    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS).map_err(nix_io)?;

    // setgroups must be 'deny' before gid_map is writable in an
    // unprivileged user namespace (kernel ≥ 3.19). The file may not exist
    // on very old kernels — we fail loudly there.
    let mut sg = OpenOptions::new()
        .write(true)
        .open("/proc/self/setgroups")?;
    sg.write_all(b"deny")?;
    drop(sg);

    let uid_line = format!("0 {} 1\n", map.host_uid);
    let mut um = OpenOptions::new().write(true).open("/proc/self/uid_map")?;
    um.write_all(uid_line.as_bytes())?;
    drop(um);

    let gid_line = format!("0 {} 1\n", map.host_gid);
    let mut gm = OpenOptions::new().write(true).open("/proc/self/gid_map")?;
    gm.write_all(gid_line.as_bytes())?;
    drop(gm);

    Ok(())
}
