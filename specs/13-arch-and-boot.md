---
title: 13-arch-and-boot — aarch64 layout, sysregs, PSCI, FDT, kernel loader
type: design
status: draft
last_updated: 2026-05-03
depends_on: 11-runtime-core.md, 12-hvf-backend.md
---

# 13 · Arch & Boot — aarch64 layout, sysregs, PSCI, FDT, kernel loader

Status: draft · Owner: squib-arch + squib-fdt + squib-loader · Depends on: [11-runtime-core.md](./11-runtime-core.md), [12-hvf-backend.md](./12-hvf-backend.md)

## 1. Purpose

Pin every architecture-specific constant and protocol the boot path depends on. This file is consumed by the HVF backend (sysreg list, ESR decoding), the FDT builder, the kernel loader, and the VMM boot orchestrator. If a constant is in this file, no other file invents it.

Three crates cooperate here, sharing this spec:

- `squib-arch` — memory layout constants, sysreg enum, PSCI dispatch table, ESR_EL2 decoder, vCPU initial-register helpers.
- `squib-fdt` — FDT builder via `vm-fdt`.
- `squib-loader` — kernel image loader: PE, raw `Image`, `Image.gz`, `Image.zst`.

## 2. Memory layout (concrete)

From [docs/research/aarch64-hvf-guest-stack.md § 11](../docs/research/aarch64-hvf-guest-stack.md). Pinned constants in `squib-arch::layout`:

