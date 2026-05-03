---
title: 99-key-decisions — load-bearing decisions log
type: decision-log
status: draft
last_updated: 2026-05-03
---

# 99 · Key Decisions — load-bearing trade-offs

Status: draft · Owner: squib

Each decision is permanent; supersede with a new D-id rather than editing in place. Future "why this?" questions point here, not to a chat scrollback.

---

## D1 — HVF only, no VZ

- **Context**: choosing the macOS hypervisor framework.
- **Alternatives considered**:
  - VZ.framework default with HVF as opt-in (the original PRD direction).
  - Dual-backend with `--hypervisor` flag.
  - HVF only.
- **Decision**: HVF only. No VZ surface in the workspace, no `--hypervisor` flag, no compile-time `cfg` split.
- **Why**: VZ's closed device model rules out (a) custom virtio-MMIO devices and per-queue rate limiters, (b) PVH / kernel-level boot tuning, (c) dirty-page tracking. With "performance is the priority and 1.0 ships full feature parity" as the new direction, VZ disqualifies itself on three counts. The earlier dual-backend matrix had ~24 P / R rows for VZ-related limitations; HVF-only flips them all to F. The cost: ~2 weeks additional engineering work for the vCPU run-loop port and GIC integration.
- **Pinned by**: [00-prd.md § 4](./00-prd.md#4-non-goals), [12-hvf-backend.md § 2](./12-hvf-backend.md#2-why-hvf-and-not-vz), [11-runtime-core.md § 2](./11-runtime-core.md#2-interface) (no `BackendKind::Vz`).
- **Date**: 2026-05-03

---

## D2 — macOS 15 minimum

- **Context**: minimum supported macOS version.
- **Alternatives considered**:
  - macOS 14 minimum (broader install base) with userspace GICv3 emulation.
  - macOS 15 minimum (`hv_gic_*` available) with no userspace GIC fallback.
- **Decision**: macOS 15 Sequoia minimum. No userspace GIC fallback, ever.
- **Why**: `hv_gic_*` lands in macOS 15. Without it, we carry libkrun's ~3 KLOC of userspace distributor / redistributor / LR-shadow / pending-active arbitration. With it, our GIC is one struct that calls Apple APIs. Recommended target: macOS 26 Tahoe.
- **Pinned by**: [00-prd.md § 8 R9](./00-prd.md#8-hard-requirements-10), [12-hvf-backend.md § 3,6](./12-hvf-backend.md#3-why-macos-15-minimum), [99-key-decisions.md § D1](#d1-hvf-only-no-vz) (companion).
- **Date**: 2026-05-03

---

## D3 — aarch64 Linux guests only

- **Context**: guest CPU architecture support.
- **Alternatives considered**:
  - aarch64 + x86_64 (with QEMU TCG or Rosetta-share for x86 guests).
  - aarch64 only.
- **Decision**: aarch64 only. x86 binaries inside the guest run via guest-side `binfmt_misc + qemu-user`.
- **Why**: Apple Silicon HVF is aarch64. Emulating x86 at the VMM level is out of scope; the same migration AWS Lambda customers do for Graviton applies (recompile for arm64).
- **Pinned by**: [00-prd.md § 4 (Non-goals 3)](./00-prd.md#4-non-goals), [13-arch-and-boot.md](./13-arch-and-boot.md) (everything aarch64-shaped).
- **Date**: 2026-05-03

---

## D4 — Single 1.0 release, full Firecracker compatibility on day-one

- **Context**: release strategy.
- **Alternatives considered**:
  - Phased rollout (v0.1 boots, v0.4 brings HVF, v0.7 snapshots, 1.0).
  - Single 1.0 with all Firecracker-compat features.
- **Decision**: Single 1.0 with full feature parity at first public release. Internal phases exist for engineering sequencing only; no public sub-1.0 tags.
- **Why**: The phased plan accumulated debt — every "v0.x ships without snapshots" required a parallel API surface that would later be invalidated. A single coherent 1.0 release is also a stronger signal to orchestrator authors deciding when to integrate.
- **Pinned by**: [00-prd.md § 6](./00-prd.md#6-compatibility-scope-the-contract), [90-roadmap.md § 1](./90-roadmap.md#1-principles).
- **Date**: 2026-05-03

---

## D5 — Snapshot encoding: bitcode, not versionize

- **Context**: serialization format for the snapshot state file.
- **Alternatives considered**:
  - `versionize` (upstream Firecracker pre-1.10).
  - `bitcode + serde` (upstream Firecracker post-1.10).
  - Custom CBOR-flavoured.
- **Decision**: `bitcode + serde`, matching upstream Firecracker post-1.10.
- **Why**: same wire encoding as upstream means `--describe-snapshot` against an upstream-produced state file works structurally; simpler than maintaining a versionize dialect.
- **Pinned by**: [10-data-model.md § 6.1](./10-data-model.md#61-state-file-idsnap), [16-snapshots.md § 2](./16-snapshots.md#2-state-file).
- **Date**: 2026-05-03

---

## D6 — Sysreg subset curated, not full ARMv8

- **Context**: which sysregs to save / restore in snapshots.
- **Alternatives considered**:
  - Mirror the full KVM list (~250 regs).
  - Curated subset (~100 regs).
- **Decision**: curated subset of ~100 regs, additive contract (registers can be added with a snapshot version bump but never removed).
- **Why**: many KVM-saved registers are EL2/EL3 or x86-only; saving them is dead weight. The curated list is sufficient for workload-equivalent restore on HVF and bounded by what `applevisor` can read/write.
- **Pinned by**: [13-arch-and-boot.md § 3](./13-arch-and-boot.md#3-sysreg-subset).
- **Date**: 2026-05-03

---

## D7 — Block IO: tokio + spawn_blocking (not dispatch_io)

- **Context**: block-device backend implementation.
- **Alternatives considered**:
  - libkrun-style `dispatch_io` directly on libdispatch.
  - tokio + `spawn_blocking` against `F_NOCACHE`-opened fds.
  - io_uring-equivalent (does not exist on macOS).
- **Decision**: tokio + `spawn_blocking` with `F_NOCACHE` for 1.0; revisit if P5 (≥ 100 K IOPS) misses.
- **Why**: simpler in the workspace's existing tokio runtime; meets the IOPS target on Apple SSD. `dispatch_io` would require a parallel async runtime layer.
- **Pinned by**: [14-virtio-and-devices.md § 4.1](./14-virtio-and-devices.md#41-virtio-block), [71-performance-budgets.md § 3](./71-performance-budgets.md#3-block-io).
- **Date**: 2026-05-03

---

## D8 — TSI vsock: off by default

- **Context**: virtio-vsock semantics.
- **Alternatives considered**:
  - TSI on by default (libkrun behaviour).
  - TSI off by default; opt-in via `"squib": { "vsock_tsi": true }`.
- **Decision**: off by default.
- **Why**: TSI changes vsock semantics in a non-Firecracker-compatible way (guest opens AF_VSOCK sockets and squib transparently proxies to host AF_INET). On-by-default would silently break upstream-Firecracker-compatible launchers. Opt-in preserves the contract.
- **Pinned by**: [14-virtio-and-devices.md § 4.3](./14-virtio-and-devices.md#43-virtio-vsock), [21-api-compat-matrix.md § 2 (vsock)](./21-api-compat-matrix.md#vsock-put).
- **Date**: 2026-05-03

---

## D9 — MMDS: vendor from Firecracker

- **Context**: how to integrate `dumbo` and `mmds`.
- **Alternatives considered**:
  - Vendor (copy with attribution).
  - `git`-dep against upstream.
  - Reimplement.
- **Decision**: vendor (copy with attribution under `NOTICE`); revisit if maintenance bandwidth allows tracking upstream.
- **Why**: both crates are OS-agnostic and Apache-2.0; vendoring isolates squib from upstream churn and avoids `git`-dep brittleness in CI. Reimplementing is unjustified work.
- **Pinned by**: [15-mmds.md § 1](./15-mmds.md#1-purpose).
- **Date**: 2026-05-03

---

## D10 — Cross-host snapshot replay: not supported

- **Context**: should snapshots taken on Linux/KVM aarch64 restore on macOS/HVF?
- **Alternatives considered**:
  - Cross-host memory-only restore (lossy: state blob ignored, only memory rehomed).
  - No cross-host support; explicit non-goal.
- **Decision**: not supported. Explicitly documented as a non-goal.
- **Why**: HVF and KVM expose different sysreg subsets, different timer state, different GIC representation. Workload-equivalence is achievable on save / restore within HVF; bit-exact KVM register fidelity is not. Memory-only cross-restore is a stretch goal at most.
- **Pinned by**: [00-prd.md § 4 (Non-goals 2)](./00-prd.md#4-non-goals), [16-snapshots.md § 1](./16-snapshots.md#1-purpose), [21-api-compat-matrix.md § 7](./21-api-compat-matrix.md#7-snapshot-file-format).
- **Date**: 2026-05-03

---

## D11 — Dirty tracking: 2 MiB default with 4 KiB fallback

- **Context**: granularity of `hv_vm_protect`-based dirty bitmap.
- **Alternatives considered**:
  - 4 KiB everywhere (precise; high TLB-shootdown cost).
  - 2 MiB everywhere (cheap; over-counts dirty bytes).
  - 2 MiB default + adaptive 4 KiB for hot regions.
- **Decision**: 2 MiB default; the tracker's heuristic drops to 4 KiB only for hot regions (per-region dirty rate above threshold).
- **Why**: TLB shootdown is the real performance limiter. 2 MiB granularity bounds the cost; 4 KiB is necessary only when the over-count meaningfully bloats the Diff snapshot.
- **Pinned by**: [16-snapshots.md § 4](./16-snapshots.md#4-dirty-page-tracking), [71-performance-budgets.md § 6.1](./71-performance-budgets.md#61-diff-snapshot-save).
- **Date**: 2026-05-03

---

## D12 — OpenAPI served behind a flag

- **Context**: should `GET /openapi.json` always be available?
- **Alternatives considered**:
  - Always-on.
  - Behind an explicit `--openapi` flag.
- **Decision**: behind `--openapi`, off by default to match upstream Firecracker behaviour.
- **Why**: upstream does not serve OpenAPI by default; matching that posture preserves the wire surface for compat-suite parity. Power users opt in.
- **Pinned by**: [20-firecracker-api.md § 7](./20-firecracker-api.md#7-openapi-document), [50-cli.md § 2](./50-cli.md#2-parser).
- **Date**: 2026-05-03

---

## D13 — vmnet via hand-rolled FFI

- **Context**: how to bind `vmnet.framework`.
- **Alternatives considered**:
  - Use a community crate (none maintained at the time of writing).
  - Hand-roll FFI inside `squib-net::sys`.
- **Decision**: hand-roll FFI, isolated to `squib-net::sys`. ~300 lines of `unsafe`, each block carrying a `// SAFETY:` comment.
- **Why**: no maintained crate exists. Vendoring an unmaintained one creates a supply-chain risk; hand-rolling is bounded and reviewable.
- **Pinned by**: [30-networking.md § 3](./30-networking.md#3-vmnet-binding-squib-netsys), [70-security.md § 3.2](./70-security.md#32-squib-netsys).
- **Date**: 2026-05-03

---

## D14 — Bundled reference kernel + initramfs

- **Context**: should squib ship a known-good aarch64 vmlinux + busybox initrd?
- **Alternatives considered**:
  - Yes, in `examples/`.
  - No, force users to bring their own.
- **Decision**: yes, in `examples/`.
- **Why**: a hand-tuned kernel config (lz4 compression, `pci=off`, `console=hvc0`, minimal initramfs) is the difference between p50 ≤ 400 ms boot and p50 ≈ 800 ms. Without a reference, every user benchmarks against their own kernel and concludes squib is slow.
- **Pinned by**: [71-performance-budgets.md § 2](./71-performance-budgets.md#2-targets-10), [91-impl-plan.md § 10](./91-impl-plan.md#10-phase-7--compat-suite--perf--polish-weeks-1518).
- **Date**: 2026-05-03

---

## D15 — Snapshot format pin: lockstep with upstream

- **Context**: how to evolve the snapshot format.
- **Alternatives considered**:
  - Independent versioning (squib-1.0 ≠ Firecracker 1.16).
  - Pin to upstream minor and bump in lockstep.
- **Decision**: pin to upstream Firecracker minor (currently 1.16); bump in lockstep each cycle.
- **Why**: the magic-id and bitcode encoding match upstream so `firecracker --describe-snapshot` against squib-produced files stays useful. Independent versioning would erode that.
- **Pinned by**: [21-api-compat-matrix.md § 7](./21-api-compat-matrix.md#7-snapshot-file-format), [72-testing-strategy.md § 5](./72-testing-strategy.md#5-upstream-tracking).
- **Date**: 2026-05-03

---

## D16 — Single-VM per process for 1.0

- **Context**: should a single squib process host multiple microVMs?
- **Alternatives considered**:
  - Multi-VM per process (with per-VM API endpoints under `/vms/{id}`).
  - Single-VM per process (one squib invocation = one VM).
- **Decision**: single-VM per process for 1.0. The launcher pattern (one squib per VM) matches upstream Firecracker.
- **Why**: matches upstream API exactly; isolates VM crashes (a vCPU panic is fatal to the VM but the process ends, freeing all resources cleanly). Multi-VM would diverge from the upstream contract.
- **Pinned by**: [00-prd.md § 4](./00-prd.md#4-non-goals), [11-runtime-core.md § 5](./11-runtime-core.md#5-panic-policy).
- **Date**: 2026-05-03

---

## Cross-references

- ← Read by: every component spec when justifying a non-obvious choice.
- ↔ Pairs with: [90-roadmap.md](./90-roadmap.md), [91-impl-plan.md](./91-impl-plan.md), [00-prd.md](./00-prd.md).
