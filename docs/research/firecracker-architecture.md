---
title: Firecracker Internal Architecture
status: research
audience: engineers planning the squib reimplementation on macOS
source: vendors/firecracker submodule (src/firecracker, src/vmm)
last_reviewed: 2026-05-03
---

# Firecracker Internal Architecture

This document maps Firecracker's internal architecture to identify the **KVM/Linux seams** so squib can replace the right modules with macOS analogues (Hypervisor.framework, vmnet, GCD, dispatch timers) while keeping device protocol implementations and the API surface intact.

## 1. Process layout & threads

Firecracker is a single-process VMM with a small fixed set of threads.

| Thread | Source | Role | Per-thread seccomp filter |
|--------|--------|------|---------------------------|
| API thread | `src/firecracker/src/api_server_adapter.rs` | runs `micro_http::HttpServer` on the Unix socket; converts requests into `VmmAction` | `api` |
| VMM event-loop thread (main) | `src/vmm/src/lib.rs` | hosts `EventManager`; subscribes to API requests, device events, periodic timers | `vmm` |
| vCPU threads (one per vCPU) | `src/vmm/src/vstate/vcpu.rs` | each runs an isolated `KVM_RUN` loop and handles its own exits | `vcpu` |

There are no permanent device worker threads in baseline Firecracker. Block I/O is async via `io_uring` driven by the VMM event loop; net device queue work runs on the same loop, signalled by EventFd.

### IPC pattern

```
API thread                           VMM event-loop thread                 vCPU threads
   │                                          │                                    │
   ├── ApiRequest (mpsc) ────────────────────►│                                    │
   │                                          │                                    │
   │                              EventManager::run()                              │
   │                                          │                                    │
   │                          RuntimeApiController                                 │
   │                                          ├── VcpuEvent::Pause/Resume ───────►│
   │◄── ApiResponse (mpsc) ───────────────────┤◄── VcpuResponse::Paused ──────────┤
```

Cross-thread signalling uses `EventFd` (`EFD_SEMAPHORE` mode) plus mpsc channels. The API thread implements `MutEventSubscriber` so the EventManager can wake on the `EventFd` and dispatch the queued `VmmAction`.

## 2. Crate layout under `src/vmm/src/`

| Module | Responsibility | OS coupling |
|--------|----------------|-------------|
| `arch/` | x86_64 / aarch64 platform code: GDT, IDT, MPTable, ACPI, FDT, PSCI, GIC | high (KVM ioctls, ABI assumptions) |
| `vstate/` | `Kvm`, `Vm`, `Vcpu`, `GuestMemoryMmap`; runs the exit loop | very high (KVM-only) |
| `devices/virtio/` | block, net, vsock, balloon, rng, mem, pmem device implementations | medium — virtio is portable, host backings are not |
| `devices/virtio/transport/mmio.rs` | virtio-MMIO register layout and dispatch | none |
| `device_manager/` | MMIO/PIO bus routing, device lifecycle | low |
| `builder.rs` | microVM construction: KVM init, kernel load, vCPU spawn, device attach | high |
| `persist.rs`, `vstate/snapshot.rs` | snapshot save/restore orchestration | high (KVM_GET_REGS/SET_REGS, dirty bitmaps) |
| `io_uring/` | async block I/O | Linux only |
| `rate_limiter/` | token bucket using `timerfd` | Linux only |
| `mmds/`, `dumbo/` | userspace TCP/IP stack for IMDS endpoint | none — pure logic |
| `snapshot/` | versioning, header layout for state files | none |
| `resources.rs`, `vmm_config/` | configuration structs (the API surface in Rust form) | none |
| `logger/`, `metrics.rs` | structured logging and counter/gauge metrics | low (file/FIFO IO) |

## 3. vCPU & VM lifecycle

The vCPU thread is the heart of the VMM. Each vCPU runs an independent state machine:

```
Vcpu::run(seccomp_filter)
├── apply_filter()                         # Linux seccomp via prctl + SYS_seccomp
├── StateMachine starts in Paused
│
├── Paused:        block on event_receiver
│   └── Resume → Running
│
├── Running:       loop
│   ├── kvm_vcpu.fd.run()                  # KVM_RUN ioctl (blocking)
│   │   ├── EINTR  → loop continue
│   │   └── Ok(VcpuExit::*)
│   └── handle_kvm_exit(exit)
│       ├── MmioRead/MmioWrite → mmio_bus.read/write
│       ├── Io(port,…) (x86)   → pio_bus.read/write
│       ├── SystemEvent(reset/shutdown) → Stopped
│       ├── Hlt (x86) / Debug / FailEntry / InternalError → arch handler or fatal
│
└── Pause: re-enter Paused
```

