//! Live HVF round-trip for [`squib_hv::HvfVcpuSnapshot`] +
//! [`squib_hv::HvfGicSnapshot`].
//!
//! 1. Init the singleton VM, create a vCPU on its owner thread.
//! 2. Set X0..X4, PC, PSTATE, FPCR, FPSR, SCTLR_EL1, MAIR_EL1, VBAR_EL1 to known sentinel values.
//! 3. Capture vCPU state through `HvfVcpuSnapshot`.
//! 4. Reset registers to zero (defensive — confirms capture really pulled from HVF, not from a
//!    cached value).
//! 5. Restore from the captured state.
//! 6. Read back through HVF and assert byte equality with the sentinels.
//!
//! Plus a GIC blob round trip: `HvfGicSnapshot::capture` returns a non-empty
//! blob; `restore` accepts the same blob without error.
//!
//! `#[ignore]` because it requires the `com.apple.security.hypervisor`
//! entitlement on the test binary. `make hvf-test` codesigns + runs.

#![cfg(target_os = "macos")]
#![allow(clippy::doc_markdown)]

use std::thread;

use applevisor::{
    vcpu::{Reg, SimdFpReg, SysReg as HvfSysReg},
    vm::VirtualMachineStaticInstance,
};
use squib_arch::SysReg;
use squib_gic::GicSizes;
use squib_hv::{HvfGicSnapshot, HvfHypervisor, HvfVcpuSnapshot};
use squib_snapshot::{
    GicRestoreTarget as _, GicSnapshotSource, capture_vcpu_state, restore_vcpu_state,
};

// Top-level constants so the closure body can use them without tripping
// `clippy::items_after_statements`.
const SENTINEL_X0: u64 = 0xCAFE_BEEF_DEAD_BEAD;
const SENTINEL_X1: u64 = 0x1234_5678_9ABC_DEF0;
const SENTINEL_PC: u64 = 0x4000_0000;
const SENTINEL_PSTATE: u64 = squib_arch::BOOT_PSTATE;
const SENTINEL_SCTLR: u64 = 0x0000_0000_30C5_0838;
const SENTINEL_MAIR: u64 = 0x0000_0000_BB44_FF00;
const SENTINEL_VBAR: u64 = 0x4000_8000;

