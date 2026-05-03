//! PSCI 1.1 dispatch table.
//!
//! Squib's HVC handler reads X0 (function ID) and dispatches against this table. Unknown
//! function IDs return `PSCI_NOT_SUPPORTED` — never panic, never abort. SMC traps follow
//! the same path: HVF surfaces `EC_SMC` (`0x17`), the VMM places `PSCI_NOT_SUPPORTED` in
//! X0 and advances PC by 4. This matches what KVM does when no PSCI conduit is registered
//! for SMC and what mainline Linux expects when the FDT declares `psci.method = "hvc"`.
//!
//! See [13-arch-and-boot.md § 5](../../../specs/13-arch-and-boot.md#5-psci-dispatch).

/// `PSCI_VERSION` function ID (PSCI 0.2, smc32).
pub const PSCI_VERSION: u32 = 0x8400_0000;
/// `CPU_OFF` function ID (smc32).
pub const CPU_OFF: u32 = 0x8400_0002;
/// `CPU_ON` function ID (smc64).
pub const CPU_ON: u32 = 0xC400_0003;
/// `AFFINITY_INFO` function ID (smc64).
pub const AFFINITY_INFO: u32 = 0xC400_0004;
/// `MIGRATE_INFO_TYPE` function ID (smc32).
pub const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
/// `SYSTEM_OFF` function ID (smc32).
pub const SYSTEM_OFF: u32 = 0x8400_0008;
/// `SYSTEM_RESET` function ID (smc32).
pub const SYSTEM_RESET: u32 = 0x8400_0009;
/// `PSCI_FEATURES` function ID (smc32).
pub const PSCI_FEATURES: u32 = 0x8400_000A;
/// `CPU_SUSPEND` function ID (smc64) — declared in FDT but currently routed to NOT_SUPPORTED.
pub const CPU_SUSPEND: u32 = 0xC400_0001;
/// `MIGRATE` function ID (smc64) — declared in FDT but routed to NOT_SUPPORTED.
pub const MIGRATE: u32 = 0xC400_0005;

/// PSCI return code: success.
pub const PSCI_RET_SUCCESS: i32 = 0;
/// PSCI return code: NOT_SUPPORTED.
pub const PSCI_RET_NOT_SUPPORTED: i32 = -1;
/// PSCI return code: INVALID_PARAMETERS.
pub const PSCI_RET_INVALID_PARAMETERS: i32 = -2;
/// PSCI return code: ALREADY_ON.
pub const PSCI_RET_ALREADY_ON: i32 = -4;
/// PSCI return code: ON_PENDING.
pub const PSCI_RET_ON_PENDING: i32 = -5;
/// PSCI return code: INTERNAL_FAILURE.
pub const PSCI_RET_INTERNAL_FAILURE: i32 = -6;

/// PSCI 1.1 version word — `(major << 16) | minor`.
pub const PSCI_VERSION_VALUE: u32 = 0x0001_0001;

/// Affinity-info return values.
pub mod affinity {
    /// Target CPU is on.
    pub const ON: i32 = 0;
    /// Target CPU is off.
    pub const OFF: i32 = 1;
    /// Target CPU is on but a wake-up is pending (race window between `CPU_ON` and the
    /// secondary actually starting).
    pub const ON_PENDING: i32 = 2;
}

/// Recognized PSCI function names — the dispatcher emits one of these for every known ID.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum PsciFunction {
    /// `PSCI_VERSION` — return the PSCI version (1.1).
    Version,
    /// `CPU_OFF` — park the calling vCPU.
    CpuOff,
    /// `CPU_ON` — bring up a secondary vCPU.
    CpuOn {
        /// Target CPU MPIDR.
        target_cpu: u64,
        /// Entry point address for the secondary.
        entry_point: u64,
        /// Context ID passed back in X0 to the secondary.
        context_id: u64,
    },
    /// `AFFINITY_INFO` — query a vCPU's PSCI state.
    AffinityInfo {
        /// Target CPU MPIDR.
        target_cpu: u64,
        /// Affinity level (0..=3).
        lowest_affinity_level: u32,
    },
    /// `MIGRATE_INFO_TYPE` — squib always returns `2` (TOS not present).
    MigrateInfoType,
    /// `SYSTEM_OFF` — guest powered down.
    SystemOff,
    /// `SYSTEM_RESET` — guest requested reset.
    SystemReset,
    /// `PSCI_FEATURES` — query whether a PSCI function is implemented.
    Features {
        /// The function ID being queried.
        target_fn: u32,
    },
}

