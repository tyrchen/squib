# Specs index

Authoritative specifications for **squib** — a Firecracker-compatible microVM monitor for Apple Silicon. Numbered files are in build order; reading top-to-bottom matches the milestone progression in [90-roadmap.md](./90-roadmap.md).

## Direction (current)

Squib is **Apple-Silicon-only**, **HVF-only** (no VZ), **aarch64 Linux guests only**. Single 1.0 release with full Firecracker compatibility on day-one. See [99-key-decisions.md § D1–D26](./99-key-decisions.md) for the load-bearing trade-offs.

## Spec files

| #   | Spec | Type | What it covers |
|-----|------|------|----------------|
| 00  | [00-prd.md](./00-prd.md) | prd | Vision, users, goals, non-goals, hard / soft / NFR requirements, naming conventions, risks. |
| 10  | [10-data-model.md](./10-data-model.md) | design | HTTP wire shapes, `VmExit` algebra, cross-thread `ApiAction`, `MicrovmState`, snapshot file format. |
| 11  | [11-runtime-core.md](./11-runtime-core.md) | design | Alioth-shaped traits (Hypervisor / Vm / Vcpu); lifecycle; threading rules; panic policy; error types. |
| 12  | [12-hvf-backend.md](./12-hvf-backend.md) | design | `squib-hv` — applevisor binding, vCPU run loop, GIC via `hv_gic_*`, the unsafe boundary. |
| 13  | [13-arch-and-boot.md](./13-arch-and-boot.md) | design | aarch64 memory layout, sysreg subset, ESR_EL2 decoder, PSCI dispatch, FDT, kernel loader, boot orchestration. |
| 14  | [14-virtio-and-devices.md](./14-virtio-and-devices.md) | design | MMIO bus, virtio-MMIO transport, every device (block, net, vsock, balloon, rng, console, pmem, virtio-mem, boot-timer). |
| 15  | [15-mmds.md](./15-mmds.md) | design | Microservices metadata: dumbo + mmds port, virtio-net packet interception. |
| 16  | [16-snapshots.md](./16-snapshots.md) | design | `bitcode + serde` state file, sparse memory, `hv_vm_protect` dirty tracking, Mach-exception postcopy. |
| 20  | [20-firecracker-api.md](./20-firecracker-api.md) | design | axum on UDS, error model, state machine, static-config replay. |
| 21  | [21-api-compat-matrix.md](./21-api-compat-matrix.md) | design | Line-by-line bookkeeping: every endpoint, field, CLI flag with status (F / P / A / R). |
| 30  | [30-networking.md](./30-networking.md) | design | vmnet shared / bridged / host modes; gvproxy userspace fallback. |
| 40  | [40-jailer.md](./40-jailer.md) | design | `squib-jail` Darwin shim with the upstream jailer flag set. |
| 50  | [50-cli.md](./50-cli.md) | design | clap surface for `squib`, mode selection, tracing setup. |
| 61  | [61-crates-and-features.md](./61-crates-and-features.md) | design | Workspace layout, dependency graph, external dependency catalogue, feature flags, lints. |
| 70  | [70-security.md](./70-security.md) | design | Threat model, unsafe boundaries, input validation, secrets, code-signing, supply chain. |
| 71  | [71-performance-budgets.md](./71-performance-budgets.md) | design | Per-axis targets (boot, mem, exit, network, block, snapshot), bench harness, CI gates. |
| 72  | [72-testing-strategy.md](./72-testing-strategy.md) | design | Test pyramid, compat suite, fixtures, CI matrix, upstream tracking. |
| 80  | [80-glossary.md](./80-glossary.md) | glossary | Disambiguation of overloaded terms (backend, vsock vs TSI, sysreg vs reg, snapshot vs state file, jailer vs squib-jail, …). |
| 90  | [90-roadmap.md](./90-roadmap.md) | roadmap | **Stakeholder-facing**: milestones M0–M5, exit criteria, calendar shape. |
| 91  | [91-impl-plan.md](./91-impl-plan.md) | impl-plan | **Engineer-facing**: dependency-ordered phases, effort estimates, exit criteria. |
| 99  | [99-key-decisions.md](./99-key-decisions.md) | decision-log | D1…D26 — the *why* behind every load-bearing choice. |

## Reading order

For a new contributor:

1. [00-prd.md](./00-prd.md) — the why and what, plus the explicit non-goals.
2. [99-key-decisions.md](./99-key-decisions.md) — the *why* behind the trade-offs.
3. [11-runtime-core.md](./11-runtime-core.md) → [12-hvf-backend.md](./12-hvf-backend.md) → [13-arch-and-boot.md](./13-arch-and-boot.md) — the spine.
4. [10-data-model.md](./10-data-model.md) — every wire and cross-thread shape.
5. [14-virtio-and-devices.md](./14-virtio-and-devices.md) → [15-mmds.md](./15-mmds.md) → [30-networking.md](./30-networking.md) → [16-snapshots.md](./16-snapshots.md) — the device tree, networking, snapshots.
6. [20-firecracker-api.md](./20-firecracker-api.md) → [21-api-compat-matrix.md](./21-api-compat-matrix.md) — the API surface.
7. [40-jailer.md](./40-jailer.md), [50-cli.md](./50-cli.md), [61-crates-and-features.md](./61-crates-and-features.md) — packaging and entry points.
8. [70-security.md](./70-security.md), [71-performance-budgets.md](./71-performance-budgets.md), [72-testing-strategy.md](./72-testing-strategy.md) — cross-cuts read alongside, not in sequence.
9. [80-glossary.md](./80-glossary.md) — disambiguate as needed.

For a stakeholder: [00-prd.md](./00-prd.md) → [90-roadmap.md](./90-roadmap.md). For an engineer about to write code: [91-impl-plan.md](./91-impl-plan.md).

## Build-order graph

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

## Background research

These specs depend on the research under [`docs/research/`](../docs/research/index.md). See that index for reading order on the research side. Highlights:

- **`docs/research/hvf-prior-art-deep-dive.md`** — borrow-vs-build verdict per subsystem; libkrun is Apache-2.0 (freely copyable), cloud-hypervisor has zero HVF, alioth has the cleanest trait shape.
- **`docs/research/aarch64-hvf-guest-stack.md`** — concrete memory layout, FDT skeleton, `hv_gic_*`, PSCI table, ESR_EL2 decoder, codesign.
- **`docs/research/hvf-performance-and-snapshots.md`** — `hv_vm_protect` dirty tracking, Mach exception ports for postcopy, snapshot file format.

## Spec layout convention

Files follow `NN-name.md` numbering. The numbering is the build order — reading top to bottom matches the engineer-facing dependency progression in [91-impl-plan.md](./91-impl-plan.md). Component design files start at 10; integration / surface files at 20; cross-cuts at 60+; roadmap / impl-plan / decisions at 90+.

When upstream Firecracker adds a field, [21-api-compat-matrix.md](./21-api-compat-matrix.md) gets a new row. When squib makes a load-bearing decision, [99-key-decisions.md](./99-key-decisions.md) gets a new D-id (never edit existing decisions in place; supersede with a new D-id).
