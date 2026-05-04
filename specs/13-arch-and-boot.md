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

From [docs/research/aarch64-hvf-guest-stack.md § 1.3, § 2.2, § 11](../docs/research/aarch64-hvf-guest-stack.md). Pinned constants in `squib-arch::layout`:

```text
0x0000_0000 .. 0x07FF_FFFF  (128 MiB)  reserved low MMIO (firmware sandbox; unused)
0x0800_0000 .. 0x0800_FFFF  ( 64 KiB)  GICD       (size queried via hv_gic_get_distributor_size)
0x0801_0000 .. 0x0809_FFFF  (576 KiB)  reserved (legacy / future system devices; PL031 RTC may live here)
0x080A_0000 .. 0x0E09_FFFF  ( 96 MiB)  GICR window (max 32 vCPUs × 128 KiB × 24 = headroom; live size = hv_gic_get_redistributor_region_size)
0x0E0A_0000 .. 0x0E0A_0FFF  (  4 KiB)  PL011 UART (FDT SPI cell 1 → INTID 33, level-high)
0x0F00_0000 .. 0x0F01_FFFF  (128 KiB)  virtio-MMIO (32 × 4 KiB; FDT SPI cells 16..47 → INTIDs 48..79, edge-rising)
0x1000_0000 .. 0x7FFF_FFFF  (~1.75 GiB) reserved hole between MMIO and DRAM
0x8000_0000                            DRAM start (matches Firecracker DRAM_MEM_START)
  +0x0020_0000                         kernel Image load (2 MiB-aligned; Firecracker reserves first 2 MiB for system metadata)
  +0x1000_0000                         initrd (start at DRAM+256 MiB, or kernel_end + 16 MiB rounded up to 2 MiB-aligned, whichever is larger)
  ram_end - 0x0020_0000                FDT (last 2 MiB of RAM; 8-byte aligned, ≤ 2 MiB per arm64 booting.rst)
  ram_end                              RAM end
```

Bounds and rationale:

- **DRAM**: `0x8000_0000 ≤ ram_end < 0x00FF_8000_0000` (max 1022 GiB; matches upstream `DRAM_MEM_MAX_SIZE`). DRAM base matches Firecracker for API parity.
- **GICR sizing was a real bug in earlier drafts.** Per `hv_gic_get_redistributor_size = 128 KiB` and `MAX_SUPPORTED_VCPUS = 32` (D19), worst-case GICR is `32 × 128 KiB = 4 MiB`. The earlier 13 § 2 layout placed virtio-MMIO at `0x0A00_0000` and GICR ended at `0x0E0A_0000` — virtio-MMIO and GICR overlapped for any vCPU count > 12. The fix: the GICR window is reserved as a fixed `[0x080A_0000, 0x0E0A_0000)` band sized for the 32-vCPU worst case; virtio-MMIO and PL011 sit *above* that band. The actual GICR live size is `vcpu_count × hv_gic_get_redistributor_size` and is reported in the FDT `intc.reg` cell. Recorded as [99-key-decisions.md § D22](./99-key-decisions.md#d22-fixed-mmio-layout-sized-for-32-vcpu-worst-case-gicr).
- **MMIO base diverges from Firecracker.** Firecracker on aarch64 places MMIO at `0x4000_0000` with the GIC just below DRAM; squib follows the QEMU virt / libkrun convention (low MMIO, high DRAM) because it matches `applevisor` examples and the bundled reference kernel. The standard FDT-driven discovery path used by mainline Linux works regardless. A Firecracker-tuned kernel that *hard-codes* MMIO addresses (rather than reading the FDT) will not boot on squib unmodified; documented in `docs/api-deviations.md` and surfaced as a startup warning if the configured kernel image's `bootargs` contains `earlycon=pl011,0x9000000` (the Firecracker default).
- **Initrd placement.** `arm64/booting.rst` requires the initrd live within a 1 GiB-aligned ≤ 32 GiB window covering the kernel. The "DRAM+256 MiB or kernel_end + 16 MiB" heuristic guarantees that for any kernel ≤ 256 MiB, both regions fit comfortably in a 1 GiB window starting at DRAM base.

### 2.1 GIC interrupt-ID conventions

The FDT GICv3 interrupt cell triple is `<type intid_offset flags>`:

| Field | Meaning |
|-------|---------|
| `type` | `0` = SPI, `1` = PPI |
| `intid_offset` | `INTID − 32` for SPI; `INTID − 16` for PPI |
| `flags` | `1` = edge-rising, `4` = level-high, `8` = level-low |

So **"FDT SPI cell N" means raw INTID `32+N`**. Pinned mappings:

| Hardware | FDT cell | Raw INTID | Trigger | Source |
|----------|----------|-----------|---------|--------|
| Virtual timer (CNTV) | PPI cell 11 | `27` | level-high | ARM ARM, kernel `arm,armv8-timer` |
| Hypervisor timer (CNTHP) | PPI cell 10 | `26` | level-high | same |
| Physical timer EL1 (CNTP) | PPI cell 14 | `30` | level-high | same |
| PL011 UART | SPI cell 1 | `33` | level-high | QEMU virt convention |
| virtio-MMIO slot 0 | SPI cell 16 | `48` | edge-rising | QEMU virt convention; leaves cells 0..15 for legacy/system devices |
| virtio-MMIO slot N (N=0..31) | SPI cell `16+N` | `48+N` | edge-rising | same |

SPI cells `0..15` (INTIDs 32..47) are reserved for legacy/system devices (PL031 RTC, PL011 UART, future PCIe MSI line — though squib uses virtio-MMIO and does not configure PCIe MSI in 1.0). The "virtio SPIs 16..47" phrasing in earlier drafts was the *FDT cell-value* range; the equivalent raw INTIDs are 48..79. This spec uses **raw INTID** by default and notes the FDT cell value when emitting tree text.

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
    DataAbort { is_write: bool, sas: u8, srt: u8, sf: bool },
    Hvc { imm16: u16 },
    Smc { imm16: u16 },
    SystemRegister { read: bool, op0: u8, op1: u8, crn: u8, crm: u8, op2: u8, xt: u8 },
    Wfi,
    Wfe,
    Brk { imm16: u16 },
    Other { ec: u8, raw: u64 },
}
```

Property-tested against random `u64` inputs; never panics. Sourced from the Arm ARM, sections D17.2 and D24. `FAR_EL2` is *not* part of `EsrDecoded` — `decode(esr)` cannot derive it; the run-loop reads `FAR_EL2` separately and threads it onto the data-abort `Exit::Mmio { addr, … }` envelope downstream.

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

`squib-fdt::build(...)` produces a flat device tree using `vm-fdt = "0.3"`:

```text
/                          (model = "squib,microvm", compatible = "linux,squib-microvm,linux,dummy-virt"
                            #address-cells = 2, #size-cells = 2, interrupt-parent = <&intc>)
├── chosen
│   ├── bootargs           = effective_boot_args (see § 6.1)
│   ├── stdout-path        = "/pl011@e0a0000"
│   ├── linux,initrd-start = <0x0 initrd_start>
│   └── linux,initrd-end   = <0x0 initrd_end>
├── memory@80000000        (device_type = "memory", reg = <0x0 ram_start 0x0 ram_size>)
├── cpus
│   ├── #address-cells = 1
│   ├── #size-cells = 0
│   └── cpu@<mpidr>×N      (device_type="cpu", compatible = "arm,armv8", enable-method = "psci",
│                             reg = <MPIDR_AFF1<<8 | MPIDR_AFF0>)
├── psci                   (compatible = "arm,psci-1.0,arm,psci-0.2,arm,psci",
│                            method = "hvc",
│                            cpu_on = 0xC4000003, cpu_off = 0x84000002,
│                            cpu_suspend = 0xC4000001, migrate = 0xC4000005)
├── timer                  (compatible = "arm,armv8-timer", always-on,
│                            interrupts = <1 13 0xf08    /* secure   PPI 13 = INTID 29 */
│                                          1 14 0xf08    /* non-sec  PPI 14 = INTID 30 */
│                                          1 11 0xf08    /* virtual  PPI 11 = INTID 27 */
│                                          1 10 0xf08>)  /* hyp      PPI 10 = INTID 26 */
├── intc@8000000           (compatible = "arm,gic-v3",
│                            #interrupt-cells = 3,
│                            interrupt-controller,
│                            #redistributor-regions = 1,
│                            redistributor-stride = <0x0 0x20000>,
│                            reg = <0x0 0x08000000 0x0 hv_gic_get_distributor_size,
│                                   0x0 0x080A0000 0x0 (hv_gic_get_redistributor_size * vcpu_count)>)
├── pl011@e0a0000          (compatible = "arm,pl011,arm,primecell",
│                            reg = <0x0 0x0E0A0000 0x0 0x1000>,
│                            interrupts = <0 1 4>          /* SPI cell 1 = INTID 33, level-high */,
│                            clocks = <&apb_clk>, clock-names = "apb_pclk")
├── virtio_mmio@f000000    (one per slot at 0x0F000000 + slot*0x1000;
│                            interrupts = <0 (16+slot) 1>  /* SPI cell 16+slot = INTID 48+slot, edge-rising */)
└── apb_clk                (compatible = "fixed-clock", #clock-cells = 0,
                            clock-frequency = 24000000, clock-output-names = "clk24mhz")
```

The builder is parameterized by `vcpu_count`, `mem_size`, `mmio_devices`, `kernel_load_addr`, `initrd_range`, `boot_args`; the resulting blob is written 8-byte-aligned into the last 2 MiB of guest RAM and its address is the value placed in vCPU 0's X0 register at boot.

### 6.1 Boot-args composition

`effective_boot_args` is computed at FDT build time, not at API-load time, so squib can inject identifiers it controls without serialising them into the user's `/boot-source` payload:

1. Start with the user's `/boot-source.boot_args` (may be empty).
2. **Append** `console=ttyAMA0` if the user has not specified `console=`.
3. **Append** `panic=1` if not present (matches Firecracker's hard-coded boot-args policy).
4. If `is_root_device` is set on a drive and that drive carries a `partuuid`, append `root=PARTUUID=<uuid>`; otherwise leave root selection to the user.

The 21 § 2 row "passed verbatim; no defaults injected unless absent" is interpreted by this rule. Recorded as [99-key-decisions.md § D23](./99-key-decisions.md#d23-boot-args-composition-rule).

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
| I-AB-6 | The GICR live region `[0x080A_0000, 0x080A_0000 + vcpu_count × hv_gic_get_redistributor_size)` does not overlap PL011 (`0x0E0A_0000`) or virtio-MMIO (`0x0F00_0000`) for any `vcpu_count ∈ 1..=32`. | Compile-time `const` assertion in `squib-arch::layout` plus runtime check in `squib-vmm::builder` |
| I-AB-7 | Every interrupt cell emitted into the FDT decodes back to the same `(intid, trigger)` pair via the inverse of the table in § 2.1. | Property test in `squib-fdt` |

## 11. Cross-references

- ← Depends on: [11-runtime-core.md](./11-runtime-core.md), [12-hvf-backend.md](./12-hvf-backend.md)
- → Consumed by: [14-virtio-and-devices.md](./14-virtio-and-devices.md) (MMIO slot map), [16-snapshots.md](./16-snapshots.md) (sysreg list, GIC state), [20-firecracker-api.md](./20-firecracker-api.md) (boot-source validation)
- ↔ Related research: [docs/research/aarch64-hvf-guest-stack.md](../docs/research/aarch64-hvf-guest-stack.md) (every section)
