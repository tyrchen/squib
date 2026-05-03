# Specs index

Authoritative specifications for squib. Read in order; each file declares its `depends_on` in front-matter.

## Direction (current)

Squib is **Apple-Silicon-only**, **HVF-only** (no VZ), **aarch64 Linux guests only**. Single 1.0 release with full Firecracker compatibility on day-one.

## Specs

| Spec | Type | What it covers |
|------|------|----------------|
| [squib-prd.md](./squib-prd.md) | prd | Mission, audiences, hard/soft requirements, scope, non-goals, success metrics, risks. New direction superseding the prior VZ-default draft. |
| [squib-design.md](./squib-design.md) | design | Workspace layout, alioth-shaped trait surface, vCPU run loop, GIC via `hv_gic_*`, PSCI dispatch, memory layout, devices, snapshots, networking, threading, security posture. |
| [squib-api-compat-design.md](./squib-api-compat-design.md) | design | Line-by-line Firecracker API compatibility matrix — every endpoint, field, CLI flag with status (Full / Partial / Accept-no-op / Reject). Single column (HVF only). |
| [squib-impl-plan.md](./squib-impl-plan.md) | impl-plan | 8 construction tracks (HVF backend, API, devices, MMDS, networking, snapshots, jailer/dist, compat/perf), 18-week timeline, single 1.0 release. |

## Reading order

1. **squib-prd.md** — the why and what, plus the explicit non-goals.
2. **squib-design.md** — the how, top-down.
3. **squib-api-compat-design.md** — the how, by every wire-level field. The contract bookkeeping.
4. **squib-impl-plan.md** — the when, track by track.

## Background research

These specs depend on the research under [`docs/research/`](../docs/research/index.md). See that index for reading order on the research side. Highlights:

- **`docs/research/hvf-prior-art-deep-dive.md`** — the borrow-vs-build verdict per subsystem; libkrun is Apache-2.0 (freely copyable), cloud-hypervisor has zero HVF, alioth has the cleanest trait shape.
- **`docs/research/aarch64-hvf-guest-stack.md`** — concrete memory layout, FDT skeleton, `hv_gic_*`, PSCI table, ESR_EL2 decoder, codesign.
- **`docs/research/hvf-performance-and-snapshots.md`** — `hv_vm_protect` dirty tracking, Mach exception ports for postcopy, snapshot file format.
