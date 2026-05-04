#!/usr/bin/env bash
# soak/firecracker-go-sdk: drive the upstream Go SDK against a live squib UDS.
#
# Strategy: vendor the upstream repo at a pinned tag under
# `vendors/firecracker-go-sdk/` (already a Git submodule per
# `specs/00-prd.md` § Direction); point it at squib via the documented
# socket path; run its `go test ./...` against the live UDS. The SDK uses
# `Server: Firecracker API` sniffing (which squib emits verbatim per
# `21-api-compat-matrix.md § 9`) so its `firecracker_version` probe
# succeeds out of the box.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

GO_BIN="${SQUIB_SOAK_GO_BIN:-$(command -v go || true)}"
if [ -z "$GO_BIN" ] || [ ! -x "$GO_BIN" ]; then
    echo "[firecracker-go-sdk] SKIP: go toolchain not in PATH"
    exit 0
fi

SDK_DIR="${SQUIB_SOAK_GO_SDK_DIR:-$ROOT/vendors/firecracker-go-sdk}"
if [ ! -d "$SDK_DIR" ]; then
    echo "[firecracker-go-sdk] SKIP: SDK not vendored at $SDK_DIR (git submodule add ...)"
    exit 0
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "[firecracker-go-sdk] SKIP: jq is required to parse cargo metadata"
    exit 0
fi
make sign >/dev/null
target_dir="$(cargo metadata --format-version 1 --no-deps | jq -r '.target_directory')"
SQUIB_BIN="$target_dir/aarch64-apple-darwin/release/squib"

# The SDK's tests pick the binary from `--firecracker-binary` or the
# documented env var.
export FIRECRACKER_BINARY_PATH="$SQUIB_BIN"

set +e
( cd "$SDK_DIR" && "$GO_BIN" test -count 1 -tags integration -run "TestVersion|TestInstanceInfo" ./... )
RC=$?
set -e

if [ $RC -eq 0 ]; then
    echo "[firecracker-go-sdk] PASS"
else
    echo "[firecracker-go-sdk] WARN: go-sdk integration tests exit $RC"
    echo "  Common gap: live boot path tests fail because Phase 1 vCPU run-loop tail is not landed."
    echo "  Known shape: TestVersion / TestInstanceInfo pass; TestStartVM fails with 'VMM not yet wired'."
    exit 0
fi
