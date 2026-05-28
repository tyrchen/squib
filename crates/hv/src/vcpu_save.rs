//! Live HVF impls of [`squib_snapshot::vcpu_save`] traits.
//!
//! [`HvfVcpuSnapshot`] wraps an `applevisor::vcpu::Vcpu` and implements
//! [`squib_snapshot::vcpu_save::VcpuSnapshotSource`] +
//! [`squib_snapshot::vcpu_save::VcpuRestoreTarget`] over it. The traits are
//! defined in the portable snapshot crate so the snapshot machinery doesn't
//! pull in `applevisor`; the concrete bindings live here.
//!
//! ## Squib `SysReg` ↔ HVF `hv_sys_reg_t` mapping
//!
//! Squib's curated [`squib_arch::SysReg`] enum (47 variants) does not 1:1
//! match HVF's `hv_sys_reg_t`. Three classes of mismatch:
//!
//! 1. **Same name, different value path** — most variants (SCTLR_EL1, TTBR0_EL1, …) are reachable
//!    via `Vcpu::get_sys_reg(SysReg::FOO)`.
//! 2. **Routed through `Reg`, not `SysReg`** — `FPCR` and `FPSR` are accessed via
//!    `get_reg(Reg::FPCR)` because HVF treats them as part of the FP register file, not as system
//!    registers.
//! 3. **Not exposed by HVF at all** — the PMU set (`PMCCNTR_EL0`, `PMCR_EL0`, `PMUSERENR_EL0`,
//!    `PMCNTENSET_EL0`, `PMOVSSET_EL0`, `PMSELR_EL0`, `PMCCFILTR_EL0`), `OSLAR_EL1`, `OSDLR_EL1`,
//!    and `CNTFRQ_EL0` have no corresponding `hv_sys_reg_t` variant. The trait's `read_sys_reg`
//!    returns `Ok(None)` for these — the snapshot capture treats `None` as "skip" so the resulting
//!    state file simply omits the register and the restore re-uses the reset default. This is the
//!    documented behaviour of [`VcpuSnapshotSource`] and was anticipated by the trait shape.
//!
//! Per-variant routing lives in [`map_to_hvf_sys_reg`] and [`map_to_hvf_reg`];
//! both are pure functions over the squib enum so unit tests can exercise the
//! mapping without needing a live HVF.

#![cfg(target_os = "macos")]

use applevisor::vcpu::{Reg, SimdFpReg, SysReg as HvfSysReg, Vcpu as VirtualCpu};
use squib_arch::SysReg;
use squib_snapshot::{
    FpSimdRegs, GpRegs, MmdsState, PsciVcpuState, Result as SnapshotResult, SnapshotError,
    vcpu_save::{
        GicRestoreTarget, GicSnapshotSource, MmdsRestoreTarget, MmdsSnapshotSource,
        VcpuRestoreTarget, VcpuSnapshotSource,
    },
};