#[test]
#[ignore = "requires com.apple.security.hypervisor entitlement on the test binary; run via `make \
            hvf-test`"]
fn hvf_round_trips_a_vcpu_state_through_capture_and_restore() {
    let hv = HvfHypervisor::new();
    let sizes = GicSizes::query()
        .expect("GicSizes::query — does the test binary have the right entitlement?");
    let _vm = hv
        .init_vm(1, sizes.redistributor_per_vcpu)
        .expect("HvfHypervisor::init_vm");

    // The vCPU lifecycle methods must run on the same thread that called
    // `vcpu_create`. We do everything (set, capture, reset, restore, verify)
    // inside a single thread closure to keep that contract.
    let outcome = thread::spawn(|| -> Result<(), String> {
        let instance = VirtualMachineStaticInstance::get_gic()
            .ok_or("VM instance not available after init")?;
        let vcpu = instance
            .vcpu_create()
            .map_err(|e| format!("vcpu_create: {e:?}"))?;

        // Stage state.
        vcpu.set_reg(Reg::X0, SENTINEL_X0)
            .map_err(|e| format!("X0: {e:?}"))?;
        vcpu.set_reg(Reg::X1, SENTINEL_X1)
            .map_err(|e| format!("X1: {e:?}"))?;
        vcpu.set_reg(Reg::X2, 0x2222)
            .map_err(|e| format!("X2: {e:?}"))?;
        vcpu.set_reg(Reg::X3, 0x3333)
            .map_err(|e| format!("X3: {e:?}"))?;
        vcpu.set_reg(Reg::X4, 0x4444)
            .map_err(|e| format!("X4: {e:?}"))?;
        vcpu.set_reg(Reg::PC, SENTINEL_PC)
            .map_err(|e| format!("PC: {e:?}"))?;
        vcpu.set_reg(Reg::CPSR, SENTINEL_PSTATE)
            .map_err(|e| format!("CPSR: {e:?}"))?;
        vcpu.set_sys_reg(HvfSysReg::SCTLR_EL1, SENTINEL_SCTLR)
            .map_err(|e| format!("SCTLR: {e:?}"))?;
        vcpu.set_sys_reg(HvfSysReg::MAIR_EL1, SENTINEL_MAIR)
            .map_err(|e| format!("MAIR: {e:?}"))?;
        vcpu.set_sys_reg(HvfSysReg::VBAR_EL1, SENTINEL_VBAR)
            .map_err(|e| format!("VBAR: {e:?}"))?;
        // Q0: nontrivial SIMD bytes — verify FP/SIMD path round-trips.
        vcpu.set_simd_fp_reg(
            SimdFpReg::Q0,
            0x1234_5678_9ABC_DEF0_AABB_CCDD_EEFF_0011_u128,
        )
        .map_err(|e| format!("Q0: {e:?}"))?;
        vcpu.set_reg(Reg::FPCR, 0x0000_0000_0700_0000)
            .map_err(|e| format!("FPCR: {e:?}"))?;
        vcpu.set_reg(Reg::FPSR, 0x0000_0000_1000_0000)
            .map_err(|e| format!("FPSR: {e:?}"))?;

        // Capture.
        let snapshot = HvfVcpuSnapshot::new(&vcpu, /* mpidr */ 0x0001_0000);
        let captured =
            capture_vcpu_state(&snapshot).map_err(|e| format!("capture_vcpu_state: {e:?}"))?;
        assert_eq!(captured.regs.x[0], SENTINEL_X0);
        assert_eq!(captured.regs.x[1], SENTINEL_X1);
        assert_eq!(captured.regs.pc, SENTINEL_PC);
        assert_eq!(captured.regs.pstate, SENTINEL_PSTATE);
        // SCTLR/MAIR/VBAR are inside the sysreg map — keyed by squib's
        // wire encoding.
        let sctlr_key = SysReg::SctlrEl1.as_encoded();
        let mair_key = SysReg::MairEl1.as_encoded();
        let vbar_key = SysReg::VbarEl1.as_encoded();
        assert_eq!(
            captured.sys_regs.get(&sctlr_key).copied(),
            Some(SENTINEL_SCTLR)
        );
        assert_eq!(
            captured.sys_regs.get(&mair_key).copied(),
            Some(SENTINEL_MAIR)
        );
        assert_eq!(
            captured.sys_regs.get(&vbar_key).copied(),
            Some(SENTINEL_VBAR)
        );
        assert_eq!(captured.fp_regs.fpcr, 0x0000_0000_0700_0000);
        assert_eq!(captured.fp_regs.fpsr, 0x0000_0000_1000_0000);
        // Q0 split into low/high u64 halves.
        assert_eq!(captured.fp_regs.v[0][0], 0xAABB_CCDD_EEFF_0011);
        assert_eq!(captured.fp_regs.v[0][1], 0x1234_5678_9ABC_DEF0);

        // Defensive reset — confirms capture really pulled live values.
        vcpu.set_reg(Reg::X0, 0).unwrap();
        vcpu.set_reg(Reg::X1, 0).unwrap();
        vcpu.set_reg(Reg::PC, 0).unwrap();
        vcpu.set_sys_reg(HvfSysReg::SCTLR_EL1, 0).unwrap();
        vcpu.set_sys_reg(HvfSysReg::MAIR_EL1, 0).unwrap();
        vcpu.set_sys_reg(HvfSysReg::VBAR_EL1, 0).unwrap();
        vcpu.set_simd_fp_reg(SimdFpReg::Q0, 0).unwrap();

        // Restore.
        let mut target = HvfVcpuSnapshot::new(&vcpu, captured.mpidr);
        restore_vcpu_state(&mut target, &captured, /* vcpu_index */ 0)
            .map_err(|e| format!("restore_vcpu_state: {e:?}"))?;

        // Verify by reading back through HVF.
        assert_eq!(vcpu.get_reg(Reg::X0).unwrap(), SENTINEL_X0);
        assert_eq!(vcpu.get_reg(Reg::X1).unwrap(), SENTINEL_X1);
        assert_eq!(vcpu.get_reg(Reg::PC).unwrap(), SENTINEL_PC);
        assert_eq!(
            vcpu.get_sys_reg(HvfSysReg::SCTLR_EL1).unwrap(),
            SENTINEL_SCTLR
        );
        assert_eq!(
            vcpu.get_sys_reg(HvfSysReg::MAIR_EL1).unwrap(),
            SENTINEL_MAIR
        );
        assert_eq!(
            vcpu.get_sys_reg(HvfSysReg::VBAR_EL1).unwrap(),
            SENTINEL_VBAR
        );
        let q0_back: u128 = vcpu.get_simd_fp_reg(SimdFpReg::Q0).unwrap();
        assert_eq!(q0_back, 0x1234_5678_9ABC_DEF0_AABB_CCDD_EEFF_0011);
        Ok(())
    })
    .join()
    .expect("vCPU thread panicked");
    outcome.expect("vcpu round-trip");
}

#[test]
#[ignore = "requires com.apple.security.hypervisor entitlement on the test binary; run via `make \
            hvf-test`"]
fn hvf_round_trips_the_gic_state_blob() {
    let hv = HvfHypervisor::new();
    let sizes = GicSizes::query().expect("GicSizes::query");
    let _vm = hv
        .init_vm(1, sizes.redistributor_per_vcpu)
        .expect("init_vm");

    let mut snapshot = HvfGicSnapshot::new().expect("HvfGicSnapshot::new");
    let blob = snapshot.capture().expect("GIC capture");
    assert!(!blob.is_empty(), "GIC blob must be non-empty");

    snapshot
        .restore(&blob)
        .expect("GIC restore must accept the just-captured blob");
}
