---
title: 11-runtime-core — alioth-shaped trait spine
type: design
status: draft
last_updated: 2026-05-03
depends_on: 00-prd.md, 10-data-model.md
---

# 11 · Runtime Core — alioth-shaped trait spine

Status: draft · Owner: squib-core · Depends on: [00-prd.md](./00-prd.md), [10-data-model.md](./10-data-model.md)

## 1. Purpose

The squib-core crate is the **portable spine**: the traits, value types, and lifecycle every other crate links against. Zero OS dependencies, zero `unsafe`, no I/O. If you can build squib-core on a Linux host, you can read the trait surface without flipping back and forth to a backend.

This file pins the trait shapes, the threading rules, the panic policy, and the lifecycle. The HVF implementation lives in [12-hvf-backend.md](./12-hvf-backend.md).

## 2. Interface

Adopted from `vendors/alioth/alioth/src/hv/hv.rs` (Apache-2.0). Associated types — not `dyn Trait` — for the per-VM type hierarchy. The `dyn`-style escape hatch (`Box<dyn Vcpu>`) only exists where heterogeneous device trees demand it.

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
    fn cancel(&self);                       // hv_vcpus_exit; idempotent
    fn get_reg(&self, reg: Reg) -> Result<u64>;
    fn set_reg(&mut self, reg: Reg, val: u64) -> Result<()>;
    fn get_sys_reg(&self, reg: SysReg) -> Result<u64>;
    fn set_sys_reg(&mut self, reg: SysReg, val: u64) -> Result<()>;
    fn set_pending_irq(&mut self, irq: Irq) -> Result<()>;
}
```

`Reg` and `SysReg` enums cover only the registers we touch — see [13-arch-and-boot.md § 3](./13-arch-and-boot.md#3-sysreg-subset). `VmExit` is defined in [10-data-model.md § 3](./10-data-model.md#3-vmexit--the-vcpu-run-loop-algebra).

```rust
#[non_exhaustive]
pub struct BackendCapabilities {
    pub kind: BackendKind,                  // Hvf | Mock
    pub max_vcpus: u32,
    pub dirty_page_tracking: bool,
    pub postcopy_restore: bool,
    pub exposes_vcpu_exits: bool,
    pub custom_mmio_devices: bool,
}
```

The `BackendKind::Vz` variant is removed at workspace level — see [99-key-decisions.md § D1](./99-key-decisions.md#d1-hvf-only-no-vz). `Mock` exists for unit tests; never selectable from the CLI.

## 3. Lifecycle

A VM transitions through a small internal state machine, owned by the VMM event loop:

```
Uninitialized
   │ PUT /machine-config + /boot-source + /drives + ...   (pre-boot mutations)
   ▼
NotStarted
   │ PUT /actions {InstanceStart}
   ▼
Starting    — vCPUs spawned, GIC live, devices wired, vCPU 0 about to run
   │
   ▼
Running     — at least one vCPU active
   │ PATCH /vm {Pause}                    │ PSCI SYSTEM_OFF / SHUTDOWN
   ▼                                       ▼
Paused                                   Shutdown   — terminal
   │ PATCH /vm {Resume}                    │
   ▼                                       │
Running ────────────────────────────────────┘
```

Pre-boot vs post-boot admissibility per endpoint follows upstream Firecracker — see [21-api-compat-matrix.md § 1](./21-api-compat-matrix.md#1-http-api-endpoints).

### 3.1 Internal `LifecyclePhase` vs wire `VmState`

The internal phase enum is richer than what the wire exposes. Squib distinguishes `Uninitialized` (no config posted) from `NotStarted` (config posted, awaiting `InstanceStart`) from `Starting` (boot orchestration in progress) so handlers can produce precise `fault_message`s on misordered requests. None of those leak to clients:

```rust
pub enum LifecyclePhase {
    Uninitialized,
    NotStarted,
    Starting,
    Running,
    Paused,
    Shutdown,
}

