#!/bin/sh
# Phase 2 host setup for brokkr.
#
# Creates /sys/fs/cgroup/brokkr.slice and gives the target user write
# access, plus enables the cpu/memory/pids/io controllers under it. Running
# this once per host is the documented Phase 2 prerequisite for cgroup
# delegation; running it again is idempotent.
#
# Usage:
#   sudo scripts/install-cgroup-slice.sh
#   SLICE=/sys/fs/cgroup/brokkr.slice TARGET_USER=alice sudo -E scripts/install-cgroup-slice.sh
#
# After it succeeds, run `brokkr-worker --check-host` to verify.

set -eu

SLICE="${SLICE:-/sys/fs/cgroup/brokkr.slice}"
TARGET_USER="${TARGET_USER:-${SUDO_USER:-$(id -un)}}"

if [ "$(id -u)" -ne 0 ]; then
    echo "re-running under sudo (target user: $TARGET_USER)..." >&2
    exec sudo -E SLICE="$SLICE" TARGET_USER="$TARGET_USER" "$0" "$@"
fi

ROOT="/sys/fs/cgroup"

if ! grep -q "^cgroup2 $ROOT cgroup2" /proc/mounts; then
    echo "error: $ROOT is not a cgroup2 unified hierarchy" >&2
    echo "       brokkr Phase 2 requires cgroup v2; see docs/phase-2-plan.md §5.5" >&2
    exit 1
fi

# Sanity-check the controllers we plan to use exist on this kernel.
controllers=$(cat "$ROOT/cgroup.controllers")
for c in cpu memory pids io; do
    case " $controllers " in
        *" $c "*) ;;
        *) echo "warning: controller '$c' is missing from $ROOT/cgroup.controllers" >&2 ;;
    esac
done

# Delegate the controllers from the root cgroup to its immediate children
# (which is what brokkr.slice will be). No-op if already enabled.
echo "+cpu +memory +pids +io" > "$ROOT/cgroup.subtree_control"

mkdir -p "$SLICE"
chown -R "$TARGET_USER:$(id -gn "$TARGET_USER")" "$SLICE"

# Enable the same controllers within brokkr.slice's subtree so per-action
# cgroups (the leaves we will create at runtime) inherit them.
echo "+cpu +memory +pids +io" > "$SLICE/cgroup.subtree_control"

echo "ok: $SLICE owned by $TARGET_USER:$(id -gn "$TARGET_USER")"
echo "    controllers cpu/memory/pids/io delegated"
echo "verify with: brokkr-worker --check-host"