### KVM exit reasons handled

| `VcpuExit` | Handler | Comment |
|-----------|---------|---------|
| `MmioRead/MmioWrite(addr, &[u8])` | `mmio_bus` | the only device-exit path on aarch64 |
| `Io(port, dir, size, &[u8])` | `pio_bus` | x86 only; serial, i8042, RTC |
| `Hlt` | arch handler | x86 only |
| `SystemEvent(RESET|SHUTDOWN)` | stop the VM | both archs |
| `FailEntry`, `InternalError` | fatal — VMM exits | KVM-specific |
| `Debug(_)` | optional GDB stub | feature-gated |

### Interrupt injection

- x86_64: `KvmVcpu::inject_irq()` → `KVM_INTERRUPT` ioctl, with LAPIC state managed via `KVM_SET_LAPIC`.
- aarch64: `KvmVcpu::inject_irq()` → `KVM_INJECT_IRQ`; GICv3 state lives kernel-side.

A device that asserts an IRQ writes to a per-device `EventFd`, the VMM event loop wakes, advances the device queue, then calls into the vCPU's interrupt API. There is also `KVM_IRQFD` shortcut wiring used in places.

## 4. Memory model

Guest memory is anonymous-mmap'd in the host process, registered with KVM as a single (or a few) memory region(s).

- `vm-memory::MmapRegion` builds the host-side mapping.
- `Vm::set_user_memory_region` calls `KVM_SET_USER_MEMORY_REGION` with `{slot, guest_phys_addr, memory_size, userspace_addr, flags}`.
- `flags` includes `KVM_MEM_LOG_DIRTY_PAGES` when `track_dirty_pages` is true.
- Dirty bitmaps are stored in `AtomicBitmap` per region; `Vm::get_kvm_dirty_log()` retrieves the KVM-side bitmap and merges into Firecracker's view.
- virtio-mem extends this by pre-registering hot-pluggable slots; `VirtioMem` plugs/unplugs blocks at runtime.

Hugepages: when `huge_pages: "2M"` is set, the mmap is backed by hugetlbfs; the guest memory region's `userspace_addr` covers a huge-page-aligned mapping.

## 5. Device model

Firecracker uses **virtio-MMIO only** in its baseline (PCIe transport is recent/optional via `--enable-pci`). Device construction follows a fixed pattern:

```
DeviceManager::attach_*_device(config)
├── Construct device (Block / Net / Vsock / Balloon / Rng / Mem / Pmem)
├── Wrap in MmioTransport
├── Register on the MMIO bus at next free MMIO base address
├── Wire interrupt: IrqTrigger { irq_status: AtomicUsize, eventfd: EventFd }
└── At driver DRIVER_OK → device.activate(mem, interrupt) registers queue events
```

| Device | virtio type | Queues | Host backing |
|--------|-------------|--------|--------------|
| Block | 2 | 1 | regular file (Sync) or io_uring (Async); or vhost-user socket |
| Net | 1 | 2 (rx, tx) | Linux TAP via `/dev/net/tun` |
| Vsock | 19 | 3 (rx, tx, event) | Unix-socket multiplexer at `uds_path` (userspace virtio-vsock; not AF_VSOCK) |
| Balloon | 5 | 2–3 | `madvise(MADV_DONTNEED)` against host mmap |
| Rng (entropy) | 4 | 1 | `aws-lc-rs` CSPRNG |
| Mem | 24 | 1 | KVM memory slot operations |
| Pmem | 25 | 1 | mmap of file or memfd |

**MMIO register map** (virtio-MMIO v2):
- 0x000 magic / 0x004 version / 0x008–0x00C device_id, vendor_id, status, config_generation
- 0x010–0x024 feature negotiation
- 0x028–0x034 queue config
- 0x050 NOTIFY (write-only) — guest writes queue index here to kick

A guest write to NOTIFY signals an EventFd; the VMM event loop wakes the device handler, walks the available ring, processes descriptors, fills the used ring, and asserts the queue interrupt.

## 6. Boot protocol

`builder.rs::build_microvm_for_boot()` runs:

1. Load kernel via the `linux-loader` crate (auto-detects bzImage, ELF, or PVH note).
2. Optionally load initrd above the kernel.
3. Write the kernel command line to the boot parameter area.
4. For each vCPU: configure CPUID, MSRs, GDT/IDT/segment registers (x86) or PSTATE/PC/X0 (aarch64).
5. `configure_system_for_boot()`:
   - x86_64: build MPTable (and optionally ACPI tables for newer FC versions).
   - aarch64: build a Flattened Device Tree, set up GICv3 distributor/redistributor, PSCI.
6. vCPUs start Paused; `InstanceStart` flips them to Running.

Default x86_64 layout: kernel at `0x0010_0000`, initrd at `0x0200_0000`, MMIO above 1 GiB.

## 7. Architecture-specific code

### `arch/x86_64/`
- `msr.rs`: MSR enums, boot MSR setup (EFER, STAR, LSTAR, etc.).
- `vcpu.rs`: CPUID assembly, `KVM_SET_CPUID2`, register init.
- `regs.rs`: initial RIP/RSP/RAX, GDT/IDT pointers.
- `mptable.rs`: legacy MP table.
- `gdt.rs`: long-mode GDT.
- `xstate.rs`: AVX/AVX-512 state via XSAVE.
- `interrupts.rs`: LAPIC via `KVM_SET_LAPIC`.
- `layout.rs`: address constants (kernel base, MMIO holes).

### `arch/aarch64/`
- `vcpu.rs`: registers, GIC vCPU interface.
- `vm.rs`: GICv3 distributor/redistributor (in-kernel KVM model).
- `regs.rs`: PC, X0 (FDT addr), SP_EL1.
- `interrupts.rs`: SPI/SGI routing.
- `psci.rs`: PSCI SMC handler for CPU-on/CPU-off.
- `layout.rs`: memory map.

### macOS analogues at this layer

| KVM op | HVF analogue | Note |
|--------|--------------|------|
| `KVM_SET_REGS / KVM_SET_SREGS` | `hv_vcpu_set_reg`, `hv_vcpu_write_register` (x86), `hv_vcpu_set_sys_reg` (aarch64) | per-register, not bulk |
| `KVM_SET_CPUID2` | no API; CPUID intercepted via VMX_EXIT and emulated | squib must trap and rewrite |
| `KVM_SET_LAPIC` | not exposed; HVF sets up host LAPIC implicitly; MSI pathway differs | major rework on x86 macOS |
| `KVM_INJECT_IRQ` (aarch64) | `hv_vcpu_set_pending_interrupt` | direct mapping |
| `KVM_SET_USER_MEMORY_REGION` | `hv_vm_map` (Intel), `hv_vm_map_*` (aarch64) | one-shot map; no per-region flags table |
| `KVM_GET_DIRTY_LOG` | none baseline; macOS 14+ adds limited APIs (`hv_vm_*_protect`) | dirty-page tracking is a known gap |

## 8. Threading details

- API thread: blocking accept; produces `ApiRequest`s.
- VMM event loop: epoll (Linux) over EventFds. On macOS the equivalent is kqueue or libdispatch.
- vCPU threads: each owns its `KvmVcpu` handle; SIGRTMIN (`VCPU_RTSIG_OFFSET`) is sent to break out of `KVM_RUN` for pause/snapshot. The macOS equivalent is `hv_vcpus_exit` (an explicit cancel API), which is cleaner than signals.
- Thread startup uses a `Barrier` so that VMM does not proceed until each vCPU thread has installed its TLS and seccomp filter.

## 9. Seccomp / sandboxing

Per-thread BPF filters compiled from JSON (`seccompiler-bin`) into bitcode and embedded at build time. Categories: `vmm`, `api`, `vcpu`. Loaded via `prctl(PR_SET_NO_NEW_PRIVS)` then `seccomp(SECCOMP_SET_MODE_FILTER, …)`.

This is fundamentally Linux-only. macOS has no equivalent runtime syscall filter; squib should rely on:
- Apple-style code signing + entitlements.
- Optional `sandbox_init`/`sandbox-exec` profile around the binary.
- Privilege drop after init.

The flag must remain accepted for compatibility — `--seccomp-filter <path>` and `--no-seccomp` both no-op with a single warning.

## 10. Snapshot internals

`MicrovmState` (the in-memory aggregate that gets serialized) contains:

