# Phase 2 — Hermetic Sandboxing

- **Status:** in progress
- **Plan:** `docs/phase-2-plan.md`
- **Started:** 2026-04-30

This journal accumulates short retrospectives as each milestone (M1–M9)
lands. Each milestone is a single PR; each section here is the post-merge
debrief.

## M1 — `feat/phase2-host-check`

- **Date:** 2026-04-30
- **PR:** _(filled in after merge)_
- **Outcome:** `brokkr-worker --check-host` runs seven probes against the
  host kernel and prints a per-probe `[ OK | WARN | FAIL ]` checklist. A
  one-shot `scripts/install-cgroup-slice.sh` creates the
  `/sys/fs/cgroup/brokkr.slice` cgroup, chowns it, and enables the
  controllers we'll need (cpu/memory/pids/io). The journal you're reading
  was started in this milestone too.

### Open questions resolved

- **Cgroup delegation default** (plan §11.1). Going with the manual chown
  path — `scripts/install-cgroup-slice.sh` — as the documented default.
  systemd's `Delegate=yes` keeps working; documenting both paths is M9.
- **`unprivileged_userns_clone` is Debian/Ubuntu-only.** Probe via the
  universal `/proc/sys/user/max_user_namespaces` first; treat the
  Debian/Ubuntu sysctl as an *additional* gate, not the only one. Saves us
  spurious failures on Fedora / RHEL / Arch.

### Open questions deferred

- **`brokkr-sandboxd` discovery** (plan §11.2). Not relevant until M2
  when the binary actually exists. Decision: relative path next to the
  worker binary, with `$PATH` fallback.
- **systemd vs no-systemd hosts** (plan §11.5). M1 documents only the
  no-systemd path; M9 will add a systemd unit file.

### What surprised me

- `/proc/sys/kernel/unprivileged_userns_clone` is *not* universal —
  Debian/Ubuntu added it as an extra hardening knob. The cross-distro probe
  has to look at `/proc/sys/user/max_user_namespaces` (universal) and
  *also* honour the Debian/Ubuntu sysctl when it's present.
- Cgroup write-tests can't use `open(O_RDWR)` reliably — the only
  trustworthy probe is `mkdir`-then-`rmdir`. The directory name needs the
  PID baked in to avoid collisions if two workers run `--check-host`
  concurrently.
- Kernel release strings on WSL2 look like `6.6.87.2-microsoft-standard-WSL2`
  — the parser splits on both `.` and `-` to extract the leading
  `(major, minor)` pair without choking on the suffix.
