---
title: aarch64-on-HVF Guest Construction Stack
status: research
audience: engineers building squib's HVF backend, FDT/PSCI/GIC/boot pipeline
last_reviewed: 2026-05-03
---

# aarch64-on-HVF Guest Construction Stack — Implementation Research

Target: squib (Apple Silicon, HVF, aarch64 Linux guests, Firecracker-API-compatible).

This document drills into the guest-construction layers that sit between Apple's Hypervisor.framework primitives and a booting aarch64 Linux kernel: boot protocol, FDT, GIC, PSCI, timer, MMIO bus, vCPU register seeding, memory layout, ESR_EL2 decoding, and code-signing requirements.

## 1. aarch64 Linux Boot Protocol

Source of truth: `Documentation/arch/arm64/booting.rst` in the upstream kernel tree.

### 1.1 Required register state at kernel entry

Per the kernel doc:

- **PC** = first instruction of the kernel image (which is the start of the 64-byte arm64 header — the `code0` field is a real branch instruction).
- **X0** = "physical address of device tree blob (dtb) in system RAM."
- **X1, X2, X3** = `0` (reserved for future use; quote: *"x1 = 0 (reserved for future use)"*, same for x2/x3).
- **PSTATE.DAIF** = all four interrupt classes masked: *"All forms of interrupts must be masked in PSTATE.DAIF (Debug, SError, IRQ and FIQ)."*
- CPU mode: non-secure, **EL2 (recommended) or EL1**. *"All CPUs must enter the kernel in the same exception level."*  Under HVF, guest code only ever runs at EL1 — HVF itself owns EL2. So squib enters the guest at **EL1h**.
- **MMU off**, **D-cache off** (or at least the loaded-image range cleaned to PoC). I-cache may be on as long as it has no stale entries.
- The image must be placed `text_offset` bytes from a 2 MiB-aligned base anywhere in RAM.
- DTB: 8-byte aligned, max 2 MiB.
- initrd: must lie within a 1 GiB-aligned, ≤32 GiB physical window that fully covers the kernel `Image`.

### 1.2 Kernel image formats

The 64-byte arm64 boot header (struct visible in `arch/arm64/include/asm/image.h`):

```text
u32 code0          // 'MZ' for PE/COFF compatibility, but a real branch
u32 code1          // branch to stext
u64 text_offset    // offset from 2 MiB-aligned base where Image must be placed
u64 image_size     // effective size of the loaded Image
u64 flags          // endian, page-size, phys-placement
u64 res2/3/4       // reserved (zero)
u32 magic          // 0x644d5241 ("ARM\x64", little-endian)
u32 res5           // PE header offset (used in EFI mode)
```

- **Raw `Image`**: 4 KiB page, little-endian. This is what HVF guests boot directly.
- **`Image.gz`**: gzip-compressed; the VMM must decompress before placing in memory. Trivial with `flate2`.
- **PE/COFF**: same `Image` file is a valid PE binary because `code0`'s low half is `'MZ'` and `res5` points at the PE header. Useful for EFI; not relevant for direct kernel boot.

### 1.3 Where the kernel goes

Different VMMs make different choices for the kernel load address:

| VMM | RAM base | Kernel load address | Rationale |
|---|---|---|---|
| Firecracker | `0x8000_0000` (2 GiB) | `DRAM_MEM_START + SYSTEM_MEM_SIZE` (`= 0x8020_0000`) | First 2 MiB of RAM reserved for ACPI device manager state |
| cloud-hypervisor | `0x4000_0000` (1 GiB) | After ACPI (`KERNEL_START = ACPI_START + ACPI_MAX_SIZE`) | ACPI tables placed first |
| libkrun | `0x8000_0000` (kernel) / `0x4000_0000` (EFI) | `0x8000_0000` for direct kernel boot | EFI vs. direct-kernel split |

Squib should adopt **Firecracker's `0x8000_0000` base** because it matches our compatibility goal. Note: Firecracker's MMIO32 region is at `0x4000_0000`, but on aarch64 it puts RAM at `0x8000_0000`, leaving the entire `0x0000_0000–0x7FFF_FFFF` low region for MMIO — this differs from x86 microvm conventions and is what we copy.

### 1.4 Initrd

Per `booting.rst`, the kernel needs only:

- `linux,initrd-start` and `linux,initrd-end` properties on `/chosen` in the FDT (physical addresses).
- The initrd must be entirely within a 1 GiB-aligned ≤32 GiB window covering the kernel.

No special header on the initrd itself. Just `mmap` the file into guest memory, set the chosen properties.

### 1.5 `linux-loader` crate status (May 2026)

- **Latest published**: `linux-loader = "0.13.2"` (2025-11-20).
- Dual-licensed Apache-2.0 / BSD-3-Clause.
- aarch64 support is via the `pe` Cargo feature, exposing `loader::pe::PE::load`, which parses the `Image` header and returns a `KernelLoaderResult { kernel_load, kernel_end, ... }`.
- The crate **only loads the kernel image** — it does not handle FDT, vCPU state, PSCI, or initrd. Those remain VMM responsibilities.
- We should depend on it for the PE loader and write our own thin gzip front-end for `Image.gz` inputs (decompress to a `Cursor<Vec<u8>>`, hand to `PE::load`).

