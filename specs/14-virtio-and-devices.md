---
title: 14-virtio-and-devices — MMIO bus, transport, and per-device designs
type: design
status: draft
last_updated: 2026-05-03
depends_on: 11-runtime-core.md, 13-arch-and-boot.md
---

# 14 · Virtio & Devices — MMIO bus, transport, and per-device designs

Status: draft · Owner: squib-bus + squib-virtio · Depends on: [11-runtime-core.md](./11-runtime-core.md), [13-arch-and-boot.md](./13-arch-and-boot.md)

## 1. Purpose

Pin the device model: the MMIO bus, the virtio-MMIO transport, and the contract each virtio device implements. All transports are virtio-MMIO; PCI is out of scope for 1.0 (`--enable-pci` accepts and warns). See [21-api-compat-matrix.md § 3](./21-api-compat-matrix.md#3-cli-flag-compatibility).

## 2. Bus

`squib-bus` implements an MMIO router. Ported from `vendors/libkrun/src/devices/src/bus.rs` (or Firecracker — equivalent shape):

```rust
pub struct Bus {
    devices: BTreeMap<BusRange, Arc<Mutex<dyn BusDevice>>>,
}

pub trait BusDevice: Send {
    fn read(&mut self, offset: u64, data: &mut [u8]) -> Result<()>;
    fn write(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    fn debug_label(&self) -> &str;
}
```

The VMM's MMIO exit handler does `bus.dispatch(addr, RW, data)`; the bus binary-searches the `BTreeMap` and forwards to the device under its mutex. Lock contention is bounded — each device serializes its own queues, and the only cross-device traffic is read/write to the same MMIO range (impossible by construction).

Per CLAUDE.md § Async & Concurrency, we considered `DashMap` for the device map; the access pattern (rare insert at boot, frequent lookup) favours `BTreeMap` for cache locality and ordered range queries.

## 3. virtio-MMIO transport

`squib-virtio::transport` ported from cloud-hypervisor `virtio-devices/src/transport/mmio.rs` (Apache-2.0). Uses upstream `virtio-queue` for descriptor handling.

Each `VirtioMmioDevice` owns:

- A `BusDevice` impl exposing the [virtio v1.2 MMIO register layout](https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-1340002).
- `Arc<GuestMemory>` for descriptor reads and DMA.
- One or more `VirtQueue` handles (one per virtio queue).
- An `IrqLine` to the GIC: SPI 16..47 mapped per slot at `0x0A00_0000 + slot * 0x1000`.

Notification path:

- Guest writes to `QueueNotify` MMIO register → exit → bus dispatch → device handler reads queue index → drains descriptor chain → injects IRQ via `IrqLine` if `VIRTIO_F_NOTIFY_ON_EMPTY` semantics warrant.

Configuration space writes are mediated through the device's `WriteConfig` callback; pre-`DRIVER_OK` writes are honored, post-`DRIVER_OK` writes return `VIRTIO_F_BAD_FEATURE` per the spec.

## 4. Device catalogue

| Device | Source | Notes |
|--------|--------|-------|
| virtio-block | port from cloud-hypervisor + rust-vmm `block` | Sync engine (default), Async engine via tokio `spawn_blocking` against `F_NOCACHE`-opened files |
| virtio-net | port from cloud-hypervisor + new `squib-net::vmnet` host backend | NAT default; bridged gated on entitlement; userspace via `gvproxy` |
| virtio-vsock | port from libkrun (TSI included) | UDS multiplex protocol bit-identical to upstream Firecracker; TSI is the killer feature |
| virtio-balloon | port from cloud-hypervisor | `madvise(MADV_DONTNEED)` for free-page reporting |
| virtio-rng (entropy) | port from cloud-hypervisor or fresh (~50 LOC) | Source: `aws-lc-rs::rand::SystemRandom` |
| virtio-console (serial) | port from libkrun | UART backend writes to file or FIFO per `/serial` config |
| virtio-pmem | fresh (small) | Memory-mapped file as a pmem device |
| virtio-mem | fresh + cloud-hypervisor reference | Memory hotplug; uses `Vm::map_memory` slot management |
| boot-timer | fresh (trivial virtio device) | Records time-to-userspace |

### 4.1 virtio-block

```rust
pub struct BlockConfig {
    pub drive_id: String,
    pub path_on_host: PathBuf,
    pub is_root_device: bool,
    pub is_read_only: bool,
    pub cache_type: CacheType,           // Unsafe | Writeback
    pub io_engine: IoEngine,             // Sync | Async
    pub partuuid: Option<String>,
    pub rate_limiter: Option<RateLimiter>,
}
```

- **Sync engine**: every queue notification handler reads/writes synchronously on the device thread. Suitable for low-IOPS workloads.
- **Async engine**: queue notifications dispatch onto `tokio::task::spawn_blocking`, which calls `pread`/`pwrite` against an `F_NOCACHE`-opened fd. The macOS analogue of Linux `O_DIRECT`. ≥ 100 K IOPS target on Apple SSD ([71-performance-budgets.md § 3](./71-performance-budgets.md#3-block-io)).
- Rate limiter: token bucket at the per-device queue level, ported from upstream rust-vmm `rate-limiter`.

The libkrun-style `dispatch_io` block backend is deferred to day-2; tokio + `spawn_blocking` carries us through 1.0. Recorded as [99-key-decisions.md § D7](./99-key-decisions.md#d7-block-io-tokio-spawn_blocking-not-dispatch_io).

### 4.2 virtio-net

Frontend ported from cloud-hypervisor `virtio-devices/src/net.rs`. Host backend lives in [30-networking.md](./30-networking.md) — virtio-net is parameterised over a `NetBackend` trait so the frontend is host-agnostic.

`MmdsInterceptor` in [15-mmds.md § 3](./15-mmds.md#3-packet-interception) sits between the frontend's RX/TX queues and the host backend, peeling off ARP and TCP-to-MMDS-IP frames before they hit the wire.

### 4.3 virtio-vsock

Ported from `vendors/libkrun/src/devices/src/virtio/vsock/`. Two modes:

- **Plain mode** (default): UDS multiplex protocol bit-identical to upstream Firecracker. Host-initiated `CONNECT <port>\n` → `OK <port>\n`; guest-initiated `<uds_path>_<port>` listener.
- **TSI mode** (opt-in via `"squib": { "vsock_tsi": true }`): guest opens AF_VSOCK sockets and we transparently proxy to host AF_INET / AF_UNIX. Useful for Lambda-shaped guests. Off by default — TSI changes vsock semantics in a non-Firecracker-compatible way. See [99-key-decisions.md § D8](./99-key-decisions.md#d8-tsi-vsock-off-by-default).

> **Guest kernel requirement.** TSI is a libkrun extension that requires a *cooperating guest kernel* — the AF_VSOCK socket bytes have to land in libkrun's TSI dispatcher, which only happens with libkrun's guest kernel patches. A stock upstream Linux kernel with `vsock_tsi: true` enabled on the host does **not** transparently get host AF_INET; instead the AF_VSOCK sockets behave as plain vsock and the user sees no benefit. squib emits a startup warning when `vsock_tsi: true` is configured to make this expectation visible. Document in `docs/macos-setup.md`.

### 4.4 virtio-balloon

Ported from cloud-hypervisor. Inflate / deflate via the standard virtio-balloon protocol. Free-page hinting and free-page reporting both implemented; reporting uses `madvise(MADV_DONTNEED)` to actually return memory to the OS on macOS.

### 4.5 virtio-rng (entropy)

`/dev/urandom`-backed by default; `aws-lc-rs::rand::SystemRandom` is the source. Per CLAUDE.md § Cryptography, never `thread_rng()` for security-sensitive randomness. ~50 LOC.

### 4.6 virtio-console (serial)

Frontend ported from libkrun. UART backend writes to a regular file, a FIFO (`mkfifo` works on macOS), or `stdout` per the `/serial` PUT config. Same shape as Firecracker.

### 4.7 virtio-pmem and virtio-mem

- **virtio-pmem**: a memory-mapped file exposed as persistent memory to the guest. `mmap(file, MAP_SHARED)` plus a `Vm::map_memory` registration; flush semantics honor `VIRTIO_PMEM_REQ_TYPE_FLUSH`.
- **virtio-mem**: memory hotplug. The device exposes a memory region but only some of it is mapped at boot; the guest requests `plug` / `unplug` via the virtio-mem queue; we issue `Vm::map_memory` / `Vm::unmap_memory` against per-block ranges. Verified that HVF allows the unmap/remap pattern at runtime (an open question in early drafts; resolved in week 8 of [91-impl-plan.md § 6](./91-impl-plan.md#6-phase-3-devices-and-mmds)).

### 4.8 boot-timer

A trivial virtio device with no queues; reads the host monotonic clock and exposes it as a config-space register. Used to measure time-to-userspace. Fresh code, ~30 LOC.

## 5. MMIO slot allocation

32 slots at `0x0A00_0000 + slot * 0x1000`, SPI 16..47. Allocation order at boot:

```
slot 0  → virtio-block (root)
slot 1  → virtio-net   (eth0)
slot 2  → virtio-vsock
slot 3  → virtio-balloon (if /balloon configured)
slot 4  → virtio-rng    (if /entropy configured)
slot 5  → virtio-console (if /serial configured)
slot 6+ → additional drives, additional NICs, virtio-pmem, virtio-mem, boot-timer
```

The allocator is in `squib-vmm::builder` and emits the matching FDT `virtio_mmio@...` nodes.

## 6. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-DEV-1 | Every virtio device passes its corresponding upstream Firecracker functional test. | Compat suite ([72-testing-strategy.md § 3](./72-testing-strategy.md#3-compat-suite)) |
| I-DEV-2 | The MMIO bus dispatches reads / writes to exactly one device or returns `Error::NoDevice` (which surfaces as `MmioReadFault` to the guest, never as a panic). | Property test with random addresses |
| I-DEV-3 | Rate limiters bound aggregate throughput within ±5% of the configured rate. | Per-device benchmark |
| I-DEV-4 | virtio-mem hotplug `plug` / `unplug` of an N-block range performs exactly N `Vm::map_memory` / `Vm::unmap_memory` calls. | Unit test with a stub `Vm` |
| I-DEV-5 | Device config-space writes after `DRIVER_OK` return `VIRTIO_F_BAD_FEATURE` per the virtio spec. | Per-device unit test |

## 7. Cross-references

- ← Depends on: [11-runtime-core.md](./11-runtime-core.md), [13-arch-and-boot.md](./13-arch-and-boot.md), [10-data-model.md](./10-data-model.md)
- → Consumed by: [15-mmds.md](./15-mmds.md), [16-snapshots.md](./16-snapshots.md), [20-firecracker-api.md](./20-firecracker-api.md), [30-networking.md](./30-networking.md)
- ↔ Related research: [docs/research/firecracker-subsystems.md](../docs/research/firecracker-subsystems.md), [docs/research/hvf-prior-art-deep-dive.md](../docs/research/hvf-prior-art-deep-dive.md) (cloud-hypervisor / libkrun device porting plan)