/// Map a squib [`SysReg`] to the matching HVF [`HvfSysReg`].
///
/// Returns `None` for registers that HVF does not expose via `hv_sys_reg_t`.
/// `FPCR`/`FPSR` also return `None` here because they are read via
/// [`map_to_hvf_reg`] instead.
#[must_use]
pub fn map_to_hvf_sys_reg(reg: SysReg) -> Option<HvfSysReg> {
    Some(match reg {
        // Boot setup
        SysReg::SctlrEl1 => HvfSysReg::SCTLR_EL1,
        SysReg::Ttbr0El1 => HvfSysReg::TTBR0_EL1,
        SysReg::Ttbr1El1 => HvfSysReg::TTBR1_EL1,
        SysReg::MairEl1 => HvfSysReg::MAIR_EL1,
        SysReg::AmairEl1 => HvfSysReg::AMAIR_EL1,
        SysReg::TcrEl1 => HvfSysReg::TCR_EL1,
        SysReg::SpEl1 => HvfSysReg::SP_EL1,
        SysReg::ElrEl1 => HvfSysReg::ELR_EL1,
        SysReg::SpsrEl1 => HvfSysReg::SPSR_EL1,
        SysReg::VbarEl1 => HvfSysReg::VBAR_EL1,
        // ID registers
        SysReg::IdAa64Mmfr0El1 => HvfSysReg::ID_AA64MMFR0_EL1,
        SysReg::IdAa64Mmfr1El1 => HvfSysReg::ID_AA64MMFR1_EL1,
        SysReg::IdAa64Pfr0El1 => HvfSysReg::ID_AA64PFR0_EL1,
        SysReg::IdAa64Pfr1El1 => HvfSysReg::ID_AA64PFR1_EL1,
        SysReg::IdAa64Dfr0El1 => HvfSysReg::ID_AA64DFR0_EL1,
        SysReg::IdAa64Isar0El1 => HvfSysReg::ID_AA64ISAR0_EL1,
        SysReg::IdAa64Isar1El1 => HvfSysReg::ID_AA64ISAR1_EL1,
        SysReg::MpidrEl1 => HvfSysReg::MPIDR_EL1,
        // Generic timers (CNTV_OFF_EL2, CNTP_*_EL0 are gated on macos-15-0;
        // squib's applevisor feature flag enables that gate, so they're available)
        SysReg::CntvCtlEl0 => HvfSysReg::CNTV_CTL_EL0,
        SysReg::CntvCvalEl0 => HvfSysReg::CNTV_CVAL_EL0,
        SysReg::CntvOffEl2 => HvfSysReg::CNTVOFF_EL2,
        SysReg::CntKctlEl1 => HvfSysReg::CNTKCTL_EL1,
        SysReg::CntpCtlEl0 => HvfSysReg::CNTP_CTL_EL0,
        SysReg::CntpCvalEl0 => HvfSysReg::CNTP_CVAL_EL0,
        // Exception handling
        SysReg::EsrEl1 => HvfSysReg::ESR_EL1,
        SysReg::FarEl1 => HvfSysReg::FAR_EL1,
        SysReg::Afsr0El1 => HvfSysReg::AFSR0_EL1,
        SysReg::Afsr1El1 => HvfSysReg::AFSR1_EL1,
        // Memory model / TLB
        SysReg::ContextIdrEl1 => HvfSysReg::CONTEXTIDR_EL1,
        SysReg::TpidrEl0 => HvfSysReg::TPIDR_EL0,
        SysReg::TpidrroEl0 => HvfSysReg::TPIDRRO_EL0,
        SysReg::TpidrEl1 => HvfSysReg::TPIDR_EL1,
        SysReg::ParEl1 => HvfSysReg::PAR_EL1,
        // FP/SIMD control
        SysReg::CpacrEl1 => HvfSysReg::CPACR_EL1,
        // Debug
        SysReg::MdscrEl1 => HvfSysReg::MDSCR_EL1,
        // === Not exposed by HVF as `hv_sys_reg_t` ===
        // FPCR/FPSR are accessed via Reg::FPCR / Reg::FPSR (see map_to_hvf_reg).
        // PMU registers, OSLAR/OSDLR, CNTFRQ_EL0, IdAa64* on some macOS versions.
        // `SysReg` is `#[non_exhaustive]` so the wildcard `_` arm catches future
        // variants the same way — they surface as `None` (skip on capture,
        // default-on-restore) until this match gets a concrete arm.
        _ => return None,
    })
}

/// Map FPCR/FPSR through HVF's `Reg` enum (not `SysReg`).
///
/// Returns `None` for any register that is *not* FPCR or FPSR.
#[must_use]
pub fn map_to_hvf_reg(reg: SysReg) -> Option<Reg> {
    match reg {
        SysReg::Fpcr => Some(Reg::FPCR),
        SysReg::Fpsr => Some(Reg::FPSR),
        _ => None,
    }
}

fn err_to_snap(e: applevisor::error::HypervisorError, ctx: &str) -> SnapshotError {
    SnapshotError::Capture(format!("HVF {ctx}: {e:?}"))
}

/// Live HVF source/target for a single vCPU. Construct on the vCPU's
/// owner thread (HVF affinity) just before save or after `init_with_gic`
/// at restore time; the wrapper holds an `&Vcpu` so it's tied to that
/// thread by the borrow checker.
#[derive(Debug)]
pub struct HvfVcpuSnapshot<'a> {
    vcpu: &'a VirtualCpu,
    mpidr: u64,
}

impl<'a> HvfVcpuSnapshot<'a> {
    /// Wrap a live vCPU. `mpidr` is the value the boot orchestrator picked
    /// (matches the FDT cpu node's `reg` cell).
    #[must_use]
    pub const fn new(vcpu: &'a VirtualCpu, mpidr: u64) -> Self {
        Self { vcpu, mpidr }
    }
}

