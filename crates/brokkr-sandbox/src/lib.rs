//! Hermetic sandbox runtime for Brokkr workers.
//!
//! Built directly on Linux primitives — mount/PID/user/network namespaces,
//! cgroups v2, seccomp-bpf, capability dropping. **No Docker, runc, or
//! containerd dependency, ever.** Phase 2 lights this crate up; Phase 0–1
//! ship it as an empty stub.

#![deny(missing_docs)]
