# 0005 — No Docker (or other container runtime) for the sandbox

- **Status:** accepted
- **Date:** 2026-04-30
- **Deciders:** Brokkr maintainers

## Context

The sandbox runtime (`brokkr-sandbox`, Phase 2 — `docs/plan.md` §6.5,
§14) is the most security-sensitive subsystem in Brokkr. It executes
arbitrary, untrusted code on workers and is responsible for: filesystem
isolation, process isolation, network isolation, resource limits, syscall
restriction, and capability dropping.

It is also the most **educational** subsystem. The reason for building
Brokkr at all (`docs/plan.md` §1) is to internalize how distributed
systems work end-to-end. Sandboxing on Linux is a deep, well-documented
craft built on a small set of kernel primitives: namespaces, cgroups,
seccomp-bpf, capabilities. Understanding these primitives at the
syscall level is non-negotiable for anyone serious about systems work.

The temptation is obvious: shell out to `docker run` (or `runc`, or
`podman`) and inherit a battle-tested isolation story for ~200 lines of
glue code. The temptation must be refused.

## Decision

**Build the sandbox directly on Linux primitives.** No Docker, no runc,
no containerd, no podman, no OCI-runtime spec, no external container
runtime of any kind in `brokkr-sandbox`. This is encoded as **CLAUDE.md
hard rule #9** and is a project-level invariant, not an
implementation-detail preference.

Concrete primitives used (per `docs/plan.md` §6.5):

1. **Mount namespace + `pivot_root`** — minimal rootfs, bind-mounts of
   `/usr/bin`, `/lib`, `/lib64` from a configurable host allowlist;
   tmpfs for `/tmp` and `/work`.
2. **PID namespace** — sandboxed process is PID 1 with a small reaper
   for SIGCHLD.
3. **User namespace** — host UID `brokkr-sandbox` maps to UID 0 inside.
4. **Network namespace** — empty by default; opt-in network from the
   action's `Platform` constraints.
5. **cgroups v2** — per-action cgroup with CPU, memory, pids, io
   limits; OOM = structured action failure.
6. **seccomp-bpf** — default-deny syscall filter with an explicit
   allowlist (initial list in `docs/plan.md` §14 task 6).
7. **Capability dropping** — all capabilities removed by default.
8. **Determinism guards** — `LD_PRELOAD` blocked; `/proc/self/environ`
   minimized; hostname pinned to `brokkr-sandbox`; TZ forced to UTC;
   `SOURCE_DATE_EPOCH` injected.

Crate dependencies are limited to direct syscall bindings:
**`nix`** (general syscall wrappers), **`caps`** (capability
manipulation), and **`libseccomp`** (seccomp-bpf filter compilation).
No higher-level container abstraction.

## Alternatives considered

- **Docker (`docker run` per action).**
  - Pros: ubiquitous; mature; large policy ecosystem; "everyone knows
    Docker."
  - Cons: requires the Docker daemon (root, single point of failure on
    the host, host coupling); per-container startup latency 100–500 ms
    — exceeds our `<50 ms` sandbox-setup target (`docs/plan.md` §23);
    abdicates the educational core; image-pull semantics force us to
    pre-publish images for every tool version; `docker exec` semantics
    are loose around signals and reaping; project axiom (1) says we
    want to learn this layer.

- **runc (OCI runtime, no daemon).**
  - Pros: no daemon; widely deployed via Kubernetes; the de facto OCI
    reference; lower latency than Docker.
  - Cons: still abstracts away namespaces/cgroups behind `config.json`
    — we'd be marshaling JSON and missing the point. We would learn
    the OCI runtime spec, not the kernel primitives. The OCI hooks
    surface is a poor fit for our determinism guards.

- **podman.**
  - Pros: rootless; CLI-compatible with Docker; daemonless.
  - Cons: same abdication problem as Docker; adds another runtime
    surface to integrate with and test against; rootless mode has its
    own UID/GID gotchas that we would have to debug at a layer we
    don't own.

- **gVisor (userspace kernel).**
  - Pros: strongest isolation short of full virtualization; defends
    against kernel exploits that escape namespaces.
  - Cons: real performance cost (commonly 20–50 % on syscall-heavy
    workloads); adds a Go runtime dependency; still a higher-level
    abstraction over the primitives. **Worth revisiting in Phase 6+
    as an *additional* opt-in isolation tier**, not as a replacement
    for the primitive layer this ADR establishes.

