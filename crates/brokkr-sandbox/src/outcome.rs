//! Result types returned from a sandboxed action.

use std::time::Duration;

use bytes::Bytes;

/// Outcome of running an action in the sandbox.
#[derive(Debug)]
pub struct SandboxOutcome {
    /// How the action terminated.
    pub exit_status: ExitStatus,
    /// Captured stdout.
    pub stdout: Bytes,
    /// Captured stderr.
    pub stderr: Bytes,
    /// Resource accounting from the cgroup. M2 returns zeros; M6 fills it in.
    pub accounting: ResourceAccounting,
    /// Wall-clock breakdown of the sandbox lifecycle.
    pub timings: SandboxTimings,
}

/// How the action terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// `_exit(code)` was called (or main returned code). On Unix the kernel
    /// stores the low 8 bits of the wait status here.
    Exited(i32),
    /// Killed by a signal. M4 will distinguish `core_dumped` once we have
    /// access to the full wait status; for now the bool is stripped.
    Signaled {
        /// Signal number (e.g. 9 for SIGKILL).
        signal: i32,
    },
    /// Killed by the cgroup OOM killer. The action's writable outputs may
    /// be partial. M6 produces this status; M2 cannot detect OOM and will
    /// surface it as `Signaled { signal: 9 }`.
    OutOfMemory,
    /// Wall-clock limit hit. The runner sent SIGKILL. M8 produces this
    /// status; M2 surfaces it as `Signaled { signal: 9 }`.
    Timeout,
}

/// Cgroup-derived resource counters for one action. M2 returns zeros; M6
/// reads `cpu.stat`, `memory.peak`, `io.stat`, `pids.peak` after the action
/// exits and populates these fields.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceAccounting {
    /// User-mode CPU time consumed.
    pub cpu_user: Duration,
    /// Kernel-mode CPU time consumed.
    pub cpu_system: Duration,
    /// Peak resident set size in bytes.
    pub memory_peak_bytes: u64,
    /// Bytes read from block devices.
    pub io_read_bytes: u64,
    /// Bytes written to block devices.
    pub io_write_bytes: u64,
    /// Maximum concurrent PIDs observed.
    pub max_pids: u64,
}

/// Wall-clock breakdown of one sandbox lifecycle.
#[derive(Debug, Clone, Copy, Default)]
pub struct SandboxTimings {
    /// Time spent in host-side setup before the action's child process
    /// began running (cgroup creation, workspace staging, runner spawn).
    pub setup: Duration,
    /// Wall-clock time spent inside the runner from spawn to wait.
    pub execution: Duration,
    /// Time spent in host-side teardown after the runner exited.
    pub teardown: Duration,
}
