//! vCPU exception dispatcher — the deterministic core of the run loop.
//!
//! This module decodes an ESR_EL2 syndrome (via `squib_arch::decode_esr`) and folds in
//! the vCPU's general-purpose register reads to produce a [`Exit`] enum the VMM can
//! act on. It is the testable, host-independent layer of the vCPU run loop; the
//! HVF-specific bits (`hv_vcpu_run`, `hv_vcpus_exit`, register I/O) live in
//! [`crate::vcpu`] under `cfg(target_os = "macos")`.
//!
//! Per [12-hvf-backend.md § 5](../../../specs/12-hvf-backend.md#5-vcpu-run-loop), the
//! decode-then-action flow is:
//!
//! ```text
//! ESR EC                action
//! 0x16  HVC      → Exit::Hvc { imm16, x0..x3 }              (VMM dispatches PSCI)
//! 0x17  SMC      → Exit::SmcHandledAsPsciNotSupported       (D8: matches KVM; pc+=4 in caller)
//! 0x18  MSR/MRS  → Exit::SystemRegister { ... }
//! 0x24  Data     → Exit::Mmio { addr, write, sas, srt, sf } (caller arms pending advance pc)
//! 0x01  WF{I,E}  → Exit::Wfi or Exit::Wfe
//! 0x3C  BRK      → Exit::Brk { imm16 }                      (default policy: log + shutdown)
//! anything else  → Exit::InternalError(String)
//! ```

use std::fmt;

use squib_arch::{
    EsrDecoded, decode_esr,
    psci::{PsciOutcome, PsciReturn, dispatch as dispatch_psci},
};

/// Squib's vCPU exit algebra — what the VMM event loop's dispatcher reacts to.
///
/// This is a richer variant of [`squib_core::VmExit`] suitable for the squib-hv internal
/// loop; the boundary translation to the portable enum happens in `squib-vmm`.
#[derive(Debug, Clone)]
pub enum Exit {
    /// Memory-mapped I/O. The MMU bus dispatcher reads/writes `addr` and the caller arms
    /// `pending_advance_pc` so the next `pre_run_housekeeping` pass advances PC by 4.
    Mmio {
        /// Faulting guest physical address (FAR_EL2).
        addr: u64,
        /// `true` for store, `false` for load.
        write: bool,
        /// Syndrome access size: 0=byte, 1=halfword, 2=word, 3=doubleword.
        sas: u8,
        /// Source/destination register index (X0..X30 / XZR=31).
        srt: u8,
        /// 64-bit register width.
        sf: bool,
    },
    /// Hypercall (HVC). The VMM dispatches PSCI against the args; PC advances by 4 on
    /// the next entry.
    Hvc {
        /// Immediate operand encoded in the HVC instruction.
        imm16: u16,
        /// X0..X3 read at the time of the trap.
        args: [u64; 4],
    },
    /// SMC. Per D8 / [12 § 8](../../../specs/12-hvf-backend.md#8-behaviour-edges) the
    /// VMM places `PSCI_RET_NOT_SUPPORTED` in X0 and advances PC by 4. Logged at debug
    /// level; SMC at runtime is normal probe traffic.
    SmcHandledAsPsciNotSupported {
        /// Original immediate, for logging only.
        imm16: u16,
    },
    /// MSR/MRS trap.
    SystemRegister {
        /// `true` = MRS (read), `false` = MSR (write).
        read: bool,
        /// Op0..Op2 + CRn/CRm/Xt.
        op0: u8,
        /// Op1.
        op1: u8,
        /// CRn.
        crn: u8,
        /// CRm.
        crm: u8,
        /// Op2.
        op2: u8,
        /// Source/destination register.
        xt: u8,
    },
    /// Wait-for-interrupt. The vCPU should sleep until either a pending IRQ in the
    /// shadow bitset or a vtimer wake-up.
    Wfi,
    /// Wait-for-event. Equivalent to WFI for our purposes (devices use IRQ delivery).
    Wfe,
    /// vtimer activated; the VMM unmasks the corresponding GIC line.
    VtimerActivated,
    /// `BRK` instruction — debugger breakpoint.
    Brk {
        /// 16-bit immediate.
        imm16: u16,
    },
    /// Cancellation requested via `HvfVm::cancel_vcpus` (i.e. `hv_vcpus_exit`).
    Cancelled,
    /// Anything we don't have a structured variant for. Caller logs at `error` and
    /// transitions the VM to `Shutdown`. Carries the raw EC + ESR for diagnostics
    /// without allocating on the hot path.
    UnknownExceptionClass {
        /// Exception class field (bits 31..26).
        ec: u8,
        /// Original ESR_EL2 register value.
        raw: u64,
    },
}

/// Result of decoding a single exception synchronously.
#[derive(Debug, Clone)]
pub struct RunLoopDispatch {
    /// The exit to surface up the call stack.
    pub exit: Exit,
    /// `true` if the VMM should advance PC by 4 on the next entry.
    ///
    /// Set for HVC, SMC, MMIO, and SystemRegister. Cleared for WFI/WFE and Brk
    /// (HVF auto-advances those on resume; advancing twice would skip an instruction).
    pub advance_pc: bool,
}

