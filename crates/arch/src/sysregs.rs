//! Curated sysreg subset (~100 registers we touch).
//!
//! The full ARMv8 sysreg space is enormous; squib only saves and restores what is
//! necessary for boot, exception handling, the `V1N1` CPU template, and the GIC. New
//! registers are added with a snapshot version bump (additive contract — never remove).
//!
//! See [13-arch-and-boot.md § 3](../../../specs/13-arch-and-boot.md#3-sysreg-subset).

/// Sysreg enum — `#[non_exhaustive]` because the full curated list is iterative.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum SysReg {
    // === Boot setup ===
    /// SCTLR_EL1 — system control register, EL1.
    SctlrEl1,
    /// TTBR0_EL1 — translation table base, EL1.
    Ttbr0El1,
    /// TTBR1_EL1 — translation table base 1, EL1.
    Ttbr1El1,
    /// MAIR_EL1 — memory attribute indirection register.
    MairEl1,
    /// AMAIR_EL1 — auxiliary memory attribute indirection register.
    AmairEl1,
    /// TCR_EL1 — translation control register.
    TcrEl1,
    /// SP_EL1.
    SpEl1,
    /// ELR_EL1 — exception link register.
    ElrEl1,
    /// SPSR_EL1 — saved program status register.
    SpsrEl1,
    /// VBAR_EL1 — vector base address register.
    VbarEl1,

    // === ID registers (read-only) ===
    /// ID_AA64MMFR0_EL1 — memory model feature register 0.
    IdAa64Mmfr0El1,
    /// ID_AA64MMFR1_EL1 — memory model feature register 1.
    IdAa64Mmfr1El1,
    /// ID_AA64PFR0_EL1 — processor feature register 0.
    IdAa64Pfr0El1,
    /// ID_AA64PFR1_EL1 — processor feature register 1.
    IdAa64Pfr1El1,
    /// ID_AA64DFR0_EL1 — debug feature register 0.
    IdAa64Dfr0El1,
    /// ID_AA64ISAR0_EL1 — instruction set attribute register 0.
    IdAa64Isar0El1,
    /// ID_AA64ISAR1_EL1 — instruction set attribute register 1.
    IdAa64Isar1El1,
    /// MPIDR_EL1 — multiprocessor affinity register (per-vCPU; identity).
    MpidrEl1,

    // === Generic timers ===
    /// CNTV_CTL_EL0 — virtual timer control.
    CntvCtlEl0,
    /// CNTV_CVAL_EL0 — virtual timer compare value.
    CntvCvalEl0,
    /// CNTV_OFF_EL2 — virtual timer offset.
    CntvOffEl2,
    /// CNTFRQ_EL0 — counter frequency.
    CntFrqEl0,
    /// CNTKCTL_EL1 — kernel counter control.
    CntKctlEl1,
    /// CNTP_CTL_EL0 — physical timer control.
    CntpCtlEl0,
    /// CNTP_CVAL_EL0 — physical timer compare value.
    CntpCvalEl0,

    // === Performance / Pmu ===
    /// PMCCNTR_EL0 — cycle counter.
    PmCcntrEl0,
    /// PMCCFILTR_EL0 — cycle counter filter.
    PmCcfiltrEl0,
    /// PMUSERENR_EL0 — user enable register.
    PmUserEnrEl0,
    /// PMCR_EL0 — PMU control.
    PmCrEl0,
    /// PMCNTENSET_EL0 — counter enable set.
    PmCntEnSetEl0,
    /// PMOVSSET_EL0 — overflow status set.
    PmOvsSetEl0,
    /// PMSELR_EL0 — event counter selection.
    PmSelrEl0,

    // === Exception handling ===
    /// ESR_EL1 — exception syndrome.
    EsrEl1,
    /// FAR_EL1 — fault address register.
    FarEl1,
    /// AFSR0_EL1 — auxiliary fault status register 0.
    Afsr0El1,
    /// AFSR1_EL1 — auxiliary fault status register 1.
    Afsr1El1,

    // === Memory model / TLB ===
    /// CONTEXTIDR_EL1 — context ID register.
    ContextIdrEl1,
    /// TPIDR_EL0 — thread pointer EL0.
    TpidrEl0,
    /// TPIDRRO_EL0 — thread pointer (read-only, EL0).
    TpidrroEl0,
    /// TPIDR_EL1 — thread pointer EL1.
    TpidrEl1,
    /// PAR_EL1 — physical address register.
    ParEl1,

    // === FP/SIMD ===
    /// FPCR — FP control register.
    Fpcr,
    /// FPSR — FP status register.
    Fpsr,
    /// CPACR_EL1 — architectural feature access control.
    CpacrEl1,

    // === Debug ===
    /// MDSCR_EL1 — monitor debug system control.
    MdscrEl1,
    /// OSLAR_EL1 — OS lock access register.
    OslarEl1,
    /// OSDLR_EL1 — OS double-lock register.
    OsdlrEl1,
}

