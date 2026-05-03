---
title: 10-data-model — wire shapes & in-memory contracts
type: design
status: draft
last_updated: 2026-05-03
depends_on: 00-prd.md
---

# 10 · Data Model — wire shapes & in-memory contracts

Status: draft · Owner: squib-core · Depends on: [00-prd.md](./00-prd.md)

## 1. Purpose

Pin every shape that crosses a trust boundary, a thread boundary, or a process boundary. Once these shapes are fixed, the rest of the spec set can refer to them without re-deriving. Drift here cascades through API server, snapshots, IPC, and tests.

This file is the **source of truth** for:

- HTTP request / response JSON envelopes (the `FaultMessage` shape, the OpenAPI structs).
- The `VmExit` algebraic type the vCPU run loop produces.
- The `MicrovmState` shape persisted to a snapshot state file.
- The snapshot file outer container, both state and memory.
- Cross-thread `ApiAction` / `ApiResponse` enums between the API server and the VMM event loop.

This file does **not** define behaviour for these shapes — that is in the consuming component spec.

## 2. HTTP wire envelope

### 2.1 Error body — every 4xx response

```rust
#[derive(Serialize, Deserialize, Debug)]
pub struct FaultMessage {
    pub fault_message: String,
}
```

Per upstream Firecracker, snake_case field name. No additional fields, ever. Header `Server: Firecracker API` on every response.

### 2.2 InstanceInfo — `GET /`

```rust
#[derive(Serialize, Deserialize, Debug)]
pub struct InstanceInfo {
    pub id: String,                    // user-supplied --id, default "anonymous-instance"
    pub state: InstanceState,          // Uninitialized | Starting | Running | Paused | NotStarted
    pub vmm_version: String,           // "1.16-firecracker-compat (squib X.Y.Z)"
    pub app_name: String,              // "Firecracker" — sniffed by SDKs
}
```

### 2.3 Schema layer

All API request / response structs are squib-defined Rust types with `#[serde(rename_all = "snake_case")]` for nested JSON (matching upstream) and `#[serde(rename_all = "kebab-case")]` only at the static-config-file top level. Field-level `#[serde(rename = "...")]` is reserved for the handful of upstream non-conformities (none currently known; reserve the lever).

Validation runs at `serde` deserialization time via `validator`-derived rules:
- length caps on every string (default 256 bytes; raise deliberately per field).
- range caps on every numeric (`vcpu_count: 1..=host_max`, `mem_size_mib: 1..=host_ram_minus_overhead`, etc.).
- regex allowlists on identifiers (`drive_id`, `iface_id`, `id`): `^[A-Za-z0-9_]{1,64}$`.

`#[serde(deny_unknown_fields)]` everywhere except the top-level static-config envelope (which carries the `"squib": {...}` extension and must tolerate unknown keys for forward-compat).

Per-endpoint field rules live in [21-api-compat-matrix.md](./21-api-compat-matrix.md).

## 3. `VmExit` — the vCPU run-loop algebra

```rust
pub enum VmExit {
    Mmio { addr: u64, write: bool, data: SmallVec<[u8; 8]> },
    Hvc { imm16: u16, x: [u64; 4] },             // dispatched as PSCI
    Smc { imm16: u16, x: [u64; 4] },
    SystemRegister {
        read: bool,
        op0: u8, op1: u8, crn: u8, crm: u8, op2: u8,
        xt: u8,
    },
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

Notes:
- aarch64-shaped union of HVF's `hv_vcpu_exit_t` decoded variants and the libkrun `VcpuExit` enum we are porting. Explicitly no `Pio`, no `Hlt`, no x86 variants.
- `Mmio.data` uses `SmallVec<[u8; 8]>` because every realistic MMIO payload is ≤ 8 bytes.
- `Cancelled` is an out-of-band exit produced by `applevisor::Vcpu::exit()` (i.e. `hv_vcpus_exit`); a clean signal to drain.
- `InternalError(String)` is the only non-bounded-string variant in the type. The string is for an `error!` log line and an `(InternalError, fault_message)` API surfacing — not for retry logic.

## 4. Cross-thread control plane

### 4.1 API → VMM

```rust
pub enum ApiAction {
    PutBootSource(BootSourceConfig),
    PutDrive(DriveConfig),
    PatchDrive(DrivePatch),
    DeleteDrive { drive_id: String },
    PutNetwork(NetworkInterfaceConfig),
    PatchNetwork(NetworkPatch),
    DeleteNetwork { iface_id: String },
    PutVsock(VsockConfig),
    PutMmds(serde_json::Value),
    PutMmdsConfig(MmdsConfig),
    PutBalloon(BalloonConfig),
    PatchBalloon(BalloonUpdate),
    PatchBalloonStats(BalloonStatsUpdate),
    PutEntropy(EntropyConfig),
    PutSerial(SerialConfig),
    PutPmem(PmemConfig),
    PatchHotplugMemory(HotplugMemoryUpdate),
    PutCpuConfig(CpuConfig),
    PutMachineConfig(MachineConfig),
    PatchMachineConfig(MachineConfigPatch),
    PutLogger(LoggerConfig),
    PutMetrics(MetricsConfig),
    Action(InstanceAction),                    // InstanceStart | FlushMetrics | (R: SendCtrlAltDel)
    PatchVm(VmStateChange),                    // Pause | Resume
    SnapshotCreate(SnapshotCreateConfig),
    SnapshotLoad(SnapshotLoadConfig),
    Shutdown,                                  // SIGINT path
}

