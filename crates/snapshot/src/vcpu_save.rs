//! vCPU + GIC capture/restore helpers — the host-portable surface.
//!
//! Per [16-snapshots.md § 2](../../../specs/16-snapshots.md#2-state-file) the
//! snapshot writer drives every vCPU thread through `VcpuCommand::SaveState` and
//! the GIC through `hv_gic_state_get_data`. This module pins the trait surface;
//! `squib-hv` provides the live HVF implementation, and tests stand in mock
//! sources.
//!
//! The traits split capture and restore so an implementation can be `Send` for
//! save (the snapshot path can pull state from any thread that owns the vCPU) but
//! `&mut self` for restore (the boot orchestrator owns the vCPU and re-installs
//! state before any `hv_vcpu_run`).
//!
//! ## PSCI normalization on restore (16 § 2)
//!
//! After loading a saved state, the boot orchestrator drives every vCPU through
//! `normalize_psci_on_restore`: the BSP (vCPU 0) is set Running, the secondaries
//! are set Off. The guest's PSCI driver re-issues `CPU_ON` for each secondary on
//! its first scheduling cycle. We do **not** preserve the saved PSCI state across
//! restore because vCPU thread identity differs between save and restore (HVF
//! affinity contract per 12 § 4).

use std::collections::BTreeMap;

use squib_arch::SysReg;

use crate::{
    error::{Result, SnapshotError},
    state::{FpSimdRegs, GpRegs, MmdsState, PsciVcpuState, VcpuState},
};

/// Source-of-truth for a single vCPU's state at snapshot time.
///
/// Implemented by `squib-hv::HvfVcpu` for the production path and by mock fixtures
/// for unit tests.
pub trait VcpuSnapshotSource {
    /// MPIDR_EL1 affinity bits for this vCPU (matches the FDT cpu node).
    fn mpidr(&self) -> u64;

    /// Read the vCPU's general-purpose register file.
    fn read_gp_regs(&self) -> Result<GpRegs>;

    /// Read the vCPU's FP/SIMD register file.
    fn read_fp_simd(&self) -> Result<FpSimdRegs>;

    /// Read one curated sysreg.
    ///
    /// `None` means the host backend cannot read this register (e.g. a debug
    /// register the host kernel hides); the capture treats `None` as "skip" so
    /// the resulting state file simply omits the register and the restore path
    /// re-uses the reset-default.
    fn read_sys_reg(&self, reg: SysReg) -> Result<Option<u64>>;

    /// Live PSCI affinity state at save time. Diagnostic only — restore
    /// normalises away from this value (see module docs).
    fn psci_state(&self) -> PsciVcpuState;
}

/// Restore target for a single vCPU.
pub trait VcpuRestoreTarget {
    /// Write the vCPU's general-purpose register file.
    fn write_gp_regs(&mut self, regs: &GpRegs) -> Result<()>;

    /// Write the vCPU's FP/SIMD register file.
    fn write_fp_simd(&mut self, regs: &FpSimdRegs) -> Result<()>;

    /// Write one curated sysreg. `None` from the source means "use reset
    /// default"; the restore path skips writing.
    fn write_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()>;

    /// Set the vCPU's PSCI state for the post-restore boot.
    ///
    /// Per the normalization rule, this is `On` for vCPU 0 (BSP) and `Off` for
    /// all secondaries — the saved value is **not** propagated.
    fn set_psci_state(&mut self, state: PsciVcpuState) -> Result<()>;
}

/// Source-of-truth for the GIC blob.
///
/// `applevisor::Gic::get_state_data` produces an opaque byte buffer the kernel
/// requires us to round-trip; squib treats it as a black box (D5 / D6).
pub trait GicSnapshotSource {
    /// Return the opaque GIC state bytes via `hv_gic_state_get_data`.
    fn capture(&self) -> Result<Vec<u8>>;
}

