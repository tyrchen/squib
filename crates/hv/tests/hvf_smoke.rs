//! Live HVF smoke test for the Phase 1 binding stack.
//!
//! Runs only on macOS targets with the `com.apple.security.hypervisor` entitlement on
//! the test binary. The `make hvf-test` target signs the test binary then re-runs
//! `cargo test`, mirroring the applevisor crate's own test workflow.
//!
//! The test cuts through the full Phase 1 stack:
//!
//! 1. `HvfHypervisor::init_vm` calls `applevisor::VirtualMachineStaticInstance::init_with_gic` with
//!    squib's fixed GIC config.
//! 2. `HvfVm::map_memory` allocates and maps a 16 KiB region at `DRAM_BASE`, returning a typed
//!    [`squib_hv::MappedRegion`] — no raw pointer exposed.
//! 3. `HvfVm::write_to_region` writes the encoded `HVC #0` instruction (4 bytes, little-endian
//!    `0xD400_0002`) at offset 0.
//! 4. A dedicated thread fetches the singleton `VirtualMachineStaticInstance` (the HVF rule:
//!    `vcpu_create` must run on the thread that drives `run()`), creates the vCPU, sets `PC =
//!    DRAM_BASE` and `PSTATE = BOOT_PSTATE` (`EL1h`, `DAIF` masked), and calls `vcpu.run()`.
//! 5. The trap returns with `ExitReason::EXCEPTION`; we read the syndrome via
//!    `vcpu.get_exit_info()` and feed it through `squib_arch::decode_esr`.
//! 6. The decoder yields `EsrDecoded::Hvc { imm16: 0 }` — confirming end-to-end that the binding
//!    round-trips a guest exception with the upstream syndrome bits intact.
//!
//! Note: `HvfVm` is not `Send` because it carries `applevisor::Memory` which holds a
//! raw `*const c_void`. We deliberately keep the `HvfVm` on the main thread (where it
//! also keeps the mappings alive) and let the vCPU thread reach the singleton via
//! `applevisor::VirtualMachineStaticInstance::get_gic` instead of borrowing through
//! `HvfVm`. The mappings are torn down only after `handle.join()` completes, so the
//! guest memory is live for the entire run.
//!
//! Adding more HVF tests means a new `tests/<name>.rs` (each becomes its own test
//! binary, so the process-global VM init is fresh).

#![cfg(target_os = "macos")]
#![allow(clippy::doc_markdown)]

use std::thread;

use applevisor::{
    memory::MemPerms,
    vcpu::{ExitReason, Reg},
    vm::VirtualMachineStaticInstance,
};
use squib_arch::{BOOT_PSTATE, EsrDecoded, decode_esr, layout::DRAM_BASE};
use squib_gic::GicSizes;
use squib_hv::HvfHypervisor;

#[test]
#[ignore = "requires com.apple.security.hypervisor entitlement on the test binary; run via `make \
            hvf-test`"]
fn hvf_round_trips_an_hvc_trap_via_real_vcpu() {
    // ARM64 `HVC #0` encodes to 0xD400_0002. Stored little-endian in guest RAM:
    const HVC_HASH_0: [u8; 4] = [0x02, 0x00, 0x00, 0xD4];

    let hv = HvfHypervisor::new();
    let sizes = GicSizes::query().expect(
        "GicSizes::query — does the test binary have the com.apple.security.hypervisor \
         entitlement? Run `make hvf-test`.",
    );
    let vm = hv.init_vm(1, sizes.redistributor_per_vcpu).expect(
        "HvfHypervisor::init_vm failed — most likely the test binary needs codesign with \
         com.apple.security.hypervisor. Run `make hvf-test`.",
    );

    // One 16 KiB page at DRAM_BASE — page geometry on Apple Silicon is 16 KiB (D21).
    let region = vm
        .map_memory(DRAM_BASE, 16 * 1024, MemPerms::RWX)
        .expect("HvfVm::map_memory failed");

    // Write HVC at offset 0 → guest PC begins execution there.
    vm.write_to_region(&region, 0, &HVC_HASH_0)
        .expect("HvfVm::write_to_region failed");

    let handle = thread::spawn(|| {
        // HVF rule (12 § 4): vcpu_create + run() must come from the same thread.
        // We pull the singleton via the static accessor rather than carrying `HvfVm`
        // across the thread boundary — see the module docs for why.
        let static_instance = VirtualMachineStaticInstance::get_gic()
            .expect("VM instance should exist after init_vm");
        let vcpu = static_instance
            .vcpu_create()
            .expect("VirtualMachineInstance::vcpu_create failed");

        vcpu.set_reg(Reg::PC, DRAM_BASE).expect("set Reg::PC");
        vcpu.set_reg(Reg::CPSR, BOOT_PSTATE)
            .expect("set Reg::CPSR (PSTATE) to BOOT_PSTATE");

        vcpu.run().expect("hv_vcpu_run failed");
        vcpu.get_exit_info()
    });

    let exit = handle.join().expect("vCPU thread panicked");

    // Drop order: `vm` (and therefore the mappings) is dropped at the end of this
    // function, *after* the join has returned. The guest memory was alive for the
    // whole vCPU run.
    drop(vm);

    assert_eq!(
        exit.reason,
        ExitReason::EXCEPTION,
        "expected HV_EXIT_REASON_EXCEPTION, got {:?}",
        exit.reason
    );

    let esr = exit.exception.syndrome;
    match decode_esr(esr) {
        EsrDecoded::Hvc { imm16 } => assert_eq!(
            imm16, 0,
            "encoded HVC #0; decoder should produce imm16 = 0; got {imm16}"
        ),
        other => panic!("expected EsrDecoded::Hvc, got {other:?} (raw ESR = {esr:#018x})"),
    }
}