- `vm_info`: memory size, SMT, cpu_template, boot source, hugepages.
- `kvm_state`: capability modifiers and any KVM-specific saved state.
- `vm_state`: arch-specific (x86 cr0/cr3/cr4/efer, etc.; aarch64 GIC).
- `vcpu_states[]`: registers, sregs, MSRs, XSAVE / FP state.
- `device_states`: per-device configuration plus virtqueue cursors.

Memory is a separate file (full or sparse-of-dirty-pages). The state file is bitcode-encoded with a leading magic + version header (per the subsystems doc). Restore reverses the steps in `builder.rs`, with the additional UFFD path for postcopy paging.

## 11. micro-http

micro-http was a separate firecracker-microvm crate but was vendored in 2024. It is now part of the firecracker tree (still imported as a path/git dependency in `src/firecracker/Cargo.toml`). It is deliberately minimal: blocking sockets, single-thread driven by EventManager, supports GET/PUT/PATCH, content-length only (no chunked TE, no keep-alive pipelining beyond what the FSM allows). It is portable Rust — squib can use it as-is.

## 12. Portability map (Linux/KVM → macOS)

| Subsystem | Linux/KVM impl | macOS strategy | Difficulty |
|-----------|----------------|----------------|------------|
| KVM `Vm`/`Vcpu` lifecycle | `kvm_ioctls` + `/dev/kvm` | HVF (Intel) or HVF aarch64; or VZ as a higher-level alternative | high |
| vCPU exit loop | `KVM_RUN` + `VcpuExit` enum | `hv_vcpu_run` + VMCS exit fields → unified `VmExit` enum | high |
| MMIO dispatch | `MmioRead/Write` exit | mark MMIO range as unmapped/intercepted; trap and dispatch | medium |
| PIO (x86) | `Io` exit | not provided directly by HVF; minor: only legacy serial/i8042 use it — emulate via MMIO or trap I/O | medium |
| Interrupt injection | `KVM_INTERRUPT` | `hv_vcpu_set_pending_interrupt` (aarch64) / inject through VMCS (x86) | high |
| Memory registration | `KVM_SET_USER_MEMORY_REGION` | `hv_vm_map` | low (simpler) |
| Dirty-page tracking | `KVM_GET_DIRTY_LOG` + `KVM_MEM_LOG_DIRTY_PAGES` | shadow bitmap via `mprotect` write-fault traps; or HVF dirty-tracking API on macOS 14+ | high |
| TAP networking | `/dev/net/tun` ioctls | vmnet.framework (host/shared/bridged) | high — different abstraction |
| AF_VSOCK | Linux PF_VSOCK | userspace virtio-vsock bridges to Unix sockets — already that way in Firecracker; portable | low |
| io_uring (block async) | Linux io_uring | dispatch_io / GCD-backed thread pool / tokio | medium |
| timerfd (rate limiter) | Linux timerfd | dispatch source timer / tokio interval | low |
| seccomp | Linux BPF | accept-and-warn; rely on entitlements | n/a |
| GDB stub | KVM_EXIT_DEBUG | HVF debug regs API | medium |
| UFFD postcopy | userfaultfd | mach exception handler over write-protected memory; or fall back to eager load | high |
| MPTable / ACPI / FDT | generated, KVM enables in-kernel pieces | same generators; HVF needs IDT/GDT/etc. set explicitly | low |

## Key seams to design around

1. **`VmExit` abstraction**: a portable enum that subsumes both KVM `VcpuExit` and HVF's exit fields, keeping `arch/` free of `kvm_ioctls` types.
2. **`HypervisorBackend` trait**: encapsulates VM and vCPU creation, register get/set, run, interrupt injection, memory map. Implementations: `KvmBackend` (for tests / Linux build), `HvfBackend`, `VzBackend`.
3. **Device host-side traits**: `BlockBackend`, `NetBackend`, `VsockBackend` so the same virtio device implementation can switch between TAP and vmnet, between io_uring and GCD.
4. **Dirty-page tracking trait** isolates KVM dirty-log from a `mprotect`-based fallback.

The clean takeaway: **virtio devices, MMIO transport, MMDS/dumbo, snapshot serialization, and the API server are portable**. The KVM-tied modules (`vstate/`, much of `arch/`, the device host-backings, rate limiter timer source, and seccomp) are where the macOS work concentrates.