/// Restore target for the GIC.
pub trait GicRestoreTarget {
    /// Restore the opaque GIC state via `hv_gic_state_set_data`.
    ///
    /// **Must** run before any `hv_vcpu_run`. Calling after a vCPU has run is a
    /// `SnapshotError::Incompatible`.
    fn restore(&mut self, data: &[u8]) -> Result<()>;
}

/// MMDS source-of-truth.
pub trait MmdsSnapshotSource {
    /// Capture the MMDS state. `None` means MMDS was not enabled and the saved
    /// state will omit the field.
    fn capture(&self) -> Result<Option<MmdsState>>;
}

/// MMDS restore target.
pub trait MmdsRestoreTarget {
    /// Restore the MMDS state. `None` means the saved state had no MMDS payload;
    /// the target leaves its current MMDS state untouched.
    fn restore(&mut self, state: Option<&MmdsState>) -> Result<()>;
}

/// Capture one vCPU's state from a [`VcpuSnapshotSource`].
///
/// The captured `BTreeMap<u64, u64>` is keyed by [`SysReg::as_encoded`] for stable
/// round-trip with [`restore_vcpu_state`].
///
/// # Errors
/// Any error surfaced by the source.
pub fn capture_vcpu_state<S: VcpuSnapshotSource>(source: &S) -> Result<VcpuState> {
    let regs = source.read_gp_regs()?;
    let fp_regs = source.read_fp_simd()?;
    let mut sys_regs = BTreeMap::new();
    for reg in SysReg::all() {
        if let Some(value) = source.read_sys_reg(*reg)? {
            sys_regs.insert(reg.as_encoded(), value);
        }
    }
    Ok(VcpuState {
        mpidr: source.mpidr(),
        regs,
        fp_regs,
        sys_regs,
        psci_state: source.psci_state(),
    })
}

/// Restore one vCPU's state into a [`VcpuRestoreTarget`].
///
/// The PSCI state written into the target is **normalized**: BSP-Running for
/// `vcpu_index == 0`, Off for every other vCPU. The saved `psci_state` is kept in
/// the state blob for diagnostics but never propagated.
///
/// # Errors
/// [`SnapshotError::Incompatible`] if a saved sysreg key cannot be decoded;
/// any error surfaced by the target.
pub fn restore_vcpu_state<T: VcpuRestoreTarget>(
    target: &mut T,
    state: &VcpuState,
    vcpu_index: u32,
) -> Result<()> {
    target.write_gp_regs(&state.regs)?;
    target.write_fp_simd(&state.fp_regs)?;
    for (encoded, value) in &state.sys_regs {
        let reg = SysReg::from_encoded(*encoded).ok_or(SnapshotError::Incompatible)?;
        target.write_sys_reg(reg, *value)?;
    }
    let normalized = if vcpu_index == 0 {
        PsciVcpuState::On
    } else {
        PsciVcpuState::Off
    };
    target.set_psci_state(normalized)?;
    Ok(())
}