/// Translate an ESR_EL2 + GP-register snapshot into a [`RunLoopDispatch`].
///
/// `gp_read` is a callback the caller provides — typically `vcpu.get_reg(...)`. We pass
/// it in so this function stays host-independent and unit-testable.
///
/// # Errors
/// The function never fails; an unexpected EC produces `Exit::InternalError(...)`.
#[must_use]
pub fn decode_exception<F>(esr: u64, far: u64, mut gp_read: F) -> RunLoopDispatch
where
    F: FnMut(u8) -> u64,
{
    match decode_esr(esr) {
        EsrDecoded::DataAbort {
            is_write,
            sas,
            srt,
            sf,
        } => RunLoopDispatch {
            exit: Exit::Mmio {
                addr: far,
                write: is_write,
                sas,
                srt,
                sf,
            },
            advance_pc: true,
        },
        EsrDecoded::Hvc { imm16 } => {
            let args = [gp_read(0), gp_read(1), gp_read(2), gp_read(3)];
            RunLoopDispatch {
                exit: Exit::Hvc { imm16, args },
                advance_pc: true,
            }
        }
        EsrDecoded::Smc { imm16 } => RunLoopDispatch {
            exit: Exit::SmcHandledAsPsciNotSupported { imm16 },
            advance_pc: true,
        },
        EsrDecoded::SystemRegister {
            read,
            op0,
            op1,
            crn,
            crm,
            op2,
            xt,
        } => RunLoopDispatch {
            exit: Exit::SystemRegister {
                read,
                op0,
                op1,
                crn,
                crm,
                op2,
                xt,
            },
            advance_pc: true,
        },
        EsrDecoded::Wfi => RunLoopDispatch {
            exit: Exit::Wfi,
            advance_pc: false,
        },
        EsrDecoded::Wfe => RunLoopDispatch {
            exit: Exit::Wfe,
            advance_pc: false,
        },
        EsrDecoded::Brk { imm16 } => RunLoopDispatch {
            exit: Exit::Brk { imm16 },
            advance_pc: false,
        },
        EsrDecoded::Other { ec, raw } => RunLoopDispatch {
            exit: Exit::UnknownExceptionClass { ec, raw },
            advance_pc: false,
        },
    }
}