## 2. Flattened Device Tree (FDT) Construction

### 2.1 `vm-fdt` crate status

- **Latest**: `vm-fdt = "0.3.0"` (Nov 2023, no newer release as of May 2026).
- Apache-2.0 / BSD-3-Clause.
- API surface (in `FdtWriter`):
  - Structure: `begin_node(name) -> FdtWriterNode`, `end_node(node)`, `finish() -> Vec<u8>`.
  - Properties: `property_u32`, `property_u64`, `property_string`, `property_string_list`, `property_array_u32`, `property_array_u64`, `property_null`, raw `property(name, &[u8])`.
  - **Missing**: there is no first-class phandle helper or alias support; you call `property_u32("phandle", n)` yourself and manage handles. (See [vm-fdt #22](https://github.com/rust-vmm/vm-fdt/issues/22) — abstractions improvement is open but unmerged.)
- Maturity: low commit volume but stable API; used in production by Firecracker's aarch64 port and crosvm. Adequate for our needs.

### 2.2 Required FDT nodes for an aarch64 microVM

Pseudo-DTS skeleton (squib will generate this programmatically):

```dts
/dts-v1/;

/ {
    compatible = "linux,squib-microvm";
    interrupt-parent = <&gic>;
    #address-cells = <2>;
    #size-cells = <2>;

    chosen {
        bootargs = "console=ttyAMA0 reboot=k panic=1 pci=off ...";
        stdout-path = "/pl011@<uart_base>";
        linux,initrd-start = <0x0 0x90000000>;
        linux,initrd-end   = <0x0 0x91000000>;
        // optional: kaslr-seed, rng-seed
    };

    memory@80000000 {
        device_type = "memory";
        reg = <0x0 0x80000000 0x0 0x40000000>;   /* 1 GiB at 2 GiB */
    };

    cpus {
        #address-cells = <1>;
        #size-cells = <0>;

        cpu@0 { device_type="cpu"; compatible="arm,arm-v8";
                reg=<0x0>; enable-method="psci"; };
        cpu@1 { device_type="cpu"; compatible="arm,arm-v8";
                reg=<0x1>; enable-method="psci"; };
        /* ... per vCPU ... */
    };

    psci {
        compatible = "arm,psci-1.0", "arm,psci-0.2", "arm,psci";
        method = "hvc";
        cpu_suspend = <0xc4000001>;
        cpu_off     = <0x84000002>;
        cpu_on      = <0xc4000003>;
        migrate     = <0xc4000005>;
    };

    timer {
        compatible = "arm,armv8-timer";
        always-on;
        interrupts = <1 13 0xf08   /* secure   PPI 13 */
                      1 14 0xf08   /* non-sec  PPI 14 */
                      1 11 0xf08   /* virtual  PPI 11 */
                      1 10 0xf08>; /* hyp      PPI 10 */
    };

    gic: intc@8000000 {
        compatible = "arm,gic-v3";
        #interrupt-cells = <3>;
        interrupt-controller;
        #redistributor-regions = <1>;
        redistributor-stride = <0x0 0x20000>;
        reg = <0x0 0x08000000 0x0 0x10000>,             /* GICD */
              <0x0 0x080A0000 0x0 (0x20000 * NR_CPUS)>; /* GICR */
        phandle = <1>;
    };

    pl011@9000000 {
        compatible = "arm,pl011", "arm,primecell";
        reg = <0x0 0x09000000 0x0 0x1000>;
        interrupts = <0 1 4>;       /* SPI 1, level-high */
        clocks = <&apb_clk>;
        clock-names = "apb_pclk";
    };

    virtio_mmio@a000000 {
        compatible = "virtio,mmio";
        reg = <0x0 0x0a000000 0x0 0x200>;
        interrupts = <0 16 1>;      /* SPI 16, edge */
        dma-coherent;
    };
    /* ... one per virtio device, ascending addresses ... */

    apb_clk: clock {
        compatible = "fixed-clock";
        #clock-cells = <0>;
        clock-frequency = <24000000>;
        clock-output-names = "clk24mhz";
    };
};
```

Notes:
- Interrupt cells are GICv3 format `<type number flags>`: type 0 = SPI, 1 = PPI; flags: 1 = edge-rising, 4 = level-high, 8 = level-low.
- Timer PPIs follow the kernel's "GIC interrupt number minus 16" convention because the FDT reports them as PPI offsets, while the GIC INTID is `PPI + 16`. Virtual timer is GIC INTID 27 = PPI 11; physical EL1 = GIC INTID 30 = PPI 14.
- DTB must be 8-byte aligned and ≤2 MiB, per `booting.rst`.

### 2.3 What to borrow from where

| Subsystem | Best source | License | Notes |
|---|---|---|---|
| FDT generator structure | Firecracker `src/vmm/src/arch/aarch64/fdt.rs` | Apache-2.0 | Closest match: minimal, microvm-shaped, MMIO-only, no PCI/UEFI. Direct port. |
| FDT (richer reference) | cloud-hypervisor `arch/src/aarch64/fdt.rs` | Apache-2.0 | NUMA, PCI, ACPI extras — useful for studying but more than we need. |
| FDT (alt embedded use) | crosvm `aarch64/src/fdt.rs` | BSD-3-Clause | Different style, useful tiebreaker. |
| Kernel loader (PE) | `linux-loader` 0.13.2 | Apache-2.0 / BSD-3 | Use directly. |
| Memory layout values | Firecracker `layout.rs` | Apache-2.0 | We copy verbatim for API parity. |
| Boot register seeding | Firecracker `vcpu.rs` + `regs.rs` | Apache-2.0 | KVM-shaped — needs HVF translation. |
| PSCI | Linux UAPI `include/uapi/linux/psci.h` | GPL-2.0 (constants only) | Constants are facts of the spec, not copyrightable; reimplement clean. |
| GIC | Apple `hv_gic_*` directly | N/A (Apple SDK) | Skip userspace emulation entirely. |
| ESR_EL2 decode | `aarch64-esr-decoder` 0.2.4 (Google) | Apache-2.0 | Optional dep; we may inline the bits we need. |

## 3. GICv3 on HVF

This is the watershed call. macOS 15 (Sequoia) introduced the `hv_gic_*` family, which is essentially KVM-equivalent in-kernel-GIC support implemented in Apple's hypervisor.

### 3.1 The `hv_gic_*` API surface (macOS 15.0+)

From [Apple Hypervisor "GIC functions"](https://developer.apple.com/documentation/hypervisor/gic-functions) — all 45 entry points. Grouped:

**Lifecycle / configuration:**
- `hv_gic_config_create()` — allocates an `hv_gic_config_t`.
- `hv_gic_config_set_distributor_base(cfg, gpa)` / `hv_gic_config_set_redistributor_base(cfg, gpa)` — set MMIO bases.
- `hv_gic_config_set_msi_region_base(cfg, gpa)` / `hv_gic_config_set_msi_interrupt_range(cfg, base_intid, count)` — ITS / GICv3-MSI region.
- `hv_gic_create(cfg)` — instantiate. After this point you can no longer change layout.
- `hv_gic_reset()` — clear all GIC state.

**Sizing queries (needed before laying out memory):**
- `hv_gic_get_distributor_size`, `hv_gic_get_distributor_base_alignment`
- `hv_gic_get_redistributor_size`, `hv_gic_get_redistributor_region_size`, `hv_gic_get_redistributor_base_alignment`
- `hv_gic_get_msi_region_size`, `hv_gic_get_msi_region_base_alignment`
- `hv_gic_get_spi_interrupt_range()`

**Interrupt injection (host -> guest):**
- `hv_gic_set_spi(intid, level)` — assert/deassert an SPI line.
- `hv_gic_send_msi(addr, data)` — issue an MSI write to the ITS region.
- `hv_gic_get_intid(...)` — translate.

**Snapshot / restore — this is critical:**
- `hv_gic_state_create()` / `hv_gic_state_get_size(state)` / `hv_gic_state_get_data(state, buf, len)` — extract opaque blob.
- `hv_gic_set_state(state, ...)` / `hv_gic_get_state(state, ...)` — round-trip.

**Per-register access (for things state save can't reach, or for debug):**
- `hv_gic_get_distributor_reg` / `hv_gic_set_distributor_reg`
- `hv_gic_get_redistributor_reg` / `hv_gic_set_redistributor_reg`
- `hv_gic_get_icc_reg` / `hv_gic_set_icc_reg` (CPU interface, per-vCPU)
- `hv_gic_get_ich_reg` / `hv_gic_set_ich_reg` (virt control)
- `hv_gic_get_icv_reg` / `hv_gic_set_icv_reg` (virt CPU interface)
- `hv_gic_get_msi_reg` / `hv_gic_set_msi_reg` (ITS regs)

### 3.2 What this means for squib

- **No userspace GIC emulation needed.** All distributor/redistributor MMIO accesses are handled by the hypervisor; ESR_EL2 data aborts on those addresses never escape to the VMM.
- **SPI injection is one call** (`hv_gic_set_spi`). No need for our own pending/active bitmaps, no LR shadow, no priority arbitration code. This is hundreds of LoC saved versus libkrun's pre-15 path.
- **Snapshots are first-class** via `hv_gic_state_*`. The blob is opaque; we serialize alongside vCPU state and guest RAM.
- **What's not exposed**: there is no public SPI level-querying API beyond `set_spi`. If we need to know "is IRQ N pending?" for debug, we have to round-trip through `hv_gic_get_distributor_reg` and decode GICD_ISPENDR ourselves. Acceptable.
- **GIC ITS for MSI**: provided via `hv_gic_set_msi_*` and `hv_gic_send_msi`. Sufficient for virtio-pci (which we are *not* doing initially — Firecracker uses virtio-mmio with SPIs, no MSI).

### 3.3 Existing Rust wrappers

- **`applevisor` crate** v1.0.0 ([docs.rs/applevisor](https://docs.rs/applevisor)) by Impalabs — pure-safe wrapper over Hypervisor.framework. The `gic` module behind the `macos-15-0` Cargo feature wraps all 45 `hv_gic_*` calls.
- **`applevisor-sys`** v1.0.0 — raw FFI; usable if `applevisor`'s safe wrappers don't fit our state shape.
- **`ahv`** — older alternative wrapper, no GIC support yet.

Recommendation: depend on `applevisor` with `features = ["macos-15-0"]`. If we need to deviate (e.g. for our actor-shaped vCPU lifecycle), drop to `applevisor-sys` and bind the calls we touch.

### 3.4 Userspace GIC emulation as fallback

libkrun used to do full distributor/redistributor emulation pre-macOS-15 (`src/devices/src/legacy/hvfgicv3.rs`). On current main it has switched to native `hv_gic_*`. **We deliberately do not implement the userspace path** — see Section 9 for the macOS version recommendation.

## 4. PSCI

### 4.1 Function ID table (squib must handle)

From `include/uapi/linux/psci.h` (Linux mainline) — these are spec constants:

| Function | SMC32 ID | SMC64 ID | Squib must support? |
|---|---|---|---|
| `PSCI_VERSION` | `0x84000000` | — | Yes — return `0x0001_0001` (PSCI 1.1) |
| `CPU_SUSPEND` | `0x84000001` | `0xC4000001` | Optional — can return `NOT_SUPPORTED` |
| `CPU_OFF` | `0x84000002` | — | Yes |
| `CPU_ON` | `0x84000003` | `0xC4000003` | **Yes — required for SMP** |
| `AFFINITY_INFO` | `0x84000004` | `0xC4000004` | Yes |
| `MIGRATE` | `0x84000005` | `0xC4000005` | Return `NOT_SUPPORTED` (single-VMM) |
| `MIGRATE_INFO_TYPE` | `0x84000006` | — | Return `2` (TOS not present) |
| `MIGRATE_INFO_UP_CPU` | `0x84000007` | `0xC4000007` | Stub |
| `SYSTEM_OFF` | `0x84000008` | — | **Yes** — plumb to control plane |
| `SYSTEM_RESET` | `0x84000009` | — | **Yes** — plumb to control plane |
| `PSCI_FEATURES` | `0x8400000A` | — | Yes — query mechanism |
| `SYSTEM_RESET2` | `0x84000012` | `0xC4000012` | Optional |

Return values: `0` = SUCCESS, `-1` = NOT_SUPPORTED, `-2` = INVALID_PARAMETERS, `-3` = DENIED, `-4` = ALREADY_ON, `-5` = ON_PENDING, `-7` = NOT_PRESENT, `-8` = DISABLED, `-9` = INVALID_ADDRESS.

### 4.2 Dispatch path on HVF

When the guest issues `HVC #0` (PSCI conduit chosen via FDT `method = "hvc"`):

1. `hv_vcpu_run` returns with `exit.reason == HV_EXIT_REASON_EXCEPTION`.
2. Read `ESR_EL2` syndrome from `exit.exception.syndrome`.
3. Decode EC: `EC == 0x16` (HVC AArch64).
4. The ISS imm16 field (bits [15:0]) holds the immediate operand of `HVC` — for PSCI it's `0`.
5. Read `X0` for the function ID. Read `X1`–`X3` for arguments.
6. Dispatch in a `match` on the function ID; write result back to `X0`; advance `PC` by 4 (HVC does *not* auto-advance under HVF — verify via test).
7. Resume.

`SYSTEM_RESET` and `SYSTEM_OFF` flow into squib's VM control plane: post a message to the VMM actor, return `SUCCESS` to the guest, then unblock the run loop and tear the VM down (or recreate for reset).

### 4.3 CPU_ON

`CPU_ON(target_cpu, entry_point, context_id)`:
1. Find the target vCPU actor. If already on, return `ALREADY_ON`.
2. Set its **PC** to `entry_point`, **X0** to `context_id`, all other GPRs to 0.
3. Set PSTATE to `PSR_MODE_EL1h | DAIF_ALL_MASKED` (`0x3c5`).
4. Reset `SCTLR_EL1` (caches/MMU off — Linux secondary CPU also expects this).
5. Send a wake signal to the vCPU actor's run loop.

### 4.4 Where to crib from

- Firecracker doesn't have a userspace PSCI handler — KVM does it. So Firecracker won't help here.
- cloud-hypervisor: same.
- libkrun **on HVF**: `src/cpu/src/aarch64/psci.rs` does have a userspace handler. Apache-2.0, borrowable. Worth reading line-by-line.
- QEMU `target/arm/psci.c` is the canonical reference — GPL-2.0, so read-only (no copy-paste).

## 5. ARM Architectural Timer & Virtio-MMIO Bus

### 5.1 Timer

ARM v8 generic timer interrupts (GIC INTIDs are PPI-base + 16):

| Timer | GIC INTID | FDT PPI |
|---|---|---|
| Secure physical (CNTPS) | 29 | 13 |
| Non-secure EL1 physical (CNTP) | 30 | 14 |
| Virtual (CNTV) | 27 | 11 |
| Hypervisor physical (CNTHP) | 26 | 10 |

For an EL1 Linux guest under HVF, the kernel uses the **virtual timer** (CNTV / PPI 11 / INTID 27) by default — HVF sets `CNTVOFF_EL2` to scale the guest's view of time.

HVF exits with `HV_EXIT_REASON_VTIMER_ACTIVATED` when the guest's `CNTV_CTL_EL0.IMASKED=0` and the virtual timer fires. The VMM is then expected to inject the timer interrupt back into the GIC (via `hv_gic_set_spi`? No — for PPIs, the GIC tracks them per-redistributor; the timer activation is signaled and the vCPU re-enters with the PPI pending in its redistributor). On Sequoia this loop is largely automatic; squib mostly needs to clear the masked state and resume.

`CNTPCT_EL0` and `CNTFRQ_EL0` are controlled per-vCPU via `hv_vcpu_set_sys_reg`. The frequency follows the host's apple-silicon counter (24 MHz on M-series).

### 5.2 Virtio-MMIO bus convention

Convention used by Firecracker, cloud-hypervisor and crosvm:
- Each device gets a 4 KiB MMIO window, allocated sequentially.
- IRQs allocated from the SPI pool starting at GIC INTID 32 (FDT type=0, IRQ-base=0).
- MMIO base address differs:

| VMM | Virtio-MMIO base |
|---|---|
| Firecracker | `0x4000_0000` (after RTC / serial) |
| cloud-hypervisor | dynamically allocated above `MAPPED_IO_START = 0x0900_0000` |
| libkrun | `0x0a00_0000` |
| QEMU virt | `0x0a00_0000` |

Squib will use `0x0a00_0000`, matching QEMU virt (= libkrun) — so off-the-shelf kernels with QEMU virt experience encounter familiar layout.

## 6. Per-vCPU Initial State

### 6.1 Boot vCPU (vCPU 0)

Set via `hv_vcpu_set_reg` / `hv_vcpu_set_sys_reg`:

```rust
// HV_REG_*
hv_vcpu_set_reg(vcpu, HV_REG_PC,   kernel_load_addr);
hv_vcpu_set_reg(vcpu, HV_REG_X0,   fdt_addr);
hv_vcpu_set_reg(vcpu, HV_REG_X1,   0);
hv_vcpu_set_reg(vcpu, HV_REG_X2,   0);
hv_vcpu_set_reg(vcpu, HV_REG_X3,   0);
hv_vcpu_set_reg(vcpu, HV_REG_CPSR, 0x3c5); // EL1h + DAIF masked
```

`0x3c5` decomposes as:

| Bit(s) | Name | Value | Meaning |
|---|---|---|---|
| [3:0] | M[3:0] | `0b0101` | EL1h |
| [4] | M[4] | `0` | AArch64 |
| [6] | F (FIQ) | `1` | Masked |
| [7] | I (IRQ) | `1` | Masked |
| [8] | A (SError) | `1` | Masked |
| [9] | D (Debug) | `1` | Masked |

This matches Firecracker's `PSTATE_FAULT_BITS_64 = PSR_MODE_EL1h | PSR_F_BIT | PSR_I_BIT | PSR_A_BIT | PSR_D_BIT = 0x3c5`.

### 6.2 What the VMM does NOT set

Per `booting.rst`, the kernel sets these itself in early arch entry:

- **SCTLR_EL1** — kernel turns on MMU after page tables built.
- **MAIR_EL1** — kernel programs memory attribute encodings.
- **TCR_EL1** — kernel sets translation control.
- **TTBR0_EL1 / TTBR1_EL1** — kernel installs page tables.
- **CPACR_EL1** — kernel enables FP/SIMD when needed.

The VMM is *only* responsible for: PC, X0–X3, PSTATE. Everything else can be left at HVF reset values. (Firecracker's KVM path explicitly does *not* set CPACR_EL1, MAIR_EL1, etc.)

### 6.3 Secondary vCPUs

Firecracker uses `KVM_ARM_VCPU_POWER_OFF` to start them off. The HVF analog: do not call `hv_vcpu_run` on the actor at all until PSCI `CPU_ON` arrives. Squib's vCPU actor sits in a "parked" state, the run loop only entered after a wake signal.

When `CPU_ON` arrives:
- Set PC = `entry_point` (from X1 of the originating CPU's HVC).
- Set X0 = `context_id`.
- Set X1–X3 = 0.
- Set CPSR = `0x3c5`.
- Reset SCTLR_EL1 to architectural reset value (caches/MMU off).
- Signal the actor.

## 7. Memory Layout for squib

Proposed guest physical address map (matches Firecracker conventions where they exist; uses QEMU virt MMIO/GIC bases for everything else, since Firecracker hides those behind dynamic allocation):

| Range | Size | Purpose |
|---|---|---|
| `0x0000_0000 – 0x07FF_FFFF` | 128 MiB | Reserved (low MMIO sandbox) |
| `0x0800_0000 – 0x0800_FFFF` | 64 KiB | **GIC distributor (GICD)** |
| `0x0809_0000 – 0x0809_FFFF` | 64 KiB | RTC (PL031) — optional |
| `0x0900_0000 – 0x0900_0FFF` | 4 KiB | **UART (PL011)** |
| `0x080A_0000 – 0x080A_0000 + 0x20000·N` | 128 KiB · N | **GIC redistributors** (N = vCPU count) |
| `0x0A00_0000 – 0x0A00_0FFF` | 4 KiB | virtio-mmio device 0 |
| `0x0A00_1000 – 0x0A00_1FFF` | 4 KiB | virtio-mmio device 1 |
| ... | 4 KiB each | up to 32 devices |
| `0x4000_0000 – 0x7FFF_FFFF` | 1 GiB | Reserved / firmware sandbox (unused for direct-kernel boot) |
| `0x8000_0000 – 0x8000_0FFF` | 4 KiB | (unused; aligns to 2 MiB before kernel) |
| `0x8020_0000` | — | **Kernel `Image` load address** (text_offset added per header) |
| `0x9000_0000` | — | **Initrd start** (1 GiB-aligned) |
| Top-of-RAM minus 2 MiB | 2 MiB | **FDT** |
| `0x8000_0000 + ram_size` | end | RAM end |

Notes:
- GICD/GICR sizes come from `hv_gic_get_distributor_size` / `hv_gic_get_redistributor_region_size`. Don't hardcode — Apple may change these.
- We adopt `MMIO_MEM_START = 0x0a00_0000`, IRQ_BASE = 32 (first SPI), IRQ_MAX = 159 (matches libkrun `IRQ_BASE..=IRQ_MAX`).
- DRAM_MEM_START = `0x8000_0000` (2 GiB) for Firecracker API parity.
- RAM size is bounded above by `0x00FF_8000_0000` (1022 GiB) — same as Firecracker.

## 8. ESR_EL2 Syndrome Decode

### 8.1 Register layout (ARM ARM, "ESR_EL2, Exception Syndrome Register (EL2)")

| Bits | Field | Meaning |
|---|---|---|
| [63:56] | RES0 | Reserved zero |
| [55:32] | ISS2 | Instruction-Specific Syndrome 2 (FEAT_LS64, etc.) |
| [31:26] | EC | Exception Class |
| [25] | IL | Instruction Length (1 = 32-bit, 0 = 16-bit) |
| [24:0] | ISS | Instruction-Specific Syndrome |

### 8.2 EC encoding (subset relevant to squib)

| EC | Source | Squib action |
|---|---|---|
| `0x00` | Unknown | Inject undefined-instruction; kill VM if from boot path |
| `0x01` | Trapped WFI/WFE | Park vCPU, wait for IRQ |
| `0x07` | FP/SIMD/SVE access trap | Should not occur (we don't trap FP) — bug if it does |
| `0x16` | HVC AArch64 | **PSCI dispatch** (Section 4) |
| `0x17` | SMC AArch64 | Ignore or treat as PSCI |
| `0x18` | MSR/MRS trap | System register passthrough — we shouldn't trap unless we set up trap masks |
| `0x20` / `0x21` | Instruction abort | Guest fault — log and likely terminate |
| `0x24` | Data abort, lower EL | **MMIO emulation** (Section 8.3) |
| `0x25` | Data abort, same EL | Hypervisor bug — log fatal |
| `0x3C` | BRK | Debugger breakpoint |

### 8.3 Data Abort ISS layout (EC = 0x24, ISV = 1)

| Bits | Field | Meaning |
|---|---|---|
| [24] | ISV | Instruction Syndrome Valid (0 = ISS invalid, must software-walk) |
| [23:22] | SAS | Access size: `0`=1 byte, `1`=2, `2`=4, `3`=8 |
| [21] | SSE | Sign-extended (for loads) |
| [20:16] | SRT | Syndrome Register Transfer (Xt register number, 0–31; 31 = XZR) |
| [15] | SF | 1 = 64-bit reg, 0 = 32-bit |
| [14] | AR | Acquire/Release |
| [13] | VNCR | (FEAT_NV2) |
| [12:11] | SET | Synchronous Error Type |
| [10] | FnV | FAR not valid |
| [9] | EA | External abort |
| [8] | CM | Cache maintenance |
| [7] | S1PTW | Stage-1 page-table walk |
| [6] | WnR | Write (1) / Read (0) |
| [5:0] | DFSC | Data Fault Status Code |

`FAR_EL2` holds the faulting **virtual** address. For MMIO from a stage-2 fault, the fault is on a stage-2 translation, so HVF gives us the **guest physical address** via `hv_vcpu_exit.exception.physical_address` (and `virtual_address` for the GVA).

### 8.4 Decoder pseudocode (Rust)

```rust
const EC_HVC64:  u32 = 0x16;
const EC_SMC64:  u32 = 0x17;
const EC_DABT_L: u32 = 0x24;
const EC_DABT_S: u32 = 0x25;
const EC_WFX:    u32 = 0x01;

#[derive(Debug)]
enum Decoded {
    Hvc { imm16: u16 },
    Smc { imm16: u16 },
    DataAbort(DataAbort),
    Wfx { is_wfe: bool },
    Other { ec: u32, iss: u32 },
}

#[derive(Debug)]
struct DataAbort {
    is_write: bool,
    size: u8,         // bytes: 1, 2, 4, 8
    sign_extend: bool,
    sf_64bit: bool,
    xt: u8,           // 0..=31
    isv: bool,
    s1ptw: bool,
}

fn decode_esr(esr: u64) -> Decoded {
    let ec  = ((esr >> 26) & 0x3f) as u32;
    let iss = (esr & 0x01ff_ffff) as u32;
    match ec {
        EC_HVC64 => Decoded::Hvc { imm16: (iss & 0xffff) as u16 },
        EC_SMC64 => Decoded::Smc { imm16: (iss & 0xffff) as u16 },
        EC_DABT_L | EC_DABT_S => {
            let isv = (iss >> 24) & 1 == 1;
            let sas = ((iss >> 22) & 0b11) as u8;
            Decoded::DataAbort(DataAbort {
                is_write:    (iss >> 6) & 1 == 1,
                size:        1u8 << sas,                    // 1,2,4,8
                sign_extend: (iss >> 21) & 1 == 1,
                sf_64bit:    (iss >> 15) & 1 == 1,
                xt:          ((iss >> 16) & 0x1f) as u8,
                isv,
                s1ptw:       (iss >> 7) & 1 == 1,
            })
        }
        EC_WFX => Decoded::Wfx { is_wfe: iss & 1 == 1 },
        _ => Decoded::Other { ec, iss },
    }
}
```

Reference implementations:
- `aarch64-esr-decoder` 0.2.4 (Apache-2.0, Google) — full decoder, depend on it directly if we want exhaustive coverage.
- libkrun `src/cpu/src/aarch64/sysreg.rs` and surrounding — practical microvm subset.

## 9. Build & Code-Signing

### 9.1 Minimum macOS version

**Recommendation: macOS 15.0 (Sequoia) minimum.**

Justification:
1. `hv_gic_*` is the deciding factor. The amount of code, complexity, and bug surface in userspace GICv3 emulation (priority queues, distributor/redistributor MMIO trap, LR shadowing, ITS) is enormous. libkrun's pre-15 path is ~3,000 LoC of fiddly state machine. Skipping it is the single biggest engineering win.
2. macOS 15 also adds `hv_vm_create_with_config` and richer vCPU configuration that's worth depending on.
3. Sequoia (released Sept 2024) is shipped on every supported Apple Silicon Mac as of May 2026 — there's no retro-compat case.
4. The user base for "Firecracker-on-Mac" is developer machines (CI, desktop dev loops). Bleeding-edge OS is the norm.

If we ever care about macOS 14, we'd have to write the userspace GIC. We should explicitly *not* commit to that.

### 9.2 macOS 26 (Tahoe) considerations

New in Tahoe (Oct 2025) per [macOS Tahoe 26 release notes](https://developer.apple.com/documentation/macos-release-notes/macos-26-release-notes):
- IPA (intermediate physical address) memory granularity configurable down to 4 KiB.
- vmnet now supports custom network topologies.
- No new entitlement requirements for `hv_gic_*` or core HVF.

Squib should *target* macOS 15.0 minimum and *test* on 26.x. No code changes needed for forward compat.

### 9.3 Toolchain settings

- `MACOSX_DEPLOYMENT_TARGET=15.0` in `.cargo/config.toml` `[env]`.
- `rust-toolchain.toml` pinned to latest stable Rust (1.85+ for 2024 edition).
- Target triple: `aarch64-apple-darwin` only. We do not cross-compile to x86_64.

### 9.4 Code signing

Required for any binary that uses Hypervisor.framework:

```xml
<!-- entitlements.plist -->
<key>com.apple.security.hypervisor</key><true/>
```

For vmnet (host networking), additionally:
- `com.apple.vm.networking` (developer ID needed; not for unsigned dev builds).
- For "shared" mode, the `vmnet` framework itself does not require sandbox elevation; for "host" or "bridged", root or vmnet entitlement required.

Hardened runtime flags:
- `--options runtime` on `codesign`.
- For distribution: notarize via `notarytool`. For local dev: ad-hoc `codesign -s -` is enough as long as the entitlement plist is attached.

Squib's `Makefile` should have:

```make
sign:
	codesign --entitlements squib.entitlements -s - target/release/squib

verify:
	codesign --display --entitlements - target/release/squib
```

No new entitlements added in macOS 15 or 26 that affect HVF or vmnet for our use case.

## 10. Summary: "What to Borrow" Matrix

| Subsystem | Source | Crate / Path | License |
|---|---|---|---|
| Kernel image (PE) load | `linux-loader` 0.13.2 | `loader::pe::PE::load` | Apache-2.0 / BSD-3 |
| `Image.gz` decompress | `flate2` | `flate2::read::GzDecoder` | MIT / Apache-2.0 |
| FDT writer | `vm-fdt` 0.3.0 | `FdtWriter` | Apache-2.0 / BSD-3 |
| FDT structure & node order | Firecracker `arch/aarch64/fdt.rs` | port | Apache-2.0 |
| Memory layout | Firecracker `arch/aarch64/layout.rs` (RAM) + libkrun (MMIO/GIC) | port | Apache-2.0 |
| HVF wrapper | `applevisor` 1.0 + `applevisor-sys` | direct dep, `macos-15-0` feature | Apache-2.0 |
| GIC | Apple `hv_gic_*` (no userspace emulation) | via `applevisor::gic` | Apple SDK |
| PSCI handler | libkrun `src/cpu/.../psci.rs` (HVF path) | port + clean-room | Apache-2.0 |
| ESR_EL2 decode | `aarch64-esr-decoder` 0.2.4 (or inline) | optional dep | Apache-2.0 |
| vCPU regs init | Firecracker `arch/aarch64/regs.rs` (constants only) | reimpl for HVF | Apache-2.0 |
| Timer | direct `hv_vcpu_*` register access | — | — |
| virtio-MMIO bus layout | QEMU virt convention (= libkrun) | reimpl | — |

## 11. Concrete Numeric Memory Layout (squib)

Final proposal:

```
0x0000_0000 .. 0x07FF_FFFF  (128 MiB)  reserved low MMIO
0x0800_0000 .. 0x0800_FFFF  ( 64 KiB)  GICD       (size from hv_gic_get_distributor_size)
0x0809_0000 .. 0x0809_0FFF  (  4 KiB)  PL031 RTC  (optional)
0x080A_0000 .. variable     (128 KiB×N)GICR       (N = vCPUs; size from hv_gic_get_redistributor_region_size)
0x0900_0000 .. 0x0900_0FFF  (  4 KiB)  PL011 UART (SPI 1)
0x0A00_0000 .. 0x0A01_FFFF  (128 KiB)  virtio-mmio (32×4 KiB; SPIs 16..47)
0x0A02_0000 .. 0x3FFF_FFFF             reserved
0x4000_0000 .. 0x7FFF_FFFF  (  1 GiB)  reserved (firmware sandbox; unused for direct-kernel)
0x8000_0000                            DRAM start
  +0x0020_0000                         kernel Image load (2 MiB-aligned)
  +0x1000_0000                         initrd (heuristic, ≥256 MiB above kernel)
  ..ram_end - 0x0020_0000              FDT (last 2 MiB of RAM)
  ..ram_end                            RAM end (ram_size from API)
```

Bounds: `0x8000_0000 ≤ ram_end < 0x00FF_8000_0000` (max 1022 GiB).

## Sources

- [Linux arm64 booting.rst](https://docs.kernel.org/arch/arm64/booting.html)
- [linux-loader on crates.io](https://crates.io/crates/linux-loader)
- [vm-fdt on GitHub](https://github.com/rust-vmm/vm-fdt)
- [Apple Hypervisor — GIC functions](https://developer.apple.com/documentation/hypervisor/gic-functions)
- [`hv_gic_set_state`](https://developer.apple.com/documentation/hypervisor/hv_gic_set_state(_:_:))
- [applevisor on docs.rs](https://docs.rs/applevisor)
- [Linux psci.h UAPI](https://github.com/torvalds/linux/blob/master/include/uapi/linux/psci.h)
- [PSCI device tree binding](https://www.kernel.org/doc/Documentation/devicetree/bindings/arm/psci.txt)
- [Firecracker `arch/aarch64/`](https://github.com/firecracker-microvm/firecracker/tree/main/src/vmm/src/arch/aarch64)
- [cloud-hypervisor `arch/src/aarch64/`](https://github.com/cloud-hypervisor/cloud-hypervisor/tree/main/arch/src/aarch64)
- [libkrun](https://github.com/containers/libkrun)
- [aarch64-esr-decoder](https://github.com/google/aarch64-esr-decoder)
- [ARM ESR_EL2 reference](https://developer.arm.com/documentation/ddi0595/2021-03/AArch64-Registers/ESR-EL2--Exception-Syndrome-Register--EL2-)
- [macOS Tahoe 26 release notes](https://developer.apple.com/documentation/macos-release-notes/macos-26-release-notes)
