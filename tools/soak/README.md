# Squib SDK soak harness

Per [`specs/91-impl-plan.md § 10 Phase 7.4`](../../specs/91-impl-plan.md#10-phase-7-compat-suite-perf-and-polish)
and [`specs/72-testing-strategy.md § 3`](../../specs/72-testing-strategy.md#3-compat-suite),
the 1.0 release gates on `firectl`, `firecracker-go-sdk`, and
`firecracker-containerd` integration tests passing against squib unmodified.
This directory hosts the per-SDK runner scripts.

## Layout

```text
tools/soak/
├── README.md                       # this file
├── firectl/run.sh                  # https://github.com/firecracker-microvm/firectl
├── firecracker-go-sdk/run.sh       # https://github.com/firecracker-microvm/firecracker-go-sdk
└── firecracker-containerd/run.sh   # https://github.com/firecracker-microvm/firecracker-containerd
```

Each `run.sh`:

1. Locates the SDK binary in `$PATH` (or under
   `${SQUIB_SOAK_<SDK>_BIN}` for hermetic CI lanes).
2. Builds and ad-hoc-signs `squib`.
3. Spawns squib in the background on a per-test UDS.
4. Drives the SDK against the UDS with a fixture config from
   `examples/reference-vm/config.json`.
5. Asserts the SDK reports the documented status code for every step in the
   sequence (matching the upstream Firecracker integration shape).
6. Tears down the process and the socket.

## Running

```bash
make soak                  # umbrella; skips runners whose SDK is not installed
tools/soak/firectl/run.sh  # one-off; assumes firectl already installed
```

Both paths surface a non-zero exit when the SDK observes a wire-shape
deviation. CI runs `make soak` on the post-merge lane; pre-1.0 the lane is
informational because the live VMM is gated on the Phase 1 tail.

## Status

| SDK | Binary expected | What the runner asserts | Status |
|-----|-----------------|--------------------------|--------|
| `firectl`              | `firectl`                | Boot a single-vCPU microVM via the Firecracker `getting-started.md` flow. | **scaffold present**: harness skeleton ready; SDK calls to a live VMM gate on Phase 1 tail. |
| `firecracker-go-sdk`   | `go test ./...` in vendored repo | Embedded `Test*` cases against squib's UDS. | **scaffold present**: vendoring + go-test driver gate on Phase 1 tail. |
| `firecracker-containerd` | `firecracker-containerd` (and `containerd`) | Boot a microVM image via the containerd shim. | **scaffold present**: harness skeleton ready; live integration gate on Phase 1 + a Linux-side runtime fixture. |

The "scaffold present" rows mean the runner is checked in and the umbrella
target wires correctly; "live integration" lights up when Phase 1's live vCPU
run-loop tail lands ([`specs/93-improvements-review.md`](../../specs/93-improvements-review.md)
"Phase 1 — Boot-to-busybox smoke test").

## Why each runner is opt-in

The SDKs are external Go projects with their own toolchains. Bundling them in
squib's CI matrix would inflate the build time without adding wire-shape
coverage that the in-process compat suite (`tests/firecracker-compat/`) does
not already cover. The soak runners are for end-to-end SDK-level confidence at
release time, not per-PR gating.