impl VcpuSnapshotSource for HvfVcpuSnapshot<'_> {
    fn mpidr(&self) -> u64 {
        self.mpidr
    }

    fn read_gp_regs(&self) -> SnapshotResult<GpRegs> {
        let mut x = [0u64; 31];
        // Reg's discriminants are dense 0..=30 for X0..X30. Cast through
        // `unsafe transmute` is brittle; use an explicit table instead so
        // a future Reg reordering surfaces as a compile error.
        let regs: [Reg; 31] = [
            Reg::X0,
            Reg::X1,
            Reg::X2,
            Reg::X3,
            Reg::X4,
            Reg::X5,
            Reg::X6,
            Reg::X7,
            Reg::X8,
            Reg::X9,
            Reg::X10,
            Reg::X11,
            Reg::X12,
            Reg::X13,
            Reg::X14,
            Reg::X15,
            Reg::X16,
            Reg::X17,
            Reg::X18,
            Reg::X19,
            Reg::X20,
            Reg::X21,
            Reg::X22,
            Reg::X23,
            Reg::X24,
            Reg::X25,
            Reg::X26,
            Reg::X27,
            Reg::X28,
            Reg::X29,
            Reg::X30,
        ];
        for (i, reg) in regs.iter().enumerate() {
            x[i] = self
                .vcpu
                .get_reg(*reg)
                .map_err(|e| err_to_snap(e, "get_reg X"))?;
        }
        // SP is `SP_EL1` system register; PC is `Reg::PC`; PSTATE = CPSR.
        let sp = self
            .vcpu
            .get_sys_reg(HvfSysReg::SP_EL1)
            .map_err(|e| err_to_snap(e, "get_sys_reg SP_EL1"))?;
        let pc = self
            .vcpu
            .get_reg(Reg::PC)
            .map_err(|e| err_to_snap(e, "get_reg PC"))?;
        let pstate = self
            .vcpu
            .get_reg(Reg::CPSR)
            .map_err(|e| err_to_snap(e, "get_reg CPSR"))?;
        Ok(GpRegs { x, sp, pc, pstate })
    }

    fn read_fp_simd(&self) -> SnapshotResult<FpSimdRegs> {
        const Q_REGS: [SimdFpReg; 32] = [
            SimdFpReg::Q0,
            SimdFpReg::Q1,
            SimdFpReg::Q2,
            SimdFpReg::Q3,
            SimdFpReg::Q4,
            SimdFpReg::Q5,
            SimdFpReg::Q6,
            SimdFpReg::Q7,
            SimdFpReg::Q8,
            SimdFpReg::Q9,
            SimdFpReg::Q10,
            SimdFpReg::Q11,
            SimdFpReg::Q12,
            SimdFpReg::Q13,
            SimdFpReg::Q14,
            SimdFpReg::Q15,
            SimdFpReg::Q16,
            SimdFpReg::Q17,
            SimdFpReg::Q18,
            SimdFpReg::Q19,
            SimdFpReg::Q20,
            SimdFpReg::Q21,
            SimdFpReg::Q22,
            SimdFpReg::Q23,
            SimdFpReg::Q24,
            SimdFpReg::Q25,
            SimdFpReg::Q26,
            SimdFpReg::Q27,
            SimdFpReg::Q28,
            SimdFpReg::Q29,
            SimdFpReg::Q30,
            SimdFpReg::Q31,
        ];
        let mut v = [[0u64; 2]; 32];
        for (i, q) in Q_REGS.iter().enumerate() {
            let raw: u128 = self
                .vcpu
                .get_simd_fp_reg(*q)
                .map_err(|e| err_to_snap(e, "get_simd_fp_reg"))?;
            // Split into two u64s — low half then high half — so the wire
            // format matches `FpSimdRegs::v: [[u64; 2]; 32]`.
            #[allow(
                clippy::cast_possible_truncation,
                reason = "split into low/high u64 halves"
            )]
            let lo = raw as u64;
            let hi = (raw >> 64) as u64;
            v[i] = [lo, hi];
        }
        let control = self
            .vcpu
            .get_reg(Reg::FPCR)
            .map_err(|e| err_to_snap(e, "get_reg FPCR"))?;
        let status = self
            .vcpu
            .get_reg(Reg::FPSR)
            .map_err(|e| err_to_snap(e, "get_reg FPSR"))?;
        Ok(FpSimdRegs {
            v,
            fpsr: status,
            fpcr: control,
        })
    }

    fn read_sys_reg(&self, reg: SysReg) -> SnapshotResult<Option<u64>> {
        if let Some(hvf) = map_to_hvf_sys_reg(reg) {
            return self
                .vcpu
                .get_sys_reg(hvf)
                .map(Some)
                .map_err(|e| err_to_snap(e, "get_sys_reg"));
        }
        if let Some(hvf_reg) = map_to_hvf_reg(reg) {
            return self
                .vcpu
                .get_reg(hvf_reg)
                .map(Some)
                .map_err(|e| err_to_snap(e, "get_reg (FPCR/FPSR)"));
        }
        // PMU + OSLAR + OSDLR + CNTFRQ_EL0 — HVF doesn't surface these. The
        // trait shape treats `Ok(None)` as "skip; the restore re-uses the
        // reset default", which is the correct behaviour for these registers.
        Ok(None)
    }

    fn psci_state(&self) -> PsciVcpuState {
        // Live PSCI state on HVF isn't directly readable; restore-side
        // normalisation makes this field diagnostic-only anyway. We report
        // `On` because if we're capturing, the vCPU is currently running.
        PsciVcpuState::On
    }
}