pub enum ApiResponse {
    NoContent,
    Json(serde_json::Value),
    Fault { status: u16, fault_message: String },
}
```

Channel: `tokio::sync::mpsc::Sender<(ApiAction, oneshot::Sender<ApiResponse>)>` from `RuntimeApiController` to the VMM event loop. The API thread never touches device or hypervisor state directly. See [20-firecracker-api.md § 4](./20-firecracker-api.md#4-state-machine).

### 4.2 VMM → vCPU thread

Per-vCPU command channel:

```rust
pub enum VcpuCommand {
    Start { entry_pc: u64, x0: u64, pstate: u64 },
    Pause,
    Resume,
    SaveState(oneshot::Sender<VcpuState>),
    InjectIrq(Irq),
    Shutdown,
}
```

Plus a one-way exit cancel via `applevisor::Vcpu::exit()` (idempotent, callable from any thread). See [11-runtime-core.md § 4](./11-runtime-core.md#4-threading-model).

## 5. `MicrovmState` — the snapshot state blob

```rust
#[derive(Serialize, Deserialize, Debug)]
pub struct MicrovmState {
    pub vm_info: VmInfo,                 // mem_size_mib, smt (always false), cpu_template, boot_source
    pub vcpu_states: Vec<VcpuState>,     // one per vCPU
    pub device_states: DeviceStates,     // per-device config + virtqueue cursors
    pub gic_state: GicState,             // opaque blob from hv_gic_state_get_data
    pub mmds_state: Option<MmdsState>,   // serde_json::Value tree if MMDS enabled
}

#[derive(Serialize, Deserialize, Debug)]
pub struct VcpuState {
    pub mpidr: u64,
    pub regs: GpRegs,                    // X0..X30, SP, PC, PSTATE
    pub fp_regs: FpSimdRegs,             // V0..V31, FPSR, FPCR
    pub sys_regs: BTreeMap<SysReg, u64>, // curated subset (~100 regs we touch)
    pub psci_state: PsciVcpuState,       // On | Off | OnPending
}
```

Encoded with `bitcode` (matches upstream Firecracker post-1.10 — see [99-key-decisions.md § D5](./99-key-decisions.md#d5-snapshot-encoding-bitcode-not-versionize)).

`SysReg` is an enum covering exactly the registers we touch — not the full ARMv8 set. Adding a register is a one-line enum extension plus a mapping in `squib-hv`. The full curated list lives in [13-arch-and-boot.md § 3](./13-arch-and-boot.md#3-sysreg-subset).

## 6. Snapshot file format

### 6.1 State file (`<id>.snap`)

```
| magic_id        u64                       0x07101984_AAAA_0000 (aarch64)
| version         length-prefixed UTF-8     "1.0.0" (squib snapshot format version)
| state           length-prefixed bitcode   MicrovmState
| crc64           u64                        ISO 3309 CRC-64 over magic..state
```

All fields little-endian. Length prefixes are unsigned varint (`bitcode` defaults).

### 6.2 Memory file (`<id>.mem`)

Binary dump of guest RAM, page-aligned. Two flavours:

- **Full** — every byte of the `[ram_start, ram_end)` range, dense.
- **Sparse-of-dirty** — same logical layout, holes via filesystem `SEEK_HOLE` for unmodified pages. Diff snapshots produce sparse files.

The on-disk layout matches Firecracker's so a Linux-side `firecracker --describe-snapshot` against a squib-produced file shows a familiar shape (same offsets, same totals).

## 7. Cross-references

- ← Depends on: [00-prd.md](./00-prd.md) (compatibility scope, naming conventions)
- → Consumed by: [11-runtime-core.md](./11-runtime-core.md), [16-snapshots.md](./16-snapshots.md), [20-firecracker-api.md](./20-firecracker-api.md), [21-api-compat-matrix.md](./21-api-compat-matrix.md)
- ↔ Related research: [docs/research/firecracker-api-surface.md](../docs/research/firecracker-api-surface.md), [docs/research/hvf-performance-and-snapshots.md](../docs/research/hvf-performance-and-snapshots.md)