```text
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

Bounds: `0x8000_0000 ≤ ram_end < 0x00FF_8000_0000` (max 1022 GiB; matches upstream `DRAM_MEM_MAX_SIZE`). DRAM base matches Firecracker's aarch64 layout for API parity. MMIO base matches QEMU virt / libkrun for kernel-config familiarity, **not** Firecracker's MMIO base — the consequence is that a Firecracker-tuned kernel that hard-codes MMIO addresses (rather than reading them from FDT) may not boot on squib. The standard FDT-driven discovery path used by mainline Linux kernels works regardless. Documented as a row in [21-api-compat-matrix.md § 7](./21-api-compat-matrix.md#7-snapshot-file-format) and surfaced with a one-line warning in `docs/api-deviations.md`.

## 3. Sysreg subset

The full ARMv8 sysreg space is enormous; squib touches only the registers required for boot, exception handling, snapshot save/restore, and the `V1N1` CPU template. The enum:

```rust
#[non_exhaustive]
pub enum SysReg {
    // Boot setup
    SctlrEl1, TtbrEl1, MairEl1, TcrEl1, SpEl1, ElrEl1, SpsrEl1,
    // ID registers (read-only; covered for consistency)
    IdAa64Mmfr0El1, IdAa64Mmfr1El1, IdAa64Pfr0El1, IdAa64Pfr1El1, IdAa64Dfr0El1,
    IdAa64Isar0El1, IdAa64Isar1El1, MpidrEl1,
    // Timer (vtimer)
    CntvCtlEl0, CntvCvalEl0, CntvOffEl2, CntFrqEl0,
    // Performance (used by guest, snapshot-relevant)
    PmccntrEl0, PmccfilterEl0, PmuserenrEl0, PmcrEl0,
    // GIC (handled by hv_gic_state_*; listed for completeness in vCPU state, not directly settable)
    // Exception handling
    EsrEl1, FarEl1, VbarEl1,
    // Plus the ~80 additional regs we save/restore — see crates/squib-arch/src/sysregs.rs
}
```

The full curated list (~100 regs) lives in `crates/squib-arch/src/sysregs.rs` and is treated as an additive contract — registers can be added (with a snapshot version bump if the format breaks) but never removed.

The choice not to mirror the full KVM list is deliberate; many KVM-saved registers are EL2/EL3 or x86-only. Recorded as [99-key-decisions.md § D6](./99-key-decisions.md#d6-sysreg-curated-not-full-armv8).

## 4. ESR_EL2 decoder

`squib-arch::esr` exposes a `decode(esr: u64) -> EsrDecoded` returning a structured variant:

```rust
pub enum EsrDecoded {
    DataAbort { is_write: bool, sas: u8, srt: u8, sf: bool, far: u64 },
    Hvc { imm16: u16 },
    Smc { imm16: u16 },
    SystemRegister { read: bool, op0: u8, op1: u8, crn: u8, crm: u8, op2: u8, xt: u8 },
    Wfi,
    Wfe,
    Brk { imm16: u16 },
    Other { ec: u8, raw: u64 },
}
```

Property-tested against random `u64` inputs; never panics. Sourced from the Arm ARM, sections D17.2 and D24.

## 5. PSCI dispatch

Per [docs/research/aarch64-hvf-guest-stack.md § 4](../docs/research/aarch64-hvf-guest-stack.md). Dispatch table in `squib-arch::psci::dispatch`:

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

After dispatch, the VMM advances PC by 4 (HVC does not auto-advance under HVF; see [12-hvf-backend.md § 5](./12-hvf-backend.md#5-vcpu-run-loop)).

## 6. FDT skeleton

`squib-fdt::build(...)` produces a flat device tree using `vm-fdt`:

```text
/                          (model = "squib,virt", compatible = "linux,dummy-virt")
├── chosen
│   ├── bootargs           = boot_args (verbatim, no defaults injected unless absent)
│   ├── linux,initrd-start
│   └── linux,initrd-end
├── memory                 (reg = <ram_start ram_size>)
├── cpus
│   ├── #address-cells = 2
│   ├── #size-cells = 0
│   └── cpu@0..N           (compatible = "arm,armv8", enable-method = "psci", reg = MPIDR)
├── psci                   (compatible = "arm,psci-1.0", method = "hvc", cpu_on, cpu_off, ...)
├── timer                  (compatible = "arm,armv8-timer", interrupts = <... vtimer-IRQ>)
├── intc                   (compatible = "arm,gic-v3", reg = <GICD GICR>, #interrupt-cells = 3)
├── pl011@9000000          (compatible = "arm,pl011", interrupts = <SPI 1 LEVEL>)
├── virtio_mmio@a000000+   (one node per slot)
└── clocks                 (apb-pclk, fixed at 24 MHz)
```

The builder is parameterized by `vcpu_count`, `mem_size`, `mmio_devices`, and `boot_args`; the FDT is written into the last 2 MiB of guest RAM at boot.

## 7. Kernel loader

`squib-loader` reads the kernel image, detects compression by magic bytes, decompresses if needed, and loads the result into guest memory:

| Format | Magic | Loader |
|--------|-------|--------|
| PE (Linux EFI) | `MZ` | `linux-loader::pe::PE::load` |
| Raw aarch64 `Image` | offset 0x38 = `ARM\x64` | parse aarch64 boot header, resolve text_offset, load at `0x8000_0000 + text_offset` |
| `Image.gz` | `1F 8B` | `flate2` decompress, then raw |
| `Image.zst` | `28 B5 2F FD` | `zstd` decompress, then raw |

Decompression happens at config-load time, not at boot, so the API server returns an error before `InstanceStart` if the kernel is malformed.

## 8. vCPU initial registers

`squib-arch::regs::set_boot_regs(vcpu, kernel_load_addr, fdt_addr)`:

```text
PC      = kernel_load_addr
X0      = fdt_addr
X1..X3  = 0
PSTATE  = 0x3C5    // EL1h, DAIF masked, M[3:0] = EL1h, F=I=A=D=1
```

For PSCI `CPU_ON` of a secondary vCPU, the dispatch sets the same register triple at the address provided in the `entry_point_address` argument.

## 9. Boot orchestration

`squib-vmm::builder::build_microvm_for_boot(VmResources)` is the single boot entry point:

```text
1. Resolve vcpu_count, mem_size_mib, kernel_path, initrd_path, boot_args.
2. Open kernel; auto-detect compression; decompress as needed.
3. Parse boot header; resolve text_offset; compute kernel_load_addr.
4. mmap guest memory (anonymous, RWX); register with HVF via Vm::map_memory.
5. Load kernel via squib-loader.
6. If initrd: write initrd at 0x9000_0000 (or kernel_end + 256 MiB, whichever larger).
7. Build FDT in last 2 MiB of RAM (squib-fdt).
8. Vm::create_gic(vcpu_count); place GICD/GICR at fixed addresses.
9. Create vCPUs; each spawns its OS thread, parks at CPU_OFF except vCPU 0.
10. On vCPU 0: set_boot_regs(kernel_load_addr, fdt_addr).
11. Return (vcpus, devices, mmds, gic, snapshot_handle).
```

`PUT /actions {InstanceStart}` flips vCPU 0 to `Running`. Other vCPUs come up via PSCI `CPU_ON`.

## 10. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-AB-1 | The memory layout constants in `squib-arch::layout` match the FDT node addresses byte-for-byte. | Unit test cross-checking constants vs FDT-emitted strings |
| I-AB-2 | The PSCI dispatch table returns NOT_SUPPORTED for unknown function IDs (never panics, never UB). | `rstest` over the documented function-ID space |
| I-AB-3 | The kernel loader auto-detects compression via magic bytes and rejects unknown magics with `Error::Config`. | Integration test with a malformed `kernel_image_path` |
| I-AB-4 | `set_boot_regs` writes exactly four registers (PC, X0, X1, PSTATE — X2/X3 are zeroed by HVF reset). | Snapshot test against a known-good guest |
| I-AB-5 | The FDT fits in 2 MiB for any supported `vcpu_count` and `mmio_devices.len()`. | Property test up to `vcpu_count = 32` (upstream `MAX_SUPPORTED_VCPUS`), `mmio_devices = 32` |

## 11. Cross-references

- ← Depends on: [11-runtime-core.md](./11-runtime-core.md), [12-hvf-backend.md](./12-hvf-backend.md)
- → Consumed by: [14-virtio-and-devices.md](./14-virtio-and-devices.md) (MMIO slot map), [16-snapshots.md](./16-snapshots.md) (sysreg list, GIC state), [20-firecracker-api.md](./20-firecracker-api.md) (boot-source validation)
- ↔ Related research: [docs/research/aarch64-hvf-guest-stack.md](../docs/research/aarch64-hvf-guest-stack.md) (every section)
