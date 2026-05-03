---
title: 12-hvf-backend — Apple Hypervisor.framework via applevisor
type: design
status: draft
last_updated: 2026-05-03
depends_on: 11-runtime-core.md, 13-arch-and-boot.md
---

# 12 · HVF Backend — Apple Hypervisor.framework via `applevisor`

Status: draft · Owner: squib-hv · Depends on: [11-runtime-core.md](./11-runtime-core.md), [13-arch-and-boot.md](./13-arch-and-boot.md)

## 1. Purpose

`squib-hv` is **the** unsafe boundary in the workspace. It owns:

- The HVF binding, via the `applevisor = "1.0"` crate (features = `["macos-15-0"]`). The feature gate matches our minimum-supported macOS (D2). Bumping to `macos-15-2` for SME state save/restore or `macos-26-0` for Tahoe-only refinements is a deliberate D-record + roadmap conversation, not a silent dependency bump.
- The vCPU run loop translating HVF exits into the squib-core `VmExit` algebra.
- The in-kernel GICv3 wrapper around `hv_gic_*` (macOS 15+).
- A thin Mach-exception helper used by the postcopy pager — see [16-snapshots.md § 5](./16-snapshots.md#5-postcopy--lazy-restore).

Every `unsafe` line in the production build lives here or in `squib-net::sys`. Every other crate carries `#![forbid(unsafe_code)]`.

## 2. Why HVF and not VZ

The earlier design considered VZ-default for time-to-MVP. That was the right calculus when "what works in 6 weeks" was the goal. Under the current direction — **performance is the priority and 1.0 ships full feature parity** — VZ disqualifies itself on three counts:

1. Closed device model rules out custom virtio-MMIO devices and per-queue rate limiters.
2. No PVH or kernel-level boot tuning, costing boot-time budget.
3. No dirty-page tracking, killing Diff snapshots.

HVF gives us the per-µs control and the right primitives. The decision is permanent and recorded as [99-key-decisions.md § D1](./99-key-decisions.md#d1-hvf-only-no-vz).

## 3. Why macOS 15 minimum

`hv_gic_create` and the `hv_gic_*` family land in macOS 15 Sequoia. With them we get a hypervisor-managed GICv3 — no userspace distributor/redistributor emulation, no MMIO trap handling for GIC accesses, no LR shadow, no pending/active bitmap arbitration. libkrun's pre-15 userspace path is ~3K LoC of fiddly state machine; we deliberately do not carry it.

The cost: users on macOS 14 cannot run squib. Recorded as [99-key-decisions.md § D2](./99-key-decisions.md#d2-macos-15-minimum).

## 4. Threading rules

HVF imposes a hard pthread-affinity contract: every `hv_vcpu_*` call must come from the OS thread that called `hv_vcpu_create`. `squib-hv::HvfVcpu` enforces this:

- `HvfVcpu::new` records `std::thread::current().id()` at construction.
- Every method that calls into `applevisor::Vcpu` checks the thread id; on mismatch it returns `Error::Threading` (debug) or panics (release after a `tracing::error!`).
- The trait method `Vcpu::cancel()` is the **only** member that bypasses the check, because `applevisor::Vcpu::exit()` (i.e. `hv_vcpus_exit`) is documented as callable from any thread.

`HvfVcpu` is `Send` but not `Sync`. `HvfVm` is `Send + Sync`.

## 5. vCPU run loop

Ported essentially verbatim from `vendors/libkrun/src/hvf/src/lib.rs::HvfVcpu::run`, with FFI calls translated to `applevisor`:

```text
loop {
    pre_run_housekeeping():
        // - if pending MMIO read result, write to dst register
        // - if pending_advance_pc, set PC += 4
        // - if vcpu_list.has_pending_irq(), set_pending_irq

    match applevisor_vcpu.run() {
        VTIMER_ACTIVATED => { vtimer_masked = true; return VtimerActivated; }
        CANCELLED        => return Cancelled;
        EXCEPTION        => match decode_esr(esr_el2) {
            EC_DATAABORT (0x24) => set pending_advance_pc; return Mmio { ... };
            EC_HVC      (0x16)  => return Hvc { imm16, x: [x0..x3] };       // VMM dispatches PSCI
            EC_SMC      (0x17)  => return Smc { ... };
            EC_MSR/MRS  (0x18)  => return SystemRegister { ... };
            EC_WFx      (0x01)  => compute timer deadline; return Wfi or Wfe;
            EC_BRK      (0x3c)  => return Brk;
            other               => return InternalError(format!("ec={other}"));
        }
    }
}
```

The ESR_EL2 syndrome decoder is in `squib-arch` (see [13-arch-and-boot.md § 4](./13-arch-and-boot.md#4-esr_el2-decoder)).

`pending_advance_pc` is necessary because HVC, like data aborts, is reported on a faulting instruction; HVF does not auto-advance PC on resume. The vCPU sets the flag at the exit boundary and clears it on the next `pre_run_housekeeping`.

## 6. GIC — `hv_gic_*` only

`squib-gic` (a thin wrapper crate over `applevisor::gic::*`) implements the `Gic` trait from squib-core. Lifecycle:

1. `hv_gic_config_create` → set distributor base, redistributor base, MSI region (we do not use MSI in 1.0; configure with empty range).
2. `hv_gic_create(cfg)` — fixes the layout.
3. SPI assertion: `hv_gic_set_spi(intid, level)` for level-triggered, edge pulses via on/off pair.
4. Snapshot: `hv_gic_state_create / get_size / get_data` — opaque blob, serialized into [`MicrovmState.gic_state`](./10-data-model.md#5-microvmstate--the-snapshot-state-blob).

There is **no** userspace distributor / redistributor emulation. Crate fails to initialise on macOS < 15.

The MMIO base addresses are pinned in [13-arch-and-boot.md § 2](./13-arch-and-boot.md#2-memory-layout-concrete).

## 7. Memory mapping

```rust
fn map_memory(&self, host: *mut u8, ipa: u64, len: u64, perms: Protection) -> Result<()>;
```

Wraps `hv_vm_map`. Caller-allocated host buffer (anonymous mmap, RWX); `Protection` translates to HVF flags. Three legitimate callers:

- `squib-vmm::builder` — once per RAM region at boot.
- `squib-snapshot::dirty` — temporary `protect_memory` calls flipping write bits during dirty tracking. See [16-snapshots.md § 4](./16-snapshots.md#4-dirty-page-tracking).
- `squib-host::pager` — one big `map_memory(RWX)` over a `mach_vm_protect(NONE)` host range, so faults surface to a Mach exception port. See [16-snapshots.md § 5](./16-snapshots.md#5-postcopy--lazy-restore).

`unmap_memory` is symmetric; rarely called outside teardown.

## 8. Behaviour edges

- **Cancellation race**: if `cancel()` arrives while the vCPU is in `pre_run_housekeeping`, the next `applevisor_vcpu.run()` returns `CANCELLED` immediately. No spurious exit; safe.
- **Pending IRQ flush**: on resume from `Pause`, all pending IRQs in the per-vCPU shadow set are re-asserted via `set_pending_irq` before `run` is called.
- **vtimer**: `VtimerActivated` exits suspend the vCPU until either a guest WFI/WFE wakeup or a host timer fires. `vtimer_masked = true` is cleared on the next `set_pending_irq` for the timer's IRQ.
- **Brk**: `EC_BRK` is forwarded as `VmExit::Brk` so the VMM can plug a debugger; default policy is to log at `error` and shut down.
- **InternalError**: any unknown ESR class returns `InternalError(String)` rather than panicking. Caller logs at `error!` and returns `503 Service Unavailable` to the API.

## 9. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-HV-1 | All `unsafe` blocks in `squib-hv` carry `// SAFETY:` comments referencing the HVF doc the safety claim depends on. | Code review + `cargo clippy -- -W clippy::missing_safety_doc` |
| I-HV-2 | `HvfVcpu` methods other than `cancel`, when called from a foreign thread, return `Error::Threading` (release) or panic (debug). | Unit test calling each method from a `std::thread::spawn` |
| I-HV-3 | `Gic` initialisation fails cleanly on macOS < 15 with a 4xx-mappable error. | Integration test on macOS 14 CI lane (or build-time check via `MACOSX_DEPLOYMENT_TARGET`) |
| I-HV-4 | The vCPU loop never panics on a malformed ESR; unknown ECs return `InternalError`. | Property test feeding random ESR values into the decoder |

## 10. Cross-references

- ← Depends on: [11-runtime-core.md](./11-runtime-core.md), [13-arch-and-boot.md](./13-arch-and-boot.md)
- → Consumed by: [16-snapshots.md](./16-snapshots.md) (dirty tracking, GIC state), [70-security.md](./70-security.md) (unsafe boundary), [71-performance-budgets.md](./71-performance-budgets.md) (vCPU exit dispatch budget)
- ↔ Related research: [docs/research/hvf-prior-art-deep-dive.md](../docs/research/hvf-prior-art-deep-dive.md), [docs/research/aarch64-hvf-guest-stack.md § 4–8](../docs/research/aarch64-hvf-guest-stack.md), [docs/research/hvf-performance-and-snapshots.md](../docs/research/hvf-performance-and-snapshots.md)