impl LifecyclePhase {
    /// Collapse to the upstream three-value vocabulary served by `GET /`.
    pub fn wire_state(&self) -> VmState {
        match self {
            Self::Uninitialized | Self::NotStarted | Self::Starting | Self::Shutdown => VmState::NotStarted,
            Self::Running => VmState::Running,
            Self::Paused  => VmState::Paused,
        }
    }
}
```

`VmState` is the wire shape pinned in [10-data-model.md § 2.2](./10-data-model.md#22-instanceinfo--get-) and serializes to the literal upstream strings (`"Not started"`, `"Running"`, `"Paused"`). SDKs and `firectl` see only those three values, never `Uninitialized` / `Starting` / `Shutdown`.

## 4. Threading model

| Thread | Lives in | Notes |
|--------|----------|-------|
| API thread | `squib-api`, axum runtime | accept loop on the UDS, request handlers; never touches hypervisor state directly |
| VMM event loop | `squib-vmm`, current-thread tokio | EventManager subscribes to `ApiAction`, device events, timers, metrics flush |
| Per-vCPU thread × N | spawned by `squib-hv::Vm::create_vcpu` | dedicated OS thread (HVF affinity contract); receives `VcpuCommand` on mpsc; runs `Vcpu::run` loop |
| Block I/O pool | `tokio` blocking pool | per-disk worker threads against `F_NOCACHE`-opened file descriptors |
| Mach-exception server | dedicated `std::thread` (`squib-host::pager`) | only when postcopy is active; see [16-snapshots.md § 5](./16-snapshots.md#5-postcopy--lazy-restore) |
| MMDS / dumbo | hosted on the VMM event loop | no dedicated thread |
| gvproxy (when `--network=userspace`) | child process via `tokio::process::Command` | UDS for control plane |

**Hard rules:**

1. `applevisor::Vcpu::*` calls *must* come from the creating thread. The HVF backend enforces this with a thread-local check; misuse panics in debug, returns `Error::Threading` in release. See [12-hvf-backend.md § 4](./12-hvf-backend.md#4-threading-rules).
2. Cancellation is `Vcpu::cancel()` (which wraps `hv_vcpus_exit`) — async, idempotent, callable from any thread. We do **not** use signals.
3. Snapshot save / restore drives every vCPU thread through `VcpuCommand::SaveState` and waits on the per-vCPU `oneshot`; the VMM event loop never reads vCPU registers directly.
4. The `Vcpu` trait's run/get_reg/set_reg/get_sys_reg/set_sys_reg methods take `&mut self`, so the borrow checker prevents concurrent calls from the *same* thread; the thread-local check is the runtime backstop for accidental cross-thread moves. A typestate refinement (`VcpuHandle: Send` → `VcpuOnThread: !Send` after a one-shot `bind()`) is recorded as a follow-up in [99-key-decisions.md § D18](./99-key-decisions.md#d18-vcpu-thread-affinity-runtime-check-now-typestate-later) — runtime check ships in 1.0; typestate lands when the API churn cost is justified.

Per CLAUDE.md § Async & Concurrency: Tokio multi-thread runtime explicitly enabled, message-passing over shared state, every spawned task awaited or explicitly detached with justification.

## 5. Panic policy

Per CLAUDE.md § Safety & Security:

- `#![forbid(unsafe_code)]` at the crate root of every crate except `squib-hv` and `squib-net::sys`.
- No `unwrap()`, `expect()`, `[]` indexing, `unreachable!()`, `todo!()`, `panic!()` reachable from API input. Boundary modules (`squib-api`, the VMM event loop's `ApiAction` dispatch) lint with `clippy::unwrap_used`, `clippy::expect_used`, `clippy::indexing_slicing`, `clippy::panic` denied.
- Library crates use `thiserror`-derived enum errors with `#[source]`. The CLI (`apps/squib`) uses `anyhow` only at `main.rs`.
- A vCPU thread panic is **fatal to the VM** but not to the process: the VMM event loop catches the `JoinError`, transitions the VM to `Shutdown`, and returns a 500 to any pending API call. Other VMs in the process (if any — see [00-prd.md § 4](./00-prd.md#4-non-goals); we ship single-VM per process for 1.0) are unaffected.

## 6. Error types

```rust
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("hypervisor backend error")]
    Backend(#[from] BackendError),

    #[error("guest exit could not be handled: {0}")]
    Exit(String),

    #[error("threading rule violated: {0}")]
    Threading(String),

    #[error("device error")]
    Device(#[from] DeviceError),

    #[error("snapshot error")]
    Snapshot(#[from] SnapshotError),

    #[error("validation error")]
    Validation(#[from] validator::ValidationErrors),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = core::result::Result<T, Error>;
```

`Option<T>` is **not** used to represent errors anywhere in the trait surface. `Option` only appears for genuinely-optional configuration (e.g. `initrd_path`).

## 7. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-RC-1 | Every `unsafe` block lives in `squib-hv` or `squib-net::sys`, with a `// SAFETY:` comment. | `#![forbid(unsafe_code)]` in every other crate; CI grep for raw `unsafe` |
| I-RC-2 | A `Vcpu::run` call only ever happens on the thread that produced the `Vcpu`. | `squib-hv` thread-local check + unit test asserting `Error::Threading` from a foreign thread |
| I-RC-3 | `Vcpu::cancel` is callable from any thread and idempotent. | `applevisor::Vcpu::exit` semantics; unit test calling `cancel()` twice |
| I-RC-4 | The VMM event loop is the only thread that mutates device state. | All device handles owned by VMM; cross-thread access only via `ApiAction` channel |
| I-RC-5 | Pre-boot and post-boot endpoints reject requests in the wrong state with the upstream `fault_message`. | `RuntimeApiController` state-machine table; per-endpoint test in compat suite |
| I-RC-6 | `BackendCapabilities` is queried once at VMM construction; runtime configuration that contradicts it is rejected at config-load. | Single call site; `RuntimeApiController` consults the cached struct |

## 8. Cross-references

- ← Depends on: [00-prd.md](./00-prd.md), [10-data-model.md](./10-data-model.md)
- → Consumed by: [12-hvf-backend.md](./12-hvf-backend.md), [13-arch-and-boot.md](./13-arch-and-boot.md), [14-virtio-and-devices.md](./14-virtio-and-devices.md), [16-snapshots.md](./16-snapshots.md), [20-firecracker-api.md](./20-firecracker-api.md)
- ↔ Related research: [docs/research/hvf-prior-art-deep-dive.md](../docs/research/hvf-prior-art-deep-dive.md) (alioth trait shape), [docs/research/firecracker-architecture.md](../docs/research/firecracker-architecture.md) (event-loop discipline)
