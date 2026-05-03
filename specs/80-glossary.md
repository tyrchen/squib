---
title: 80-glossary — disambiguating overloaded terms
type: glossary
status: draft
last_updated: 2026-05-03
---

# 80 · Glossary — disambiguating overloaded terms

Status: draft · Owner: workspace

## 1. Purpose

Define *overloaded* terms only — words two readers used differently in the same conversation, or terms borrowed from a neighbouring ecosystem with different meanings. If a word's meaning is obvious from the codebase, it is not in this list.

## 2. Terms

### Backend / Hypervisor

In this project, **backend** refers to a `HypervisorBackend` trait implementation. Squib has exactly one production backend: HVF, in `squib-hv`. The `BackendKind::Mock` variant is for unit tests; never selectable at runtime.

In upstream Firecracker, "backend" sometimes refers to a *device* backend (e.g. block backend, net backend). When we use it that way, we say *device backend* or *host backend* explicitly. See [14-virtio-and-devices.md](./14-virtio-and-devices.md).

### vsock vs TSI

**vsock** in squib defaults to the upstream Firecracker UDS-multiplex protocol: host-side Unix-domain socket; guest-initiated streams encoded as `<uds_path>_<port>` listeners; host-initiated streams via the `CONNECT <port>\n` → `OK <port>\n` handshake. Bit-identical to upstream.

**TSI** (Transparent Socket Impersonation) is a libkrun-introduced *extension* where the guest opens AF_VSOCK sockets and squib transparently proxies to host AF_INET / AF_UNIX. Squib supports TSI behind an opt-in `"squib": { "vsock_tsi": true }` flag. TSI changes vsock semantics in ways upstream Firecracker does not, so it is **off by default**.

### Sysreg vs Reg

**Sysreg** = ARMv8 system register (S3_<op0>_<crn>_<crm>_<op2>). Accessed via `MRS` / `MSR`; trapped through `EC_MSR/MRS` in our run loop. Curated subset in `squib-arch::sysreg::SysReg`. See [13-arch-and-boot.md § 3](./13-arch-and-boot.md#3-sysreg-subset).

**Reg** (no qualifier) = general-purpose register (X0..X30, SP, PC, PSTATE) or a SIMD register (V0..V31). Accessed via the `Vcpu::get_reg` / `Vcpu::set_reg` trait methods.

### Snapshot vs State file vs Memory file

**Snapshot** is the *pair* of files produced by `PUT /snapshot/create`: a state file and a memory file. The pair restores together.

**State file** (`<id>.snap`) holds `MicrovmState`: vCPU registers, GIC blob, device cursors, MMDS tree. Bitcode-encoded; magic `0x07101984_AAAA_0000`. See [10-data-model.md § 6.1](./10-data-model.md#61-state-file-idsnap).

**Memory file** (`<id>.mem`) holds the guest RAM dump, full or sparse-of-dirty. Page-aligned. Not part of the state file.

When upstream documentation says "snapshot", it usually means the state file specifically; we are explicit because the distinction matters at the API boundary (`mem_file_path` is a separate field).

### Jailer vs squib-jail

**jailer** is upstream Firecracker's privilege-drop / chroot binary on Linux. It uses cgroups, netns, seccomp, and PID namespaces.

**squib-jail** is squib's drop-in replacement on Darwin: same flag set, but cgroups / netns / pid-ns are accept-and-warn (no Darwin equivalent), seccomp is N/A, and the genuine work is `chroot(2)`, `setrlimit`, `setuid`, `setgid`, plus an optional macOS `sandbox-exec` profile. See [40-jailer.md](./40-jailer.md).

### GIC vs GICD vs GICR

**GIC** = the Generic Interrupt Controller as a whole (architecture v3 in squib).

**GICD** = the Distributor MMIO region. One per VM. Lives at `0x0800_0000`.

**GICR** = a Redistributor MMIO region. One per vCPU. Lives at `0x080A_0000 + vcpu * 0x20000` (128 KiB stride).

`hv_gic_*` (macOS 15+) manages all three transparently; squib does not emulate them in userspace. See [12-hvf-backend.md § 6](./12-hvf-backend.md#6-gic--hv_gic_-only).

### MMDS vs metadata

**MMDS** (microVM metadata service) is the link-local HTTP service at `169.254.169.254` reachable from inside the guest. Same shape as EC2 IMDS.

**metadata** in upstream Firecracker docs is sometimes a synonym; in squib, we say *MMDS data* when we mean the JSON tree the service serves, to disambiguate from VM configuration metadata (the `--metadata <path>` CLI flag, which seeds the tree at startup).

### Boot source vs kernel image

**Boot source** = the `/boot-source` API endpoint's payload: `kernel_image_path`, `initrd_path`, `boot_args`. The "source" of the boot.

**Kernel image** = the file at `kernel_image_path`. May be raw `Image`, `Image.gz`, `Image.zst`, or PE. squib decompresses at config-load time, not at boot.

### vCPU exit vs VM exit

In x86 KVM literature, **VM exit** (VMEXIT) is the hardware transition from guest to host. squib uses **vCPU exit** specifically to refer to a value of the `VmExit` enum returned by `Vcpu::run`. The two terms are not interchangeable: one is a hardware event, the other is a Rust value type. See [10-data-model.md § 3](./10-data-model.md#3-vmexit--the-vcpu-run-loop-algebra).

### Pre-boot vs post-boot vs running

**Pre-boot** = the VM is in `Uninitialized` or `NotStarted`. Configuration endpoints accept; some PATCH endpoints reject.

**Post-boot** = the VM has transitioned at least once into `Running` (it may currently be `Paused`). Some endpoints (e.g. `PUT /boot-source`) reject; others (e.g. `DELETE /drives/{id}`) only accept post-boot.

**Running** = the VM state, narrower than post-boot. At least one vCPU is active. `Paused` is *post-boot but not running*.

See the state diagram in [11-runtime-core.md § 3](./11-runtime-core.md#3-lifecycle).

### Dirty page vs cold page

**Dirty page** (in this project) = a guest physical page that has been written by the guest since the most recent clean checkpoint. Tracked in a shadow `Vec<AtomicU64>` per RAM region. See [16-snapshots.md § 4](./16-snapshots.md#4-dirty-page-tracking).

**Cold page** = a page that has never been touched by the guest since restore began. Used in postcopy / lazy-restore contexts: cold pages are served on demand by the pager. See [16-snapshots.md § 5](./16-snapshots.md#5-postcopy--lazy-restore).

These are orthogonal: a page can be cold (never read since restore) and not dirty (never written), or hot (touched) and dirty (modified), etc.

## 3. Cross-references

- ↔ Spec set entry point: [index.md](./index.md)
