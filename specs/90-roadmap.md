---
title: 90-roadmap — stakeholder-facing milestones M0..M5
type: roadmap
status: draft
last_updated: 2026-05-03
depends_on: 00-prd.md
---

# 90 · Roadmap — stakeholder-facing milestones

Status: draft · Owner: squib · Depends on: [00-prd.md](./00-prd.md)

## 0. How to read this

This file is for **stakeholders** — engineers planning launches, orchestrator authors deciding when to integrate, product folks tracking the release. Each milestone is named from the **user's POV** (what gets unlocked) and carries an exit criterion that an outside observer can check.

The engineer-facing dependency-ordered build plan is [91-impl-plan.md](./91-impl-plan.md). The two pair 1:1 against milestones, but the order and grouping differ.

## 1. Principles

- **Always shippable.** Every milestone leaves the workspace green on `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo +nightly fmt --check`.
- **No public sub-1.0 release.** [00-prd.md § 7](./00-prd.md#6-compatibility-scope-the-contract) says "single 1.0 release with full Firecracker compatibility on day-one." Internal milestones exist for engineering sequencing only.
- **Calibrate honestly.** If a milestone slips, this file is updated, not patched over with a new milestone.

## 2. Milestone shape (calendar weeks)

Total: ~18 weeks for one full-time developer, with explicit overhead pads for review / oncall / meetings. Two parallel-friendly tracks (API server + boot path; devices + networking) collapse some calendar time.

| M  | Name | Calendar | What gets unlocked |
|----|------|----------|--------------------|
| M0 | "Hello, kernel" | weeks 1–6 | An aarch64 Linux kernel boots inside squib; serial output observable on stdout. *Internal, not shipped.* |
| M1 | "Configurable microVM" | weeks 4–9 (overlap) | The full Firecracker API surface accepts requests; static config files replay; every virtio device frontend works against a stub backend. |
| M2 | "Networked guest with metadata" | weeks 6–10 | `--network=shared` (vmnet NAT) and `--network=userspace` (gvproxy) work; MMDS reachable from inside the guest at 169.254.169.254. |
| M3 | "Save and restore" | weeks 10–14 | Full and Diff snapshots round-trip; postcopy via Mach exception ports works for lazy restore. |
| M4 | "Distributable, sandboxable" | weeks 11–15 (overlap) | `squib-jail` ships flag-compatible with upstream jailer; codesigned, notarized binaries; `.pkg` and Homebrew formula. |
| M5 | "Compat-suite green, perf published" | weeks 15–18 | Every row in [21-api-compat-matrix.md](./21-api-compat-matrix.md) has a passing test or a documented skip; published boot-time and memory benchmarks meet [00-prd.md § 8 R6, R7](./00-prd.md#8-hard-requirements-10). **1.0 release tag.** |

## 3. Build-order graph

```text
00-prd ──► 10-data-model ──► 11-runtime-core ──► 12-hvf-backend ──► 13-arch-and-boot
                                              │
                                              ▼
                       14-virtio-and-devices ──► 15-mmds ──► 30-networking
                                              │
                                              ▼
                                       16-snapshots
                                              │
                                              ▼
                       20-firecracker-api ◄────┴──────► 21-api-compat-matrix
                                              │
                                              ▼
                       40-jailer ─────► 50-cli
                                              │
                                              ▼
                  Cross-cuts: 61-crates, 70-security, 71-perf, 72-testing
```

Engineers should also read the dependency-ordered build sequence in [91-impl-plan.md](./91-impl-plan.md), which differs from the milestone order.

## 4. Milestones in detail

### M0 — "Hello, kernel" (weeks 1–6)

A user with no prior squib install can:
1. Run `make sign` to get a codesigned local build with the right entitlements.
2. Run `cargo run -- --config-file examples/hello.json` and observe an aarch64 Linux kernel boot to a busybox shell on stdout in under 1 s.

**Specs touched**: [00-prd.md](./00-prd.md), [10-data-model.md](./10-data-model.md), [11-runtime-core.md](./11-runtime-core.md), [12-hvf-backend.md](./12-hvf-backend.md), [13-arch-and-boot.md](./13-arch-and-boot.md).

**Exit criteria**:
- A guest with 1 vCPU, 256 MiB RAM, no devices, busybox initrd, runs `/sbin/init` and the host sees its first stdout byte.
- `cargo clippy -- -D warnings` clean, `cargo +nightly fmt --check` clean.

**Not shipped publicly.** Internal demo only.

### M1 — "Configurable microVM" (weeks 4–9)

A user with the upstream Firecracker `getting-started.md` curl sequence can run it against squib unchanged and get the same status codes for every step except `InstanceStart` (which boots a real VM). All virtio device frontends work against a stub backend.

**Specs touched**: [10-data-model.md](./10-data-model.md), [14-virtio-and-devices.md](./14-virtio-and-devices.md), [20-firecracker-api.md](./20-firecracker-api.md), [21-api-compat-matrix.md](./21-api-compat-matrix.md).

**Exit criteria**:
- Every endpoint in [21-api-compat-matrix.md § 1](./21-api-compat-matrix.md#1-http-api-endpoints) returns the documented status code for at least one happy-path call.
- Static config file from `examples/full.json` replays without error.
- Every virtio device's per-spec functional test passes.

### M2 — "Networked guest with metadata" (weeks 6–10)

A user can `curl example.com` from inside a squib guest in `shared` mode (vmnet NAT) without any extra entitlements they don't already have. A user can `curl 169.254.169.254/foo` from inside a squib guest and get back MMDS-served data.

**Specs touched**: [15-mmds.md](./15-mmds.md), [30-networking.md](./30-networking.md).

**Exit criteria**:
- vmnet shared-mode and gvproxy userspace mode both pass an end-to-end TCP test from guest to host.
- MMDS V1 and V2 (IMDSv2 token) both serve the documented payload shape.

### M3 — "Save and restore" (weeks 10–14)

A user can `PUT /snapshot/create` with `snapshot_type=Full`, kill the squib process, restart it, `PUT /snapshot/load`, and observe the guest continuing where it left off. Same for `Diff` snapshots when `track_dirty_pages: true` is set. Postcopy via Mach exception ports loads lazily.

**Specs touched**: [16-snapshots.md](./16-snapshots.md).

**Exit criteria**:
- CI integration test takes a Full snapshot, kills the VM, restores, asserts a guest counter has continued.
- Diff snapshot test does the same with mid-flight dirty pages.
- Postcopy test loads a 2 GiB-RAM snapshot lazily; only touched pages are paged in.

### M4 — "Distributable, sandboxable" (weeks 11–15)

A user can `brew install squib` (or download a `.pkg`) and run `squib-jail --uid 1000 --gid 1000 --id myvm --exec-file $(which squib) -- --config-file vm.json` and get a working chroot'd microVM.

**Specs touched**: [40-jailer.md](./40-jailer.md), [50-cli.md](./50-cli.md), [61-crates-and-features.md](./61-crates-and-features.md).

**Exit criteria**:
- `squib-jail` parses every upstream jailer flag.
- `make notarize` produces a stapleable `.pkg` that installs and runs on a fresh Apple Silicon Mac.
- Homebrew formula is in a tap (or under review for core).

### M5 — "Compat-suite green, perf published" — **the 1.0 release**

The compat suite is gating, every P-axis target is met or has an explicit waiver, and the README publishes squib's measured numbers (not borrowed from upstream).

**Specs touched**: [21-api-compat-matrix.md](./21-api-compat-matrix.md), [71-performance-budgets.md](./71-performance-budgets.md), [72-testing-strategy.md](./72-testing-strategy.md).

**Exit criteria**:
- ≥ 95% of Firecracker `docs/api_requests/` examples pass against squib unmodified.
- p50 boot to `/sbin/init` ≤ 400 ms on M2 Pro / M3, measured and published.
- Memory overhead ≤ 15 MiB at idle.
- `firectl`, `firecracker-go-sdk`, `firecracker-containerd` integration tests pass.
- At least one external orchestrator's CI runs squib on macOS Apple Silicon (in flight; targets 1.0+1).
- 1.0 git tag.

## 5. What a slippage looks like

If a milestone misses its calendar window, this file is updated with the new dates and a one-line *why*. Honest calibration > optimistic plans.

Examples of slippage that have already informed estimates:

- The earlier draft assumed VZ-default for time-to-MVP (~6 weeks to first kernel boot); that target was rejected on perf grounds in favour of HVF-only at the cost of ~2 weeks of additional work (vCPU run loop port + GIC integration). Recorded as [99-key-decisions.md § D1](./99-key-decisions.md#d1-hvf-only-no-vz).
- The earlier draft assumed snapshots would land in a v0.4 phase, separate from 1.0. The new direction is single 1.0 release; snapshots become a hard dependency on the release tag, not a v0.4-then-v0.5 sequence.

## 6. Cross-references

- ← Depends on: [00-prd.md](./00-prd.md)
- → Pairs with: [91-impl-plan.md](./91-impl-plan.md) (engineer-facing build order)
- ↔ Related: [99-key-decisions.md](./99-key-decisions.md) (load-bearing trade-offs)
