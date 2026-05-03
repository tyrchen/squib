# Research index

Research for the squib project. Read these before touching `specs/`.

## Direction (current)

Squib is **Apple-Silicon-only**, **HVF-only** (no VZ), **aarch64 Linux guests only**, single-1.0 release with full Firecracker compatibility on day-one. The VZ-default architecture explored in earlier drafts is no longer in scope.

## Active docs

| Doc | What it covers |
|-----|----------------|
| [firecracker-api-surface.md](./firecracker-api-surface.md) | Complete public interface squib must replicate: HTTP API, CLI flags, static config file, MMDS, vsock, snapshot file shapes, error format, version stability. |
| [firecracker-architecture.md](./firecracker-architecture.md) | Internal architecture of upstream Firecracker — process layout, vCPU run loop, device model, memory, snapshots — with a portability map to macOS analogues. |
| [firecracker-subsystems.md](./firecracker-subsystems.md) | Auxiliary subsystems: snapshots, MMDS/dumbo, vsock multiplex protocol, jailer, logger/metrics, balloon, entropy, CPU templates, seccomp, PVH/initrd. |
| [hvf-prior-art-deep-dive.md](./hvf-prior-art-deep-dive.md) | libkrun + cloud-hypervisor + alioth deep dive. **Key finding: libkrun is Apache-2.0**, freely copyable. cloud-hypervisor has zero HVF integration. Borrow-vs-build verdict per subsystem; recommended squib crate layout. |
| [aarch64-hvf-guest-stack.md](./aarch64-hvf-guest-stack.md) | aarch64 boot protocol, FDT skeleton, GICv3 via `hv_gic_*` (macOS 15+), PSCI function table, ESR_EL2 syndrome decoder, proposed concrete memory layout, code-signing. |
| [hvf-performance-and-snapshots.md](./hvf-performance-and-snapshots.md) | HVF performance ceiling, dirty page tracking via `hv_vm_protect`, postcopy paging via Mach exception ports, snapshot file format (bitcode, not versionize), block/net IO strategies, distribution constraints. |

## Superseded

| Doc | Status |
|-----|--------|
| [macos-hypervisor-ecosystem.md](./macos-hypervisor-ecosystem.md) | **Superseded** for the recommendation sections (it argued VZ-default with HVF as escape hatch). Still useful as a reference catalog of the macOS hypervisor ecosystem (HVF API surface, VZ feature set, vmnet, every Rust crate, every macOS VMM project). The HVF deep-dive docs above replace its decision-making content. |

## Reading order

1. **firecracker-api-surface.md** — what we must accept on the wire.
2. **firecracker-subsystems.md** — what each part does.
3. **hvf-prior-art-deep-dive.md** — what to copy and from where.
4. **aarch64-hvf-guest-stack.md** — concrete construction details.
5. **hvf-performance-and-snapshots.md** — the operationally hard parts.
6. **firecracker-architecture.md** — fills in any upstream-internals gaps.