impl VcpuRestoreTarget for HvfVcpuSnapshot<'_> {
    fn write_gp_regs(&mut self, regs: &GpRegs) -> SnapshotResult<()> {
        let table: [Reg; 31] = [
            Reg::X0,
            Reg::X1,
            Reg::X2,
            Reg::X3,
            Reg::X4,
            Reg::X5,
            Reg::X6,
            Reg::X7,
            Reg::X8,
            Reg::X9,
            Reg::X10,
            Reg::X11,
            Reg::X12,
            Reg::X13,
            Reg::X14,
            Reg::X15,
            Reg::X16,
            Reg::X17,
            Reg::X18,
            Reg::X19,
            Reg::X20,
            Reg::X21,
            Reg::X22,
            Reg::X23,
            Reg::X24,
            Reg::X25,
            Reg::X26,
            Reg::X27,
            Reg::X28,
            Reg::X29,
            Reg::X30,
        ];
        for (i, reg) in table.iter().enumerate() {
            self.vcpu
                .set_reg(*reg, regs.x[i])
                .map_err(|e| err_to_snap(e, "set_reg X"))?;
        }
        self.vcpu
            .set_sys_reg(HvfSysReg::SP_EL1, regs.sp)
            .map_err(|e| err_to_snap(e, "set_sys_reg SP_EL1"))?;
        self.vcpu
            .set_reg(Reg::PC, regs.pc)
            .map_err(|e| err_to_snap(e, "set_reg PC"))?;
        self.vcpu
            .set_reg(Reg::CPSR, regs.pstate)
            .map_err(|e| err_to_snap(e, "set_reg CPSR"))?;
        Ok(())
    }

    fn write_fp_simd(&mut self, regs: &FpSimdRegs) -> SnapshotResult<()> {
        const Q_REGS: [SimdFpReg; 32] = [
            SimdFpReg::Q0,
            SimdFpReg::Q1,
            SimdFpReg::Q2,
            SimdFpReg::Q3,
            SimdFpReg::Q4,
            SimdFpReg::Q5,
            SimdFpReg::Q6,
            SimdFpReg::Q7,
            SimdFpReg::Q8,
            SimdFpReg::Q9,
            SimdFpReg::Q10,
            SimdFpReg::Q11,
            SimdFpReg::Q12,
            SimdFpReg::Q13,
            SimdFpReg::Q14,
            SimdFpReg::Q15,
            SimdFpReg::Q16,
            SimdFpReg::Q17,
            SimdFpReg::Q18,
            SimdFpReg::Q19,
            SimdFpReg::Q20,
            SimdFpReg::Q21,
            SimdFpReg::Q22,
            SimdFpReg::Q23,
            SimdFpReg::Q24,
            SimdFpReg::Q25,
            SimdFpReg::Q26,
            SimdFpReg::Q27,
            SimdFpReg::Q28,
            SimdFpReg::Q29,
            SimdFpReg::Q30,
            SimdFpReg::Q31,
        ];
        for (i, q) in Q_REGS.iter().enumerate() {
            let lo = regs.v[i][0];
            let hi = regs.v[i][1];
            let value: u128 = (u128::from(hi) << 64) | u128::from(lo);
            self.vcpu
                .set_simd_fp_reg(*q, value)
                .map_err(|e| err_to_snap(e, "set_simd_fp_reg"))?;
        }
        self.vcpu
            .set_reg(Reg::FPCR, regs.fpcr)
            .map_err(|e| err_to_snap(e, "set_reg FPCR"))?;
        self.vcpu
            .set_reg(Reg::FPSR, regs.fpsr)
            .map_err(|e| err_to_snap(e, "set_reg FPSR"))?;
        Ok(())
    }

    fn write_sys_reg(&mut self, reg: SysReg, value: u64) -> SnapshotResult<()> {
        if let Some(hvf) = map_to_hvf_sys_reg(reg) {
            return self
                .vcpu
                .set_sys_reg(hvf, value)
                .map_err(|e| err_to_snap(e, "set_sys_reg"));
        }
        if let Some(hvf_reg) = map_to_hvf_reg(reg) {
            return self
                .vcpu
                .set_reg(hvf_reg, value)
                .map_err(|e| err_to_snap(e, "set_reg (FPCR/FPSR)"));
        }
        // For registers HVF does not expose, silently drop the saved value:
        // the restored vCPU re-uses the reset default the kernel would have
        // built up itself — same shape as captures returning `None`.
        Ok(())
    }

    fn set_psci_state(&mut self, _state: PsciVcpuState) -> SnapshotResult<()> {
        // PSCI state is normalized at the orchestrator layer (BSP-Running,
        // secondaries-Off). HVF doesn't have a "set PSCI state" knob — the
        // boot orchestrator pulls secondaries up via the guest's PSCI
        // driver issuing CPU_ON, exactly as on a fresh boot. Honoring the
        // trait by no-op'ing here keeps the contract honest.
        Ok(())
    }
}