/// What the dispatcher tells the caller to do after decoding the HVC.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PsciOutcome {
    /// Direct return: place [`PsciReturn`] in X0 and advance PC by 4.
    Return(PsciReturn),
    /// `CPU_OFF`: park the calling vCPU; never returns to it.
    ParkCallerCpuOff,
    /// `CPU_ON`: signal a secondary vCPU actor with the given (target_cpu, entry, context).
    /// X0 of the calling vCPU receives [`PsciReturn::Success`] (or `AlreadyOn`/`OnPending`
    /// after the actor responds — the caller hands the actor the request and parks until
    /// it acks).
    BringUpSecondary {
        /// MPIDR of the target.
        target_cpu: u64,
        /// Entry point.
        entry_point: u64,
        /// Context ID passed in X0 of the secondary on first run.
        context_id: u64,
    },
    /// `AFFINITY_INFO`: caller computes the actor's state and returns it via
    /// [`PsciReturn::Word`]. The dispatcher emits this outcome with the parsed args; the
    /// VMM does the lookup.
    QueryAffinityInfo {
        /// Target MPIDR.
        target_cpu: u64,
        /// Lowest-affinity-level argument from PSCI.
        lowest_affinity_level: u32,
    },
    /// `SYSTEM_OFF`: tear down the VM cleanly; the call does not return.
    SystemOff,
    /// `SYSTEM_RESET`: tear down + recreate.
    SystemReset,
}

/// Return value placed in X0 after a PSCI call.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PsciReturn {
    /// `PSCI_RET_SUCCESS`.
    Success,
    /// `PSCI_RET_NOT_SUPPORTED`.
    NotSupported,
    /// `PSCI_RET_INVALID_PARAMETERS`.
    InvalidParameters,
    /// Numeric word — used by `AFFINITY_INFO` (0/1/2), `PSCI_VERSION`, and `MIGRATE_INFO_TYPE`.
    Word(u32),
}

impl PsciReturn {
    /// The 64-bit value placed in X0.
    #[must_use]
    #[allow(clippy::cast_sign_loss)] // PSCI return codes are signed; X0 carries the bit pattern
    pub const fn as_x0(self) -> u64 {
        match self {
            Self::Success => PSCI_RET_SUCCESS as u64,
            Self::NotSupported => PSCI_RET_NOT_SUPPORTED as u64,
            Self::InvalidParameters => PSCI_RET_INVALID_PARAMETERS as u64,
            Self::Word(w) => w as u64,
        }
    }
}