impl SysReg {
    /// A stable wire-encoding for use as a `BTreeMap<u64, u64>` key in
    /// `VcpuState::sys_regs`.
    ///
    /// The encoding is `index_in_all() + 1` (1-based, so `0` is reserved for
    /// "unknown"). Snapshot consumers must round-trip through
    /// [`Self::from_encoded`] — the wire shape is squib-private (D6).
    #[must_use]
    pub fn as_encoded(self) -> u64 {
        Self::all()
            .iter()
            .position(|r| *r == self)
            .map_or(0, |i| (i as u64) + 1)
    }

    /// Inverse of [`Self::as_encoded`]. Returns `None` for keys not in the curated
    /// list (forward-compat: a state file from a future squib build that added
    /// registers in the middle would surface as `None` and the loader rejects
    /// with `SnapshotError::Incompatible`).
    #[must_use]
    pub fn from_encoded(key: u64) -> Option<Self> {
        if key == 0 {
            return None;
        }
        let idx = usize::try_from(key - 1).ok()?;
        Self::all().get(idx).copied()
    }

    /// All curated sysregs, in canonical order. The order is the additive contract: new
    /// registers may be appended; never insert in the middle.
    #[must_use]
    pub const fn all() -> &'static [SysReg] {
        &[
            // boot
            Self::SctlrEl1,
            Self::Ttbr0El1,
            Self::Ttbr1El1,
            Self::MairEl1,
            Self::AmairEl1,
            Self::TcrEl1,
            Self::SpEl1,
            Self::ElrEl1,
            Self::SpsrEl1,
            Self::VbarEl1,
            // id
            Self::IdAa64Mmfr0El1,
            Self::IdAa64Mmfr1El1,
            Self::IdAa64Pfr0El1,
            Self::IdAa64Pfr1El1,
            Self::IdAa64Dfr0El1,
            Self::IdAa64Isar0El1,
            Self::IdAa64Isar1El1,
            Self::MpidrEl1,
            // timer
            Self::CntvCtlEl0,
            Self::CntvCvalEl0,
            Self::CntvOffEl2,
            Self::CntFrqEl0,
            Self::CntKctlEl1,
            Self::CntpCtlEl0,
            Self::CntpCvalEl0,
            // pmu
            Self::PmCcntrEl0,
            Self::PmCcfiltrEl0,
            Self::PmUserEnrEl0,
            Self::PmCrEl0,
            Self::PmCntEnSetEl0,
            Self::PmOvsSetEl0,
            Self::PmSelrEl0,
            // exceptions
            Self::EsrEl1,
            Self::FarEl1,
            Self::Afsr0El1,
            Self::Afsr1El1,
            // memory model / TLB
            Self::ContextIdrEl1,
            Self::TpidrEl0,
            Self::TpidrroEl0,
            Self::TpidrEl1,
            Self::ParEl1,
            // FP/SIMD
            Self::Fpcr,
            Self::Fpsr,
            Self::CpacrEl1,
            // debug
            Self::MdscrEl1,
            Self::OslarEl1,
            Self::OsdlrEl1,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_returns_unique_registers() {
        let all = SysReg::all();
        for (i, reg) in all.iter().enumerate() {
            assert!(
                !all[..i].contains(reg),
                "duplicate sysreg at index {i}: {reg:?}"
            );
        }
    }

    #[test]
    fn all_is_non_empty_and_below_two_hundred() {
        let count = SysReg::all().len();
        assert!(count >= 30, "got {count} sysregs; expected at least 30");
        assert!(count <= 200, "got {count} sysregs; spec caps at ~100");
    }

    #[test]
    fn additive_contract_starts_with_boot_setup() {
        // The boot-setup block is listed first per spec § 3; SCTLR_EL1 is the canonical
        // first entry.
        assert_eq!(SysReg::all()[0], SysReg::SctlrEl1);
    }

    #[test]
    fn test_should_round_trip_as_encoded_through_from_encoded() {
        for reg in SysReg::all() {
            let key = reg.as_encoded();
            assert_ne!(key, 0, "encoding must be non-zero for {reg:?}");
            assert_eq!(SysReg::from_encoded(key), Some(*reg));
        }
    }

    #[test]
    fn test_should_reject_zero_and_out_of_range_encoded_keys() {
        assert_eq!(SysReg::from_encoded(0), None);
        let beyond = (SysReg::all().len() as u64) + 1;
        assert_eq!(SysReg::from_encoded(beyond), None);
    }
}