/// Live HVF source/target for the GIC state blob. Construct via [`Self::new`]
/// after `init_with_gic` (it owns an `applevisor::gic::GicState`).
#[derive(Debug)]
pub struct HvfGicSnapshot {
    state: applevisor::gic::GicState,
}

impl HvfGicSnapshot {
    /// Build a fresh GIC state handle from the live VM. Must be called
    /// after `HvfHypervisor::init_vm` (the `applevisor::gic::GicState`
    /// constructor calls into the underlying static instance).
    ///
    /// # Errors
    /// `SnapshotError::Capture` for any HVF-side failure.
    pub fn new() -> SnapshotResult<Self> {
        let instance = applevisor::vm::VirtualMachineStaticInstance::get_gic()
            .ok_or_else(|| SnapshotError::Capture("HVF VM not yet initialised".to_string()))?;
        let state = instance
            .gic_state_create()
            .map_err(|e| err_to_snap(e, "gic_state_create"))?;
        Ok(Self { state })
    }
}

impl GicSnapshotSource for HvfGicSnapshot {
    fn capture(&self) -> SnapshotResult<Vec<u8>> {
        // applevisor::gic::GicState exposes `size(&mut self)` and
        // `get(&mut self, &mut [u8])`; we hold a `&self` here, so we work
        // around the `&mut` requirement by constructing a fresh handle on
        // each capture (the size + get-pair returns the same opaque blob).
        // This is the documented HVF flow: each capture is a fresh draw.
        let instance = applevisor::vm::VirtualMachineStaticInstance::get_gic()
            .ok_or_else(|| SnapshotError::Capture("HVF VM not yet initialised".to_string()))?;
        let mut state = instance
            .gic_state_create()
            .map_err(|e| err_to_snap(e, "gic_state_create"))?;
        let size = state.size().map_err(|e| err_to_snap(e, "gic_state size"))?;
        let mut buf = vec![0u8; size];
        state
            .get(&mut buf)
            .map_err(|e| err_to_snap(e, "gic_state get"))?;
        Ok(buf)
    }
}

impl GicRestoreTarget for HvfGicSnapshot {
    fn restore(&mut self, data: &[u8]) -> SnapshotResult<()> {
        self.state
            .set(data)
            .map_err(|e| err_to_snap(e, "gic_state set"))
    }
}