/// Decode a PSCI call into a structured outcome.
///
/// `func_id` is X0 truncated to 32 bits (PSCI is a 32-bit function-ID protocol carried in
/// X0, even on smc64). `args` is `[X1, X2, X3]` — the additional argument registers.
///
/// Unknown function IDs always return `PsciOutcome::Return(PsciReturn::NotSupported)`.
/// This matches Arm's PSCI 1.1 specification and is the contract every Linux kernel
/// expects.
#[must_use]
pub fn dispatch(func_id: u32, args: [u64; 3]) -> PsciOutcome {
    match func_id {
        PSCI_VERSION => PsciOutcome::Return(PsciReturn::Word(PSCI_VERSION_VALUE)),
        CPU_OFF => PsciOutcome::ParkCallerCpuOff,
        CPU_ON => PsciOutcome::BringUpSecondary {
            target_cpu: args[0],
            entry_point: args[1],
            context_id: args[2],
        },
        AFFINITY_INFO => PsciOutcome::QueryAffinityInfo {
            target_cpu: args[0],
            #[allow(clippy::cast_possible_truncation)] // PSCI defines this as a 32-bit field
            lowest_affinity_level: args[1] as u32,
        },
        MIGRATE_INFO_TYPE => PsciOutcome::Return(PsciReturn::Word(2)),
        SYSTEM_OFF => PsciOutcome::SystemOff,
        SYSTEM_RESET => PsciOutcome::SystemReset,
        PSCI_FEATURES => {
            // Bit 0 of args[0] is the function ID being queried.
            #[allow(clippy::cast_possible_truncation)] // PSCI function IDs are u32
            let queried = args[0] as u32;
            let supported = matches!(
                queried,
                PSCI_VERSION
                    | CPU_OFF
                    | CPU_ON
                    | AFFINITY_INFO
                    | MIGRATE_INFO_TYPE
                    | SYSTEM_OFF
                    | SYSTEM_RESET
                    | PSCI_FEATURES
            );
            if supported {
                PsciOutcome::Return(PsciReturn::Success)
            } else {
                PsciOutcome::Return(PsciReturn::NotSupported)
            }
        }
        _ => PsciOutcome::Return(PsciReturn::NotSupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psci_version_returns_one_one() {
        let outcome = dispatch(PSCI_VERSION, [0; 3]);
        assert_eq!(
            outcome,
            PsciOutcome::Return(PsciReturn::Word(PSCI_VERSION_VALUE))
        );
        assert_eq!(PsciReturn::Word(PSCI_VERSION_VALUE).as_x0(), 0x0001_0001);
    }

    #[test]
    fn cpu_off_parks_caller() {
        assert_eq!(dispatch(CPU_OFF, [0; 3]), PsciOutcome::ParkCallerCpuOff);
    }

    #[test]
    fn cpu_on_carries_target_entry_context() {
        let outcome = dispatch(CPU_ON, [0xAABB_CCDD, 0x8000_0000, 0xDEAD_BEEF]);
        assert_eq!(
            outcome,
            PsciOutcome::BringUpSecondary {
                target_cpu: 0xAABB_CCDD,
                entry_point: 0x8000_0000,
                context_id: 0xDEAD_BEEF,
            }
        );
    }

    #[test]
    fn affinity_info_passes_through_args() {
        let outcome = dispatch(AFFINITY_INFO, [0x100, 1, 0]);
        assert_eq!(
            outcome,
            PsciOutcome::QueryAffinityInfo {
                target_cpu: 0x100,
                lowest_affinity_level: 1,
            }
        );
    }

    #[test]
    fn migrate_info_type_returns_2() {
        assert_eq!(
            dispatch(MIGRATE_INFO_TYPE, [0; 3]),
            PsciOutcome::Return(PsciReturn::Word(2))
        );
    }

    #[test]
    fn system_off_and_reset_route_correctly() {
        assert_eq!(dispatch(SYSTEM_OFF, [0; 3]), PsciOutcome::SystemOff);
        assert_eq!(dispatch(SYSTEM_RESET, [0; 3]), PsciOutcome::SystemReset);
    }

    #[test]
    fn psci_features_says_yes_for_implemented_calls() {
        assert_eq!(
            dispatch(PSCI_FEATURES, [u64::from(PSCI_VERSION), 0, 0]),
            PsciOutcome::Return(PsciReturn::Success)
        );
        assert_eq!(
            dispatch(PSCI_FEATURES, [u64::from(SYSTEM_OFF), 0, 0]),
            PsciOutcome::Return(PsciReturn::Success)
        );
    }

    #[test]
    fn psci_features_says_no_for_cpu_suspend() {
        // CPU_SUSPEND is in the FDT but currently routed to NOT_SUPPORTED.
        assert_eq!(
            dispatch(PSCI_FEATURES, [u64::from(CPU_SUSPEND), 0, 0]),
            PsciOutcome::Return(PsciReturn::NotSupported)
        );
    }

    #[test]
    fn unknown_function_id_returns_not_supported() {
        for func_id in [0x0000_0000, 0x8400_00FF, 0xC400_00FF, 0xDEAD_BEEF, u32::MAX] {
            let outcome = dispatch(func_id, [0; 3]);
            assert_eq!(
                outcome,
                PsciOutcome::Return(PsciReturn::NotSupported),
                "func_id {func_id:#010x}"
            );
        }
    }

    #[test]
    fn cpu_suspend_and_migrate_route_to_not_supported_in_smc_view() {
        // Phase 1 routes both to NOT_SUPPORTED. CPU_SUSPEND lights up later when we
        // implement PSCI suspend semantics.
        assert_eq!(
            dispatch(CPU_SUSPEND, [0; 3]),
            PsciOutcome::Return(PsciReturn::NotSupported)
        );
        assert_eq!(
            dispatch(MIGRATE, [0; 3]),
            PsciOutcome::Return(PsciReturn::NotSupported)
        );
    }

    #[test]
    fn psci_return_x0_bit_pattern_matches_signed_constants() {
        // PSCI_RET_NOT_SUPPORTED = -1 in i32 → 0xFFFF_FFFF when stored in u32, then
        // sign-extended to 0xFFFF_FFFF_FFFF_FFFF when promoted to u64. Linux reads X0
        // as a signed i32, so the upper 32 bits don't matter — but the bit pattern of
        // the low 32 must match. (From the spec: SMC dispatch sets X0 to
        // `0xFFFF_FFFF` as a signed i32 = `-1`.)
        let not_supported_low = (PsciReturn::NotSupported.as_x0() & 0xFFFF_FFFF) as u32;
        let invalid_low = (PsciReturn::InvalidParameters.as_x0() & 0xFFFF_FFFF) as u32;
        assert_eq!(not_supported_low, 0xFFFF_FFFF);
        assert_eq!(PsciReturn::Success.as_x0(), 0);
        assert_eq!(invalid_low, 0xFFFF_FFFE);
    }
}
