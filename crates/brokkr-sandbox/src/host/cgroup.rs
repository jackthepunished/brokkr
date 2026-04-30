//! Per-action cgroup v2: create, set limits, attach pid, read accounting,
//! detect OOM, clean up.
//!
//! Phase 2 / M6. The host worker is expected to own a writable
//! delegated slice — typically `/sys/fs/cgroup/brokkr.slice/` set up
//! by `scripts/install-cgroup-slice.sh` or via systemd's `Delegate=yes`.
//! For each action the host creates a leaf cgroup
//! `<slice>/action-<uuid>/`, writes the configured limits, attaches the
//! runner process to it (so all of init / the action / its children
//! inherit it), and reads accounting back when the action finishes.
//!
//! ## Why we don't enable subtree_control on the action cgroup
//!
//! Cgroup v2 enables controllers in *children* via the parent's
//! `cgroup.subtree_control`. The action cgroup has no children of its
//! own (it's the leaf where processes live), so we touch
//! `cgroup.subtree_control` only on the *slice* (and only as part of
//! one-shot host setup, not every action). The
//! `scripts/install-cgroup-slice.sh` script does that step.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::ResourceLimits;
use crate::outcome::ResourceAccounting;

/// Per-action cgroup. RAII: drop removes the leaf directory.
#[derive(Debug)]
pub(super) struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    /// Create `<parent>/<name>` and write the configured limits. The
    /// leaf has no children, so we never touch `subtree_control` on it.
    pub(super) fn create(parent: &Path, name: &str, limits: &ResourceLimits) -> io::Result<Self> {
        let path = parent.join(name);
        fs::create_dir(&path)?;
        let cg = Cgroup { path };
        cg.apply_limits(limits)?;
        Ok(cg)
    }

    fn apply_limits(&self, limits: &ResourceLimits) -> io::Result<()> {
        if let Some(mem) = limits.memory_bytes {
            self.write_attr("memory.max", &mem.to_string())?;
            // Disable swap accounting so the action can't dodge the
            // memory cap by swapping. memory.swap.max may not exist on
            // kernels without the swap controller; ignore ENOENT so a
            // missing swap controller doesn't block the run.
            if let Err(e) = self.write_attr("memory.swap.max", "0") {
                if e.kind() != io::ErrorKind::NotFound {
                    return Err(e);
                }
            }
        }
        if let Some(pids) = limits.pids_max {
            self.write_attr("pids.max", &pids.to_string())?;
        }
        if let Some(cpu_milli) = limits.cpu_milli {
            // cpu.max is "<quota_us> <period_us>"; period 100_000us is
            // the kernel default, and quota / period gives the fraction
            // of a core. 1000 milli-CPU = one full core.
            const PERIOD_US: u64 = 100_000;
            let quota_us = cpu_milli.saturating_mul(PERIOD_US) / 1_000;
            self.write_attr("cpu.max", &format!("{quota_us} {PERIOD_US}"))?;
        }
        Ok(())
    }

    /// Write `pid` into `cgroup.procs`, moving the process (and its
    /// future descendants) into this cgroup.
    pub(super) fn attach(&self, pid: u32) -> io::Result<()> {
        self.write_attr("cgroup.procs", &pid.to_string())
    }

    /// Read every accounting counter we know about. Missing files
    /// (older kernels, controllers not enabled) leave the corresponding
    /// field at its `Default::default()` value rather than failing the
    /// whole run.
    pub(super) fn accounting(&self) -> ResourceAccounting {
        let mut acc = ResourceAccounting::default();

        if let Ok(stat) = fs::read_to_string(self.path.join("cpu.stat")) {
            for line in stat.lines() {
                let mut parts = line.split_whitespace();
                let key = parts.next();
                let val = parts.next().and_then(|s| s.parse::<u64>().ok());
                match (key, val) {
                    (Some("user_usec"), Some(v)) => acc.cpu_user = Duration::from_micros(v),
                    (Some("system_usec"), Some(v)) => acc.cpu_system = Duration::from_micros(v),
                    _ => {}
                }
            }
        }

        if let Ok(s) = fs::read_to_string(self.path.join("memory.peak")) {
            acc.memory_peak_bytes = s.trim().parse().unwrap_or(0);
        }

        if let Ok(s) = fs::read_to_string(self.path.join("pids.peak")) {
            acc.max_pids = s.trim().parse().unwrap_or(0);
        }

        // `io.stat` lines look like:
        //   "8:0 rbytes=4096 wbytes=8192 rios=2 wios=1 dbytes=0 dios=0"
        // Sum across devices.
        if let Ok(s) = fs::read_to_string(self.path.join("io.stat")) {
            for line in s.lines() {
                for token in line.split_whitespace().skip(1) {
                    if let Some(v) = token
                        .strip_prefix("rbytes=")
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        acc.io_read_bytes = acc.io_read_bytes.saturating_add(v);
                    } else if let Some(v) = token
                        .strip_prefix("wbytes=")
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        acc.io_write_bytes = acc.io_write_bytes.saturating_add(v);
                    }
                }
            }
        }

        acc
    }

    /// True if the kernel OOM-killer fired inside this cgroup. Read
    /// `memory.events`'s `oom_kill` counter; missing file or zero count
    /// → `false`.
    pub(super) fn was_oom_killed(&self) -> bool {
        let Ok(s) = fs::read_to_string(self.path.join("memory.events")) else {
            return false;
        };
        for line in s.lines() {
            let mut parts = line.split_whitespace();
            if parts.next() == Some("oom_kill") {
                return parts
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .is_some_and(|n| n > 0);
            }
        }
        false
    }

    /// Best-effort SIGKILL of every PID still inside this cgroup. Used
    /// on wall-clock timeout so `Drop`'s `rmdir` can succeed.
    pub(super) fn kill_all(&self) -> io::Result<()> {
        // cgroup v2 has cgroup.kill since 5.14 — single write of "1"
        // SIGKILLs every process in the cgroup atomically.
        match self.write_attr("cgroup.kill", "1") {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Pre-5.14 fallback: walk cgroup.procs and kill each.
                let procs = fs::read_to_string(self.path.join("cgroup.procs"))?;
                for pid_str in procs.split_whitespace() {
                    if let Ok(pid) = pid_str.parse::<i32>() {
                        // SAFETY: kill is async-signal-safe; passing an
                        // out-of-range pid returns ESRCH which we ignore.
                        #[allow(unsafe_code)]
                        unsafe {
                            nix::libc::kill(pid, nix::libc::SIGKILL);
                        }
                    }
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn write_attr(&self, name: &str, content: &str) -> io::Result<()> {
        fs::write(self.path.join(name), content)
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        // Best-effort cleanup. If processes are still inside, this
        // returns EBUSY — the caller should have called `kill_all`
        // before drop in that case.
        let _ = fs::remove_dir(&self.path);
    }
}