/// Convenience: normalize the saved PSCI state to BSP-running, secondaries-Off
/// without writing to a target. Used by callers that want to inspect the
/// post-restore PSCI shape (e.g. `--describe-snapshot --restore-shape`).
#[must_use]
pub fn normalized_psci_state(vcpu_index: u32) -> PsciVcpuState {
    if vcpu_index == 0 {
        PsciVcpuState::On
    } else {
        PsciVcpuState::Off
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Debug, Default)]
    struct MockSource {
        mpidr: u64,
        sys: std::collections::HashMap<SysReg, u64>,
    }

    impl VcpuSnapshotSource for MockSource {
        fn mpidr(&self) -> u64 {
            self.mpidr
        }
        fn read_gp_regs(&self) -> Result<GpRegs> {
            Ok(GpRegs::default())
        }
        fn read_fp_simd(&self) -> Result<FpSimdRegs> {
            Ok(FpSimdRegs::default())
        }
        fn read_sys_reg(&self, reg: SysReg) -> Result<Option<u64>> {
            Ok(self.sys.get(&reg).copied())
        }
        fn psci_state(&self) -> PsciVcpuState {
            PsciVcpuState::On
        }
    }

    #[derive(Debug, Default)]
    struct MockTarget {
        gp: RefCell<Option<GpRegs>>,
        fp: RefCell<Option<FpSimdRegs>>,
        sys: RefCell<std::collections::HashMap<SysReg, u64>>,
        psci: RefCell<Option<PsciVcpuState>>,
    }

    impl VcpuRestoreTarget for MockTarget {
        fn write_gp_regs(&mut self, regs: &GpRegs) -> Result<()> {
            *self.gp.borrow_mut() = Some(regs.clone());
            Ok(())
        }
        fn write_fp_simd(&mut self, regs: &FpSimdRegs) -> Result<()> {
            *self.fp.borrow_mut() = Some(regs.clone());
            Ok(())
        }
        fn write_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()> {
            self.sys.borrow_mut().insert(reg, value);
            Ok(())
        }
        fn set_psci_state(&mut self, state: PsciVcpuState) -> Result<()> {
            *self.psci.borrow_mut() = Some(state);
            Ok(())
        }
    }

    #[test]
    fn test_should_round_trip_curated_sysregs_via_capture_and_restore() {
        let mut source = MockSource {
            mpidr: 0x42,
            ..Default::default()
        };
        source.sys.insert(SysReg::SctlrEl1, 0xCAFE_BEEF);
        source.sys.insert(SysReg::Ttbr0El1, 0xDEAD_BEAD);
        let state = capture_vcpu_state(&source).unwrap();
        assert_eq!(state.mpidr, 0x42);
        assert_eq!(state.sys_regs.len(), 2);

        let mut target = MockTarget::default();
        restore_vcpu_state(&mut target, &state, 0).unwrap();
        let sys = target.sys.borrow();
        assert_eq!(sys.get(&SysReg::SctlrEl1), Some(&0xCAFE_BEEF));
        assert_eq!(sys.get(&SysReg::Ttbr0El1), Some(&0xDEAD_BEAD));
    }

    #[test]
    fn test_should_skip_sys_regs_the_source_returns_none_for() {
        let source = MockSource::default(); // No sysregs populated.
        let state = capture_vcpu_state(&source).unwrap();
        assert!(state.sys_regs.is_empty());
    }

    #[test]
    fn test_should_normalize_psci_state_on_restore_for_bsp_only() {
        let mut state = VcpuState::new(0);
        state.psci_state = PsciVcpuState::Off; // saved as Off, but BSP is restored On
        let mut target = MockTarget::default();
        restore_vcpu_state(&mut target, &state, 0).unwrap();
        assert_eq!(*target.psci.borrow(), Some(PsciVcpuState::On));
    }

    #[test]
    fn test_should_normalize_psci_state_to_off_for_secondaries() {
        let mut state = VcpuState::new(1);
        state.psci_state = PsciVcpuState::On; // saved as On, but secondary is restored Off
        let mut target = MockTarget::default();
        restore_vcpu_state(&mut target, &state, 1).unwrap();
        assert_eq!(*target.psci.borrow(), Some(PsciVcpuState::Off));
    }

    #[test]
    fn test_should_reject_unknown_sys_reg_keys_on_restore() {
        let mut state = VcpuState::new(0);
        // A key beyond the curated list — emulates a future-version state file.
        state.sys_regs.insert(u64::MAX, 0x1234);
        let mut target = MockTarget::default();
        let res = restore_vcpu_state(&mut target, &state, 0);
        assert!(matches!(res, Err(SnapshotError::Incompatible)));
    }

    #[test]
    fn test_should_provide_normalized_psci_helper() {
        assert_eq!(normalized_psci_state(0), PsciVcpuState::On);
        assert_eq!(normalized_psci_state(1), PsciVcpuState::Off);
        assert_eq!(normalized_psci_state(31), PsciVcpuState::Off);
    }
}