/// Resolve a PSCI HVC into the (`X0` value, action). This is the helper the VMM event
/// loop calls for `Exit::Hvc`. `args` is `[X0, X1, X2, X3]` from the trap.
///
/// Returns `(x0_value, outcome)`. The caller writes `x0_value` into the calling vCPU's X0
/// before resuming. For [`PsciOutcome::BringUpSecondary`] / [`PsciOutcome::QueryAffinityInfo`]
/// the vCPU is parked until the actor responds; the actor then sets X0 to the appropriate
/// final value (Success / AlreadyOn / OnPending / NotSupported).
///
/// For SMC the helper short-circuits to `PsciReturn::NotSupported.as_x0()` — see D8.
#[must_use]
pub fn resolve_hvc_psci(args: [u64; 4]) -> (u64, PsciOutcome) {
    #[allow(clippy::cast_possible_truncation)] // PSCI function IDs are u32
    let func_id = args[0] as u32;
    let extra = [args[1], args[2], args[3]];
    let outcome = dispatch_psci(func_id, extra);
    // For non-direct-return outcomes (BringUpSecondary, QueryAffinityInfo, SystemOff,
    // SystemReset, ParkCallerCpuOff) the dispatcher hands the actor / VMM the request
    // and the caller's X0 is updated by the actor before the calling vCPU resumes (or
    // the VM is torn down before X0 matters). Default to PSCI_RET_SUCCESS — the actor
    // overwrites if the eventual outcome is AlreadyOn / OnPending / NotSupported.
    let x0 = match outcome {
        PsciOutcome::Return(ret) => ret.as_x0(),
        _ => PsciReturn::Success.as_x0(),
    };
    (x0, outcome)
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mmio { addr, write, .. } => {
                write!(
                    f,
                    "MMIO {} addr={:#018x}",
                    if *write { "write" } else { "read" },
                    addr
                )
            }
            Self::Hvc { imm16, args } => write!(
                f,
                "HVC imm16={imm16:#06x} fid={:#010x}",
                args[0] & 0xFFFF_FFFF
            ),
            Self::SmcHandledAsPsciNotSupported { imm16 } => {
                write!(f, "SMC imm16={imm16:#06x} → PSCI_NOT_SUPPORTED")
            }
            Self::SystemRegister {
                read,
                op0,
                op1,
                crn,
                crm,
                op2,
                xt,
            } => write!(
                f,
                "{} sysreg op0={op0} op1={op1} crn={crn} crm={crm} op2={op2} xt={xt}",
                if *read { "MRS" } else { "MSR" }
            ),
            Self::Wfi => f.write_str("WFI"),
            Self::Wfe => f.write_str("WFE"),
            Self::VtimerActivated => f.write_str("VtimerActivated"),
            Self::Brk { imm16 } => write!(f, "BRK imm16={imm16:#06x}"),
            Self::Cancelled => f.write_str("Cancelled"),
            Self::UnknownExceptionClass { ec, raw } => {
                write!(f, "UnknownExceptionClass(ec={ec:#04x} esr={raw:#018x})")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esr(ec: u8, iss: u64) -> u64 {
        (u64::from(ec) << 26) | (iss & 0x01FF_FFFF)
    }

    #[test]
    fn data_abort_produces_mmio_with_advance_pc() {
        // Build a write of size 4 to register x5.
        let iss = (1u64 << 6) | (1u64 << 15) | (5u64 << 16) | (2u64 << 22);
        let dispatch = decode_exception(esr(0x24, iss), 0xDEAD_BEEF, |_| 0);
        assert!(dispatch.advance_pc);
        match dispatch.exit {
            Exit::Mmio {
                addr,
                write,
                sas,
                srt,
                sf,
            } => {
                assert_eq!(addr, 0xDEAD_BEEF);
                assert!(write);
                assert_eq!(sas, 2);
                assert_eq!(srt, 5);
                assert!(sf);
            }
            other => panic!("expected Mmio, got {other:?}"),
        }
    }

    #[test]
    fn hvc_carries_x0_through_x3() {
        let dispatch = decode_exception(esr(0x16, 0xABCD), 0, |i| u64::from(i) + 100);
        assert!(dispatch.advance_pc);
        match dispatch.exit {
            Exit::Hvc { imm16, args } => {
                assert_eq!(imm16, 0xABCD);
                assert_eq!(args, [100, 101, 102, 103]);
            }
            other => panic!("expected Hvc, got {other:?}"),
        }
    }

    #[test]
    fn smc_routes_to_psci_not_supported_with_pc_advance() {
        let dispatch = decode_exception(esr(0x17, 0x42), 0, |_| 0);
        assert!(dispatch.advance_pc);
        assert!(matches!(
            dispatch.exit,
            Exit::SmcHandledAsPsciNotSupported { imm16: 0x42 }
        ));
    }

    #[test]
    fn wfi_does_not_advance_pc() {
        let dispatch = decode_exception(esr(0x01, 0), 0, |_| 0);
        assert!(!dispatch.advance_pc);
        assert!(matches!(dispatch.exit, Exit::Wfi));
    }

    #[test]
    fn wfe_does_not_advance_pc() {
        let dispatch = decode_exception(esr(0x01, 1), 0, |_| 0);
        assert!(!dispatch.advance_pc);
        assert!(matches!(dispatch.exit, Exit::Wfe));
    }

    #[test]
    fn brk_does_not_advance_pc() {
        let dispatch = decode_exception(esr(0x3C, 0xF000), 0, |_| 0);
        assert!(!dispatch.advance_pc);
        assert!(matches!(dispatch.exit, Exit::Brk { imm16: 0xF000 }));
    }

    #[test]
    fn unknown_ec_routes_to_unknown_exception_without_pc_advance() {
        // EC 0x10 is unallocated.
        let dispatch = decode_exception(esr(0x10, 0xFEED), 0, |_| 0);
        assert!(!dispatch.advance_pc);
        assert!(matches!(
            dispatch.exit,
            Exit::UnknownExceptionClass { ec: 0x10, .. }
        ));
    }

    #[test]
    fn psci_resolve_unknown_function_id_returns_not_supported() {
        let (x0, outcome) = resolve_hvc_psci([0xDEAD_BEEF, 0, 0, 0]);
        assert_eq!(outcome, PsciOutcome::Return(PsciReturn::NotSupported));
        // -1 as i32 sign-extended into u64: low 32 bits are 0xFFFFFFFF.
        let low_32 = (x0 & 0xFFFF_FFFF) as u32;
        assert_eq!(low_32, 0xFFFF_FFFF);
    }

    #[test]
    fn psci_resolve_version_returns_one_one() {
        let (x0, _outcome) = resolve_hvc_psci([u64::from(squib_arch::psci::PSCI_VERSION), 0, 0, 0]);
        assert_eq!(x0, 0x0001_0001);
    }

    #[test]
    fn psci_resolve_cpu_on_carries_secondary_args_through() {
        let (x0, outcome) = resolve_hvc_psci([
            u64::from(squib_arch::psci::CPU_ON),
            0xAABB_CCDD,
            0x8000_0000,
            0xDEAD_BEEF,
        ]);
        // Caller sees Success; the actor finalises the actual outcome.
        assert_eq!(x0, 0);
        assert!(matches!(
            outcome,
            PsciOutcome::BringUpSecondary {
                target_cpu: 0xAABB_CCDD,
                entry_point: 0x8000_0000,
                context_id: 0xDEAD_BEEF,
            }
        ));
    }
}