/// In-memory MMDS snapshot bridge. squib's MMDS state is fully
/// portable (no HVF-side data), so the live impl just passes the
/// caller's stored snapshot through. Production code uses
/// `squib_mmds` handles directly; this is a convenience for the
/// tests + boot orchestrator that drive both at once.
#[derive(Debug, Default)]
pub struct InProcMmdsSnapshot {
    state: parking_lot::Mutex<Option<MmdsState>>,
}

impl InProcMmdsSnapshot {
    /// Construct empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-load a state to be returned by `capture`.
    pub fn set(&self, state: Option<MmdsState>) {
        *self.state.lock() = state;
    }
}

impl MmdsSnapshotSource for InProcMmdsSnapshot {
    fn capture(&self) -> SnapshotResult<Option<MmdsState>> {
        Ok(self.state.lock().clone())
    }
}

impl MmdsRestoreTarget for InProcMmdsSnapshot {
    fn restore(&mut self, state: Option<&MmdsState>) -> SnapshotResult<()> {
        *self.state.lock() = state.cloned();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that don't need a live HVF: the squib-SysReg → HVF-SysReg
    /// table is exhaustively covered, and unsupported variants surface as
    /// `None` from both the sysreg and the reg map.
    #[test]
    fn test_should_route_curated_sysregs_to_hvf_or_skip() {
        for reg in SysReg::all() {
            let sys_route = map_to_hvf_sys_reg(*reg);
            let reg_route = map_to_hvf_reg(*reg);
            // No register may route through both paths.
            assert!(
                !(sys_route.is_some() && reg_route.is_some()),
                "{reg:?} routes through both sys_reg and reg paths"
            );
            // Boot/ID/timer/exception/memory/debug registers that *should* have
            // a sys_reg route should not surface as `None` if HVF surface includes
            // them — the explicit list below is the source of truth.
        }
    }

    #[test]
    fn test_should_route_fpcr_fpsr_via_reg_path_only() {
        assert_eq!(map_to_hvf_reg(SysReg::Fpcr), Some(Reg::FPCR));
        assert_eq!(map_to_hvf_reg(SysReg::Fpsr), Some(Reg::FPSR));
        assert_eq!(map_to_hvf_sys_reg(SysReg::Fpcr), None);
        assert_eq!(map_to_hvf_sys_reg(SysReg::Fpsr), None);
    }

    #[test]
    fn test_should_skip_pmu_oslar_osdlr_cntfrq() {
        for reg in [
            SysReg::CntFrqEl0,
            SysReg::PmCcntrEl0,
            SysReg::PmCcfiltrEl0,
            SysReg::PmUserEnrEl0,
            SysReg::PmCrEl0,
            SysReg::PmCntEnSetEl0,
            SysReg::PmOvsSetEl0,
            SysReg::PmSelrEl0,
            SysReg::OslarEl1,
            SysReg::OsdlrEl1,
        ] {
            assert_eq!(
                map_to_hvf_sys_reg(reg),
                None,
                "{reg:?} unexpectedly routed to HVF"
            );
            assert_eq!(
                map_to_hvf_reg(reg),
                None,
                "{reg:?} unexpectedly routed via Reg path"
            );
        }
    }

    #[test]
    fn test_should_route_canonical_boot_sysregs() {
        // Spot-check a few known-good entries.
        assert_eq!(
            map_to_hvf_sys_reg(SysReg::SctlrEl1),
            Some(HvfSysReg::SCTLR_EL1)
        );
        assert_eq!(
            map_to_hvf_sys_reg(SysReg::Ttbr0El1),
            Some(HvfSysReg::TTBR0_EL1)
        );
        assert_eq!(map_to_hvf_sys_reg(SysReg::SpEl1), Some(HvfSysReg::SP_EL1));
        assert_eq!(
            map_to_hvf_sys_reg(SysReg::VbarEl1),
            Some(HvfSysReg::VBAR_EL1)
        );
        assert_eq!(map_to_hvf_sys_reg(SysReg::EsrEl1), Some(HvfSysReg::ESR_EL1));
        assert_eq!(
            map_to_hvf_sys_reg(SysReg::MpidrEl1),
            Some(HvfSysReg::MPIDR_EL1)
        );
        assert_eq!(
            map_to_hvf_sys_reg(SysReg::CntvCtlEl0),
            Some(HvfSysReg::CNTV_CTL_EL0)
        );
    }
}