- **Firecracker (microVM per action).**
  - Pros: VM-grade isolation; AWS Lambda's choice; fast for VMs.
  - Cons: ~125 ms boot floor even with snapshotting; designed for
    long-lived FaaS workers, not sub-second build steps; Linux
    KVM-only; overkill for the "compile a C file" baseline workload.

- **Bubblewrap.**
  - Pros: closest in spirit — a thin C wrapper over the same kernel
    primitives. Used by Flatpak.
  - Cons: still a wrapper; we want to write the wrapper. Reading its
    source is allowed and recommended.

- **NsJail / Bazel `linux-sandbox` / nsjail-style as a dependency.**
  - Pros: closest existing sandbox shaped like ours.
  - Cons: same abdication; using their code defeats the learning
    purpose. Reading their source is required and informs our design.

## Consequences

### Positive

- **Educational core preserved.** Phase 2 forces deep familiarity with
  `man 7 namespaces`, `man 7 cgroups`, `man 2 seccomp`. This compounds
  into Phase 5 (Raft) and beyond — the kernel-level fluency makes
  every later phase easier.
- **Lower latency.** No daemon, no container image pull, no
  marshaling through `config.json`. Our `<50 ms` sandbox-setup target
  (`docs/plan.md` §23) is achievable; with Docker it would not be.
- **Tighter security policy.** We can author seccomp filters per
  *action class* (compilers, test runners, ML training) rather than
  one-size-fits-all. Easy when we own the policy generator; awkward
  through OCI.
- **No host coupling.** Workers do not require a Docker daemon, a
  containerd socket, or any privileged service except their own
  user-namespace setup.
- **Fewer moving parts.** A sandbox bug is a Brokkr bug, not a
  Docker-version bug. We own the entire failure mode.

### Negative

- **More code.** Estimated 2–3 kLOC for the full sandbox vs. ~200 for
  an OCI-shim; expect a Phase 2 of significant size.
- **Steeper kernel knowledge curve.** Every contributor touching
  `brokkr-sandbox` needs to read the relevant `man` pages
  (Phase 2 reading list, `docs/plan.md` §28 Tier 2).
- **We own boundary bugs.** A sandbox escape is on us. Mitigated by
  the "evil action" test suite (`docs/plan.md` §14 task 10) — every
  isolation boundary tested with an attacker.
- **Linux-only workers.** Confirmed and intentional (`docs/plan.md`
  §2). The CLI and control plane stay portable; only worker
  binaries require Linux.

### Neutral

- **gVisor as Phase 6+ extension.** Adding a "high-isolation mode"
  later that wraps the primitive sandbox in gVisor does **not**
  invalidate this ADR. It layers above the primitive layer this
  decision establishes.
- **Phase 1 uses plain `tokio::process::Command`** with no
  isolation. That is documented as correct in the CLAUDE.md
  phase-awareness section, not a bug. The sandbox lands in Phase 2.
- **Crate dependency invariants.** `brokkr-sandbox` depends only on
  `brokkr-common` plus the primitive bindings (`nix`, `caps`,
  `libseccomp`). It does not depend on any HTTP/RPC code; the
  worker calls into it.

## References

- `docs/plan.md` §1 (Project axioms), §6.5 (Sandbox Runtime),
  §14 (Phase 2 tasks), §27 (Anti-patterns), §28 Tier 2
  (kernel reading list).
- CLAUDE.md hard rule #9.
- `man 7 namespaces`, `man 7 cgroups`, `man 2 unshare`, `man 2 seccomp`.
- Aleksa Sarai's container blog: <https://www.cyphar.com/blog>
- Bazel `linux-sandbox` source:
  <https://github.com/bazelbuild/bazel/tree/master/src/main/tools>
- gVisor architecture: <https://gvisor.dev/docs/architecture_guide/>
- NsJail: <https://github.com/google/nsjail>
- Bubblewrap: <https://github.com/containers/bubblewrap>
- Firecracker: <https://firecracker-microvm.github.io/>
