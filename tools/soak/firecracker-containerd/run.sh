#!/usr/bin/env bash
# soak/firecracker-containerd: drive the containerd shim against squib.
#
# This runner is the most environment-sensitive of the three SDK soaks: the
# shim needs `containerd` running, a `runtime.toml` pointing at squib's UDS,
# and a Linux microVM image registered with the local image store.
#
# Phase 7 ships the runner skeleton + a documentation note pointing at the
# canonical install path. CI does *not* run this lane until M5; the soak is
# release-gating only. Out-of-scope for this revision is the actual containerd
# shim invocation — the script reports SKIP unless every prerequisite is in
# place.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

CONTAINERD_BIN="${SQUIB_SOAK_CONTAINERD_BIN:-$(command -v containerd || true)}"
SHIM_BIN="${SQUIB_SOAK_FC_CONTAINERD_BIN:-$(command -v firecracker-containerd || true)}"

if [ -z "$CONTAINERD_BIN" ] || [ -z "$SHIM_BIN" ]; then
    echo "[firecracker-containerd] SKIP: containerd / firecracker-containerd not installed"
    echo "  Install: https://github.com/firecracker-microvm/firecracker-containerd"
    exit 0
fi

# Pre-flight: a runtime.toml pointing at squib must exist. Populate by hand;
# squib doesn't ship a vendored copy because the path is operator-specific.
RUNTIME_TOML="${SQUIB_SOAK_RUNTIME_TOML:-/etc/firecracker-containerd/runtime.toml}"
if [ ! -f "$RUNTIME_TOML" ]; then
    echo "[firecracker-containerd] SKIP: $RUNTIME_TOML not present"
    echo "  See tools/soak/firecracker-containerd/runtime.toml.example"
    exit 0
fi

echo "[firecracker-containerd] WARN: runtime invocation deferred to M5 release lane"
echo "  Phase 7 ships the skeleton; Phase 1 vCPU tail + a bundled microVM image are the gating prereqs."
exit 0
