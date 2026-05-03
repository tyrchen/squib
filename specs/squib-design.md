---
title: squib — Architecture & Design
type: design
status: draft
last_updated: 2026-05-03
depends_on: squib-prd.md, docs/research/*
supersedes: prior VZ-default design (2026-05-03 morning)
---

# squib — Architecture & Design

## 1. Shape

Single-process Rust VMM running on Apple Silicon. Exposes Firecracker's HTTP API over a Unix socket. Boots aarch64 Linux microVMs via Apple Hypervisor.framework (HVF) directly — no VZ.framework, no compile-time `cfg` split, no runtime `--hypervisor` flag. Targets macOS 15 Sequoia minimum, macOS 26 Tahoe recommended.

```
┌──────────────────────────────────────────────────────────────────┐
│ apps/squib  (binary; codesigned with hypervisor + vmnet entitlements)
│  ├── CLI (clap) — Firecracker-compatible flag set                 │
│  └── runtime: spawns API server + VMM event loop                  │
└──────────────────────────────────────────────────────────────────┘
                       │ axum on UDS                  │ EventManager
        ┌──────────────┴─────────────┐  ┌─────────────┴────────────┐
        │  squib-api                 │  │  squib-vmm               │
        │  • OpenAPI-faithful routes │◄─┼► • boot orchestration   │
        │  • action queue → VMM      │  │  • virtio device manager │
        │  • static config loader    │  │  • per-vCPU run loop     │
        │  • {"fault_message":"..."} │  │  • snapshot save/restore │
        └──────────────┬─────────────┘  └─────────────┬────────────┘
                       │                              │
        ┌──────────────┴──────────────────────────────┴────────────┐
        │  squib-core   (portable types, no I/O)                   │
        │  Hypervisor / Vm / Vcpu traits (alioth-shaped)           │
        │  VmExit, GuestRange, Protection, Error                   │
        │  Device, BlockBackend, NetBackend, VsockBackend          │
        │  RateLimiter, MmdsStore, Snapshot                        │
        └──┬───────────┬───────────┬───────────┬───────────┬───────┘
           │           │           │           │           │
   ┌───────┴────┐ ┌────┴────┐ ┌────┴────┐ ┌────┴────┐ ┌────┴───────────┐
   │ squib-hv   │ │squib-fdt│ │squib-   │ │squib-   │ │ squib-host     │
   │ HVF binding│ │FDT      │ │loader   │ │snapshot │ │ vmnet, vsock-  │
   │ via apple- │ │builder  │ │PE+gz+zst│ │bitcode+ │ │ uds, gvproxy,  │
   │ visor      │ │(vm-fdt) │ │(linux-  │ │mach-exc │ │ Mach-exc pager │
   │ (the only  │ │         │ │loader)  │ │postcopy │ │                │
   │ unsafe XS) │ │         │ │         │ │         │ │                │
   └────────────┘ └─────────┘ └─────────┘ └─────────┘ └────────────────┘
```

## 2. Hypervisor: HVF, no alternatives

Squib does not have a backend abstraction for production purposes. We implement one trait surface — alioth-shaped `Hypervisor` / `Vm` / `Vcpu` / `VmExit` — but only the HVF impl ships. The trait exists for **testability** (a `MockHypervisor` for unit tests) and **escape hatch** (if Apple ships a successor API), not for runtime polymorphism.

The HVF binding goes through the `applevisor` crate (`features = ["macos-26-0"]`) inside the **single** crate `squib-hv`. Every `unsafe` line in the workspace lives there. All other crates declare `#![forbid(unsafe_code)]`.

### Why HVF and not VZ

The earlier design considered VZ-default for time-to-MVP. That was the right calculus when "what works in 6 weeks" was the goal. Under the current direction — **performance is the priority and 1.0 ships full feature parity** — VZ disqualifies itself on three counts: (a) closed device model rules out custom virtio-MMIO devices and per-queue rate limiters; (b) no PVH or kernel-level boot tuning, costing boot-time budget; (c) no dirty-page tracking, killing Diff snapshots. HVF gives us the per-µs control and the right primitives.

### Why macOS 15 minimum

`hv_gic_create` and the `hv_gic_*` family land in macOS 15 Sequoia. With them we get a hypervisor-managed GICv3 — no userspace distributor/redistributor emulation, no MMIO trap handling for GIC accesses, no LR shadow, no pending/active bitmap arbitration. libkrun's pre-15 userspace path is ~3K LoC of fiddly state machine; we deliberately do not carry it.

The cost: users on macOS 14 cannot run squib. The benefit: our day-1 GIC is one struct that calls Apple APIs.

## 3. Workspace layout

```
apps/
  squib/                  → CLI binary, code-signed with entitlements
  squib-jail/             → drop-in jailer shim (Firecracker-flag-compatible)

crates/
  squib-core/             → portable types & traits (Hypervisor / Vm / Vcpu / VmExit / GuestRange / ...)
  squib-hv/               → HVF binding via applevisor; *the* unsafe boundary
  squib-arch/             → aarch64 layout, vCPU initial regs, sysreg list, ESR_EL2 decode
  squib-fdt/              → FDT builder via vm-fdt (cpus / memory / chosen / psci / gic / timer / virtio-mmio)
  squib-loader/           → kernel loader: Image / Image.gz (flate2) / Image.zst (zstd) / PE (linux-loader)
  squib-bus/              → MMIO bus + BusDevice trait
  squib-virtio/           → virtio-MMIO transport + device subcrates (block, net, vsock, balloon, rng, console)
  squib-gic/              → in-kernel GICv3 wrapper (hv_gic_*) — no userspace fallback
  squib-mmds/             → ported from Firecracker (dumbo + mmds)
  squib-net/              → vmnet integration (shared/bridged/host) + gvproxy embed for userspace mode
  squib-snapshot/         → bitcode + serde state file; full + sparse-of-dirty memory file; Mach-exc postcopy
  squib-vmm/              → VMM core: builder, vCPU thread, device manager, event loop
  squib-api/              → Firecracker-compatible REST + JSON config loader (axum on UDS)
```

This mirrors the alioth/libkrun layout with squib-specific names. Each crate has a single responsibility; cross-crate types live in `squib-core`.

### Workspace deps (highlights)

- **HVF**: `applevisor = "1.0"` (features = `["macos-26-0"]`) — only consumed by `squib-hv`.
- **rust-vmm**: `vm-memory = "0.17"`, `vm-fdt` (latest), `linux-loader = "0.13"` (with `pe` feature), `virtio-queue` (latest), `virtio-bindings` (latest).
- **Snapshot**: `bitcode = "0.6"`, `serde`.
- **Networking host side**: hand-rolled FFI in `squib-net::sys` for `vmnet.framework`; bundled `gvproxy` binary for userspace mode.
- **Mach exceptions**: `mach2 = "0.4"` plus hand-rolled FFI for the MIG-generated message types.
- **API server**: `axum`, `tokio`, `tower-http`.
- **CLI**: `clap` with `derive`.
- **Validation**: `validator`.
- **Compression**: `flate2`, `zstd`.
- **Observability**: `tracing`, `tracing-subscriber`.

We **do not** depend on `vmm-sys-util`, `kvm-bindings`, `kvm-ioctls`, `vhost-*`, or `seccompiler` — Linux-only crates that have no business in our build graph.

## 4. The portable trait surface (`squib-core`)

Adopted from `google/alioth/alioth/src/hv/hv.rs` (Apache-2.0). Associated types, not `dyn Trait`, for the per-VM type hierarchy:

```rust
pub trait Hypervisor: Send + Sync {
    type Vm: Vm;
    fn create_vm(&self, cfg: &VmConfig) -> Result<Self::Vm>;
    fn capabilities(&self) -> BackendCapabilities;
}

pub trait Vm: Send + Sync {
    type Vcpu: Vcpu;
    fn create_vcpu(&self, idx: u32, mpidr: u64) -> Result<Self::Vcpu>;
    fn map_memory(&self, host: *mut u8, ipa: u64, len: u64, perms: Protection) -> Result<()>;
    fn unmap_memory(&self, ipa: u64, len: u64) -> Result<()>;
    fn protect_memory(&self, ipa: u64, len: u64, perms: Protection) -> Result<()>;
    fn create_gic(&self, vcpu_count: u32) -> Result<Box<dyn Gic>>;
    fn save_state(&self) -> Result<VmState>;
    fn restore_state(&self, s: VmState) -> Result<()>;
}

pub trait Vcpu: Send {
    fn run(&mut self, ctx: &mut RunContext) -> Result<VmExit>;
    fn cancel(&self);                       // hv_vcpus_exit
    fn get_reg(&self, reg: Reg) -> Result<u64>;
    fn set_reg(&mut self, reg: Reg, val: u64) -> Result<()>;
    fn get_sys_reg(&self, reg: SysReg) -> Result<u64>;
    fn set_sys_reg(&mut self, reg: SysReg, val: u64) -> Result<()>;
    fn set_pending_irq(&mut self, irq: Irq) -> Result<()>;
}

pub enum VmExit {
    Mmio { addr: u64, write: bool, data: SmallVec<[u8; 8]> },
    Hvc { imm16: u16, x: [u64; 4] },                        // PSCI dispatch
    Smc { imm16: u16, x: [u64; 4] },
    SystemRegister { read: bool, op0: u8, op1: u8, crn: u8, crm: u8, op2: u8, xt: u8 },
    Wfi,
    Wfe,
    VtimerActivated,
    Brk,
    Reset,
    Shutdown,
    Cancelled,
    InternalError(String),
}
```

`Reg` and `SysReg` are squib enums covering exactly the registers we touch — not the full ARMv8 set. Adding a register is a one-line enum extension plus a mapping in `squib-hv`.

`VmExit` matches the union of HVF's `hv_vcpu_exit_t` decoded variants and the libkrun `VcpuExit` enum we're porting. It is explicitly aarch64-shaped: no `Pio`, no `Hlt`, no x86 variants.

## 5. vCPU run loop

Ported essentially verbatim from `containers/libkrun/src/hvf/src/lib.rs::HvfVcpu::run`, with FFI calls translated to `applevisor`:

```
loop {
    pre_run_housekeeping();
        // - if pending MMIO read result, write to dst register
        // - if pending_advance_pc, set PC += 4
        // - if vcpu_list.has_pending_irq(), set_pending_irq
    match applevisor_vcpu.run() {
        VTIMER_ACTIVATED => set vtimer_masked = true; return VtimerActivated;
        CANCELLED => return Cancelled;
        EXCEPTION => decode ESR_EL2:
            EC_DATAABORT (0x24) => Mmio { ... } (set pending_advance_pc)
            EC_HVC      (0x16) => return Hvc { ... }   // VMM dispatches PSCI
            EC_SMC      (0x17) => return Smc { ... }
            EC_MSR/MRS  (0x18) => return SystemRegister { ... }
            EC_WFx      (0x01) => return Wfi or Wfe (compute timer deadline)
            EC_BRK      (0x3c) => return Brk;
    }
}
```

### Threading

Each vCPU runs on a dedicated `std::thread` (HVF's pthread-affinity contract: every `hv_vcpu_*` call must come from the creating thread). Lifecycle is actor-shaped: the vCPU thread receives commands (`Pause`, `Resume`, `SaveState`, `Shutdown`) on an mpsc channel, and signals back via a oneshot for state snapshots.

Cancellation is `applevisor::Vcpu::exit()` (which wraps `hv_vcpus_exit`) — async, idempotent, callable from any thread. We do not use signals.

## 6. PSCI dispatch

Per `squib-arch::psci`. Dispatch table (from `docs/research/aarch64-hvf-guest-stack.md` §4):

| Function ID | Handling |
|-------------|----------|
| `PSCI_VERSION` (0x84000000) | Return 0x0001_0001 (PSCI 1.1) in X0 |
| `CPU_ON` (0xC4000003) | Find target vCPU actor; if Off, set PC/X0/PSTATE/SCTLR_EL1 reset, signal actor; else ALREADY_ON |
| `CPU_OFF` (0x84000002) | Park current actor in Off state; never returns |
| `AFFINITY_INFO` (0xC4000004) | Return 0=ON, 1=OFF, 2=ON_PENDING |
| `MIGRATE_INFO_TYPE` (0x84000006) | Return 2 (TOS not present) |
| `SYSTEM_OFF` (0x84000008) | Plumb to VMM control plane → exit Running, mark Shutdown |
| `SYSTEM_RESET` (0x84000009) | Plumb to VMM → tear down + recreate |
| `PSCI_FEATURES` (0x8400000A) | Return SUCCESS only for the IDs we implement |
| Everything else | Return NOT_SUPPORTED |

After dispatch, advance PC by 4 (HVC does not auto-advance under HVF).

## 7. Memory layout (concrete)

From `docs/research/aarch64-hvf-guest-stack.md` §11:

```
0x0000_0000 .. 0x07FF_FFFF  (128 MiB)  reserved low MMIO
0x0800_0000 .. 0x0800_FFFF  ( 64 KiB)  GICD       (size from hv_gic_get_distributor_size)
0x0809_0000 .. 0x0809_0FFF  (  4 KiB)  PL031 RTC  (optional)
0x080A_0000 .. variable     (128 KiB×N) GICR      (N = vCPUs)
0x0900_0000 .. 0x0900_0FFF  (  4 KiB)  PL011 UART (SPI 1)
0x0A00_0000 .. 0x0A01_FFFF  (128 KiB)  virtio-mmio (32 × 4 KiB; SPIs 16..47)
0x4000_0000 .. 0x7FFF_FFFF  (  1 GiB)  reserved (firmware sandbox; unused)
0x8000_0000                            DRAM start
  +0x0020_0000                         kernel Image load (2 MiB-aligned)
  +0x1000_0000                         initrd (heuristic, ≥256 MiB above kernel)
  ..ram_end - 0x0020_0000              FDT (last 2 MiB of RAM)
  ..ram_end                            RAM end
```

Bounds: `0x8000_0000 ≤ ram_end < 0x00FF_8000_0000` (max 1022 GiB). DRAM base matches Firecracker's aarch64 layout for API parity. MMIO base matches QEMU virt / libkrun for kernel-config familiarity.

## 8. GIC: `hv_gic_*` only

`squib-gic` wraps `applevisor::gic::*`. Lifecycle:
1. `hv_gic_config_create` → set distributor base, redistributor base, MSI region (we don't use MSI in 1.0; configure with empty range).
2. `hv_gic_create(cfg)` — fixes the layout.
3. SPI assertion: `hv_gic_set_spi(intid, level)` for level-triggered, edge-pulses via on/off pair.
4. Snapshot: `hv_gic_state_create / get_size / get_data` — opaque blob, serialize alongside vCPU state.

There is **no** userspace distributor/redistributor emulation. macOS 15 minimum is enforced.

## 9. Boot orchestration

Per `squib-vmm::builder`:

```
build_microvm_for_boot(VmResources):
  determine vcpu_count, mem_size_mib, kernel_path, initrd_path, boot_args
  open kernel; auto-detect compression by magic bytes (gzip/zstd/raw); decompress as needed
  parse aarch64 boot header; resolve text_offset; compute kernel_load_addr
  mmap guest memory (anonymous, RWX); register with HVF via Vm::map_memory
  load kernel via linux-loader::pe::PE::load
  if initrd: write initrd at 0x9000_0000 (or kernel_end + 256 MiB, whichever larger)
  build FDT in last 2 MiB of RAM:
    /, /chosen, /memory, /cpus[N], /psci, /timer, /intc (gicv3), /pl011, /virtio_mmio[K]
    chosen.bootargs = boot_args (verbatim, no defaults injected unless absent)
  create_gic(vcpu_count); place GICD/GICR at fixed addresses
  create vCPUs; each spawns its OS thread, parks at CPU_OFF except vCPU 0
  on vCPU 0: set PC = kernel_load_addr, X0 = fdt_addr, PSTATE = 0x3C5
  return (vcpus, devices, mmds, gic, snapshot_handle)
```

`PUT /actions {InstanceStart}` flips vCPU 0 to Running. Other vCPUs come up via PSCI `CPU_ON`.

## 10. Devices

All virtio-MMIO. PCI is out of scope for 1.0.

| Device | Source | Notes |
|--------|--------|-------|
| virtio-block | port from cloud-hypervisor + rust-vmm `block` | Sync engine (default), Async engine via tokio `spawn_blocking` against `F_NOCACHE`-opened files |
| virtio-net | port from cloud-hypervisor + new `squib-net::vmnet` host backend | NAT default; bridged gated on entitlement; userspace via `gvproxy` |
| virtio-vsock | port from libkrun (TSI included) | UDS multiplex protocol bit-identical to upstream Firecracker; TSI is the killer feature |
| virtio-balloon | port from cloud-hypervisor | `madvise(MADV_DONTNEED)` for free-page reporting |
| virtio-rng (entropy) | port from cloud-hypervisor or fresh (~50 LOC) | Source: `aws-lc-rs` |
| virtio-console (serial) | port from libkrun | UART backend writes to file or FIFO per `/serial` config |
| boot-timer | fresh (trivial virtio device) | Records time-to-userspace |

The MMIO bus is `BTreeMap<BusRange, Arc<Mutex<dyn BusDevice>>>` ported from libkrun (or Firecracker — equivalent shape).

### MMDS interception

Network interfaces with MMDS bound get their guest-side packet path routed through `squib-mmds::dumbo`. We intercept ARP and TCP-to-MMDS-IP at the virtio-net device, so those frames never reach vmnet. Same pattern as upstream Firecracker; the dumbo and mmds crates port directly.

## 11. Snapshots

### State file

`bitcode + serde`-encoded `MicrovmState`, with the upstream Firecracker outer container:

```
| magic_id (u64)  | 0x07101984_AAAA_0000 (aarch64)
| version         | semver string
| state           | bitcode-encoded MicrovmState blob
| crc64           | over magic + version + state
```

`MicrovmState` includes:
- `vm_info`: mem_size, smt (always false), cpu_template, boot_source.
- `vcpu_states[N]`: GP regs, FP/SIMD regs, curated sysreg subset (about 100 regs we touch — not the full KVM list).
- `device_states`: per-device config + virtqueue cursors.
- `gic_state`: opaque blob from `hv_gic_state_get_data`.
- (Memory not in this file; separate.)

### Memory file

Full or sparse (lseek `SEEK_HOLE`/`SEEK_DATA`). Layout matches Firecracker's so a Linux Firecracker tool inspecting it sees a familiar shape. Sparse files contain only dirty pages.

### Dirty page tracking

`hv_vm_protect`-and-fault scheme implemented in `squib-snapshot::dirty`. After a clean checkpoint:
1. Strip `HV_MEMORY_WRITE` from the entire tracked range (single call per region).
2. Guest writes generate ESR `EC=0x24, WnR=1` exits. The vCPU exit handler sets the page bit in a shadow `Vec<AtomicU64>`, re-grants `HV_MEMORY_WRITE` on that page (or 2 MiB block), and resumes.
3. On snapshot: drain the bitmap, write only dirty pages.

Granularity is **2 MiB by default**, dropping to 4 KiB only when the dirty-rate heuristic in the tracker says so. This bounds the TLB-shootdown cost (which is the real performance limiter — see `docs/research/hvf-performance-and-snapshots.md` §2.3).

### Postcopy / lazy restore

Optional in 1.0; the infrastructure ships, the `Uffd` `mem_backend` accepts a Unix-socket path, and we serve pages via Mach exception ports. The implementation:
- Allocate guest RAM with `mach_vm_allocate`, immediately `mach_vm_protect(VM_PROT_NONE)`.
- `hv_vm_map` the region with full RWX (host-side `PROT_NONE` is what causes faults to surface).
- Register a task-level Mach exception port for `EXC_MASK_BAD_ACCESS` with `MACH_EXCEPTION_CODES`.
- A dedicated server thread runs `mach_msg(MACH_RCV_MSG)` and dispatches MIG `exception_raise` calls. On fault: copy bytes from snapshot file, `mach_vm_protect(VM_PROT_READ | VM_PROT_WRITE)`, reply `KERN_SUCCESS`.
- The vCPU exit handler does the same for guest-side stage-2 faults.

Save and forward to prior exception ports (LLDB attach must keep working).

## 12. API server (`squib-api`)

`axum` on `tokio::net::UnixListener`. Routes mirror `firecracker.yaml` exactly. Schemas are squib-defined Rust structs with `serde` annotations matching the Firecracker JSON shapes (snake_case in nested JSON, kebab-case at the static-config-file top level via `#[serde(rename_all = "kebab-case")]`).

Key middleware:
- `Server: Firecracker API` header on every response.
- `--http-api-max-payload-size` body limit (default 51200).
- `FaultMessage` extractor that produces `(StatusCode, Json<FaultMessage>)` for every 4xx.

Handlers call into a `RuntimeApiController` that owns an `mpsc::Sender<(ApiAction, oneshot::Sender<ApiResponse>)>` to the VMM event loop. The controller validates state-machine constraints (pre-boot vs post-boot) before forwarding; the VMM never receives malformed actions.

The static-config-file path replays a deterministic sequence of internal `ApiAction`s — no parallel codepath, same validation rules.

## 13. Threading model

| Thread | Lives in | Notes |
|--------|----------|-------|
| API thread | `squib-api`, axum runtime | accept loop on the UDS, request handlers |
| VMM event loop | `squib-vmm`, current-thread tokio | EventManager subscribes to API actions, device events, timers, metrics flush |
| Per-vCPU thread × N | spawned by `squib-hv::Vm::create_vcpu` | dedicated OS thread (HVF affinity); receives commands on mpsc; runs `Vcpu::run` loop |
| Block I/O pool | `tokio` blocking pool | per-disk worker threads against `F_NOCACHE`-opened file descriptors |
| Mach-exception server | dedicated `std::thread` (`squib-mach-exc`) | only when postcopy is active |
| MMDS / dumbo | hosted on the VMM event loop | no dedicated thread |
| gvproxy (when `--network=userspace`) | child process via `tokio::process::Command` | UDS for control plane |

All cross-thread state mutation goes through the VMM event loop. The API thread never touches device or hypervisor state directly.

## 14. Networking (`squib-host`/`squib-net`)

vmnet integration via hand-rolled FFI in `squib-net::sys` (no maintained crate exists). Modes:

| Mode | Entitlement | Notes |
|------|-------------|-------|
| `--network=shared` (default) | `com.apple.vm.networking` (open / self-claimable) | NAT through host |
| `--network=bridged` | `com.apple.vm.networking` (restricted) | gated on Apple DTS request; ships disabled by default |
| `--network=host` | `com.apple.vm.networking` | host-only |
| `--network=userspace` | none | bundled `gvproxy` child process; no entitlement, slightly slower |

`host_dev_name` from the Firecracker API maps deterministically to a vmnet handle (`squib-tap-<iface_id>`), opaque to the caller.

## 15. vsock (`squib-virtio::vsock`)

Wire-identical to upstream Firecracker:
- Host-initiated: connect to `uds_path`, send `CONNECT <port>\n`, receive `OK <port>\n`.
- Guest-initiated: per-port AF_UNIX listener at `<uds_path>_<port>`.

Optional **TSI mode** (libkrun pattern): guest opens AF_VSOCK sockets and we transparently proxy to host AF_INET/AF_UNIX. Useful for Lambda-shaped guests; opt-in via a squib-extension config field. Day-1 includes the implementation behind a `tsi: true` flag.

## 16. Logger / Metrics (`squib-vmm::logger`)

Reuse upstream's metric struct definitions (port from `src/vmm/src/logger/metrics.rs`). Output JSON shape preserved verbatim. macOS-irrelevant counters (e.g. `seccomp.num_faults`) are kept in the schema but pinned to zero rather than removed, so JSON consumers don't break.

Logger lines preserve `[level] origin: message` formatting; rate-limited by per-thread token-bucket. Targets accept regular files and FIFOs (`mkfifo` works on macOS).

## 17. CPU templates

aarch64-only. `squib-arch::cpu_templates`:
- `V1N1` — sysreg overrides corresponding to Graviton 1. Best-effort applied via `applevisor::Vcpu::set_sys_reg`.
- `cpu_template: "C3" | "T2" | "T2A" | "T2CL" | "T2S"` — x86 templates; accept-and-warn (logged once).
- `PUT /cpu-config` `reg_modifiers` and `vcpu_features` — applied where the registers exist on Apple Silicon; warned per unsupported.

## 18. Jailer (`apps/squib-jail`)

Standalone binary, same flag set as upstream `jailer`. Implements the safe subset on Darwin:
- `--id`, `--exec-file`, `--uid`, `--gid` → `chroot()`-equivalent (Darwin has `chroot(2)`), copy squib binary in, `setrlimit`, `setuid`/`setgid`, `execve`.
- `--cgroup`, `--parent-cgroup`, `--cgroup-version`, `--netns` → accept-and-warn.
- `--new-pid-ns` → accept-and-warn (no PID namespaces on Darwin); use `posix_spawn` for signal lineage decoupling.
- `--daemonize` → genuine: `setsid` + redirect 0/1/2 to `/dev/null`.
- `--macos-sandbox-profile <name>` (squib extension) → applies a bundled `sandbox_init` profile.

Exit codes match upstream.

## 19. Configuration

- CLI parsed via `clap` derive (see `apps/squib/src/cli.rs`).
- Static config file `--config-file <path>` parsed into `VmmConfig` mirror struct, kebab-case top-level keys, snake_case nested, same nullability rules as Firecracker.
- A `squib.yaml` host-side config (separate from the Firecracker JSON, never exposed on the API) holds squib-specific preferences: gvproxy path, Mach-exception thread name, default sandbox profile. Optional.

The Firecracker JSON also accepts a `squib` extension object that upstream Firecracker silently ignores (preserving file portability):

```json
{
  "squib": {
    "network": "shared" | "bridged" | "userspace",
    "vsock_tsi": false,
    "gvproxy_path": "/opt/squib/libexec/gvproxy"
  }
}
```

## 20. Error model

- All API failures produce `(StatusCode, Json<FaultMessage>)`.
- Library crates use `thiserror`-derived enum errors with `#[source]` chains.
- The CLI binary uses `anyhow` only at `main.rs`; library crates never use `anyhow`.
- `#![forbid(unsafe_code)]` everywhere except `squib-hv` and `squib-net::sys`.
- `cargo clippy -- -D warnings -W clippy::pedantic` is gating CI.
- `cargo +nightly fmt --check` is gating CI.

## 21. Security posture

Threat model: developer machine, trusted operator. Non-goals: production multi-tenant isolation. Rules:

- `unsafe` lives in two places: `squib-hv` (applevisor calls and a few `mach_*` helpers for postcopy) and `squib-net::sys` (vmnet FFI). Each `unsafe` block carries a `// SAFETY:` comment.
- Boundary input validation: every field deserialized from the API is bounded (length caps on strings, range caps on numbers via `validator`, regex on IDs).
- Path inputs are canonicalized and verified to not contain `..` or NUL bytes; for the chroot jail in `squib-jail`, re-canonicalize after open.
- Code-signing: binary signed with `com.apple.security.hypervisor` and `com.apple.vm.networking` (open entitlements, self-claimable). Hardened runtime flag set. Notarization in CI for releases.
- No secrets in logs (instance ID is fine; MMDS data is never logged at info level).

## 22. Performance targets (1.0)

- Cold boot to `/sbin/init`: **p50 ≤ 400 ms** on M2 Pro / M3.
- Memory overhead per microVM at idle: **≤ 15 MiB**.
- vCPU exit dispatch: **≤ 10 µs/exit** for MMIO-light workloads.
- vmnet shared-mode throughput: **≥ 1 Gbit/s** sustained.
- Block IO via tokio + spawn_blocking + F_NOCACHE: **≥ 100 K IOPS** on Apple SSD.

Measured per-release in `crates/squib-vmm/benches/`. Numbers are published, not borrowed.

## 23. Open design questions

1. **TSI default on or off in 1.0?** Off, opt-in via config — TSI changes vsock semantics in a non-Firecracker-compatible way. Decision deferred to first real workload feedback.
2. **`dispatch_io` block backend.** Day-2; tokio+`spawn_blocking` carries us through 1.0.
3. **Cross-host snapshot memory-only restore.** Accepted as a stretch goal; document as not supported in 1.0.
4. **Bundled Linux kernel for fast cold-boot.** Worth shipping a known-good aarch64 vmlinux + busybox initrd as part of `examples/` so users have a working start. Decision: yes, in 1.0.
5. **virtio-fs (file system passthrough)** for shared host directories. Not in Firecracker's API; out of scope unless we add a `squib` extension.

See `specs/squib-impl-plan.md` for the build sequence.
