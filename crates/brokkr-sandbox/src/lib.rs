//! Hermetic sandbox runtime for Brokkr workers.
//!
//! Built directly on Linux primitives — mount/PID/user/network namespaces,
//! cgroups v2, seccomp-bpf, capability dropping. **No Docker, runc, or
//! containerd dependency, ever.** Phase 2 lights this crate up incrementally
//! per `docs/phase-2-plan.md`.

#![deny(missing_docs)]

pub mod host_check;
