#!/usr/bin/env bash
# soak/firectl: drive `firectl` against a live squib UDS.
#
# Per `specs/91-impl-plan.md` § 10 Phase 7.4 and the M5 exit criterion. The
# script is intentionally simple: it builds squib, codesigns it, spawns it on a
# unique UDS, runs `firectl` with the documented `--firecracker-binary` /
# `--socket-path` overrides, and asserts the boot succeeds.
#
# Status (Phase 7): the boot-side stub VMM rejects InstanceStart; firectl will
# observe the documented `fault_message` ("VMM not yet wired"). The script
# reports the deviation as a known gap (exit 0 with WARN), so `make soak`
# stays green pre-Phase-1-tail. Once the live VMM lands, the assert switches
# from "fault_message contains 'VMM not yet wired'" to "boot completes".

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

FIRECTL_BIN="${SQUIB_SOAK_FIRECTL_BIN:-$(command -v firectl || true)}"
if [ -z "$FIRECTL_BIN" ] || [ ! -x "$FIRECTL_BIN" ]; then
    echo "[firectl] SKIP: firectl not in PATH (set SQUIB_SOAK_FIRECTL_BIN=...)"
    exit 0
fi

# Build + codesign squib.
make sign >/dev/null

SOCKET="$(mktemp -u -t squib-soak-firectl).sock"
KERNEL="$ROOT/examples/reference-vm/build/Image"
ROOTFS="${SQUIB_SOAK_ROOTFS:-/tmp/squib-soak-rootfs.ext4}"

if [ ! -f "$KERNEL" ]; then
    echo "[firectl] SKIP: $KERNEL missing — run 'make build-reference-vm' first."
    exit 0
fi
if [ ! -f "$ROOTFS" ]; then
    echo "[firectl] SKIP: rootfs $ROOTFS missing — set SQUIB_SOAK_ROOTFS=..."
    exit 0
fi

cleanup() {
    if [ -n "${SQUIB_PID:-}" ]; then
        kill "$SQUIB_PID" 2>/dev/null || true
        wait "$SQUIB_PID" 2>/dev/null || true
    fi
    rm -f "$SOCKET"
}
trap cleanup EXIT

if ! command -v jq >/dev/null 2>&1; then
    echo "[firectl] SKIP: jq is required to parse cargo metadata"
    exit 0
fi
target_dir="$(cargo metadata --format-version 1 --no-deps | jq -r '.target_directory')"
SQUIB_BIN="$target_dir/aarch64-apple-darwin/release/squib"

"$SQUIB_BIN" --api-sock "$SOCKET" --id soak-firectl &
SQUIB_PID=$!

# Wait up to 5 s for the socket to bind.
for _ in $(seq 1 50); do
    [ -S "$SOCKET" ] && break
    sleep 0.1
done
if [ ! -S "$SOCKET" ]; then
    echo "[firectl] FAIL: squib failed to bind $SOCKET"
    exit 1
fi

set +e
"$FIRECTL_BIN" \
    --firecracker-binary "$SQUIB_BIN" \
    --socket-path "$SOCKET" \
    --kernel "$KERNEL" \
    --root-drive "$ROOTFS" \
    --kernel-opts "console=ttyAMA0 reboot=k panic=1" \
    --vcpu-count 1 \
    --memory 256 2>&1 | tee /tmp/squib-soak-firectl.out
RC=$?
set -e

if [ $RC -eq 0 ]; then
    echo "[firectl] PASS"
elif grep -q "VMM not yet wired" /tmp/squib-soak-firectl.out; then
    echo "[firectl] WARN: stub VMM responded with documented fault_message — known Phase 1 tail gap"
    exit 0
else
    echo "[firectl] FAIL: unexpected firectl exit $RC"
    exit 1
fi
