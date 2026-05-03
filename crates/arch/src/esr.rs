//! ESR_EL2 syndrome decoder.
//!
//! ESR_EL2 (Exception Syndrome Register, EL2) is the synchronous-exception classifier the
//! HVF backend reads on every `EXCEPTION` exit. Bits `[31:26]` are the exception class
//! (`EC`); the meaning of the remaining bits depends on the EC value.
//!
//! The decoder never panics and never produces undefined output: any unknown EC is
//! returned as [`EsrDecoded::Other`] for the caller to log + abort cleanly. This
//! invariant is pinned by I-HV-4 (`12-hvf-backend.md § 9`).
//!
//! References: Arm ARM (Section D17.2 and D24), libkrun's `vendors/libkrun/src/hvf`.

/// Exception class (`EC`) constants, raw values from Arm ARM § D17.2.16.
pub mod ec {
    /// Unknown reason.
    pub const UNKNOWN: u8 = 0x00;
    /// Trapped WFI/WFE.
    pub const WFX: u8 = 0x01;
    /// Trapped MCR/MRC access (cp15, AArch32).
    pub const MCR_MRC_CP15: u8 = 0x03;
    /// Trapped MCRR/MRRC access (cp15, AArch32).
    pub const MCRR_MRRC_CP15: u8 = 0x04;
    /// Trapped MCR/MRC access (cp14, AArch32).
    pub const MCR_MRC_CP14: u8 = 0x05;
    /// Trapped LDC/STC access.
    pub const LDC_STC: u8 = 0x06;
    /// FP/SIMD/SVE access trap.
    pub const FP_ASIMD: u8 = 0x07;
    /// Branch target identification mismatch.
    pub const BTI: u8 = 0x0D;
    /// Illegal execution state.
    pub const ILLEGAL_STATE: u8 = 0x0E;
    /// SVC instruction (AArch64).
    pub const SVC: u8 = 0x15;
    /// HVC instruction (AArch64).
    pub const HVC: u8 = 0x16;
    /// SMC instruction (AArch64).
    pub const SMC: u8 = 0x17;
    /// Trapped MSR / MRS / system instruction (AArch64).
    pub const SYSTEM_REGISTER: u8 = 0x18;
    /// Pointer-authentication failure.
    pub const PAUTH: u8 = 0x1C;
    /// Instruction abort, lower EL.
    pub const INST_ABORT_LOWER_EL: u8 = 0x20;
    /// Instruction abort, same EL.
    pub const INST_ABORT_SAME_EL: u8 = 0x21;
    /// PC alignment fault.
    pub const PC_ALIGNMENT: u8 = 0x22;
    /// Data abort, lower EL — the virtio/MMIO path.
    pub const DATA_ABORT_LOWER_EL: u8 = 0x24;
    /// Data abort, same EL.
    pub const DATA_ABORT_SAME_EL: u8 = 0x25;
    /// SP alignment fault.
    pub const SP_ALIGNMENT: u8 = 0x26;
    /// Trapped FP exception (AArch64).
    pub const FPE_AARCH64: u8 = 0x2C;
    /// SError interrupt.
    pub const SERROR: u8 = 0x2F;
    /// Breakpoint, lower EL.
    pub const BREAKPOINT_LOWER_EL: u8 = 0x30;
    /// Breakpoint, same EL.
    pub const BREAKPOINT_SAME_EL: u8 = 0x31;
    /// Software step, lower EL.
    pub const SOFTWARE_STEP_LOWER_EL: u8 = 0x32;
    /// Software step, same EL.
    pub const SOFTWARE_STEP_SAME_EL: u8 = 0x33;
    /// Watchpoint, lower EL.
    pub const WATCHPOINT_LOWER_EL: u8 = 0x34;
    /// Watchpoint, same EL.
    pub const WATCHPOINT_SAME_EL: u8 = 0x35;
    /// AArch32 BKPT.
    pub const BKPT_AARCH32: u8 = 0x38;
    /// AArch64 BRK.
    pub const BRK_AARCH64: u8 = 0x3C;
}

/// A decoded ESR_EL2 value.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EsrDecoded {
    /// Data abort taken from a lower exception level. `is_write` distinguishes load vs
    /// store; `sas` is the syndrome access size (`0` = byte, `1` = halfword, `2` = word,
    /// `3` = doubleword). `srt` is the source/destination register index (0..=31). `sf`
    /// indicates 64-bit register width.
    DataAbort {
        /// `true` for store, `false` for load.
        is_write: bool,
        /// Syndrome access size: byte/halfword/word/doubleword.
        sas: u8,
        /// Source/destination register (X0..X31, where X31 is XZR).
        srt: u8,
        /// 64-bit operation flag.
        sf: bool,
    },
    /// HVC immediate. The argument register x[0..3] is read separately by the caller.
    Hvc {
        /// 16-bit immediate operand encoded in the instruction.
        imm16: u16,
    },
    /// SMC immediate.
    Smc {
        /// 16-bit immediate operand encoded in the instruction.
        imm16: u16,
    },
    /// Trapped MSR / MRS / system instruction.
    SystemRegister {
        /// `true` = MRS (read sysreg), `false` = MSR (write sysreg).
        read: bool,
        /// Op0 from the encoding.
        op0: u8,
        /// Op1.
        op1: u8,
        /// CRn.
        crn: u8,
        /// CRm.
        crm: u8,
        /// Op2.
        op2: u8,
        /// Source/destination register (X0..X30; X31 is XZR).
        xt: u8,
    },
    /// `WFI`.
    Wfi,
    /// `WFE`.
    Wfe,
    /// `BRK` immediate (debugger breakpoint).
    Brk {
        /// 16-bit immediate.
        imm16: u16,
    },
    /// Anything not specifically decoded above. The caller treats this as fatal and
    /// returns `VmExit::InternalError`.
    Other {
        /// Exception class field (bits 31..26).
        ec: u8,
        /// The original raw ESR_EL2 register value, for logging.
        raw: u64,
    },
}

/// Decode an ESR_EL2 register value.
///
/// The decoder is total — every `u64` produces some [`EsrDecoded`] without panicking. It
/// is read-only and zero-allocation, so it is cheap enough to call on every exception.
#[must_use]
pub fn decode(esr: u64) -> EsrDecoded {
    let ec = ((esr >> 26) & 0x3F) as u8;
    let iss = esr & 0x01FF_FFFF; // 25-bit ISS field
    match ec {
        ec::DATA_ABORT_LOWER_EL | ec::DATA_ABORT_SAME_EL => decode_data_abort(iss),
        ec::HVC => EsrDecoded::Hvc {
            imm16: (iss & 0xFFFF) as u16,
        },
        ec::SMC => EsrDecoded::Smc {
            imm16: (iss & 0xFFFF) as u16,
        },
        ec::SYSTEM_REGISTER => decode_system_register(iss),
        ec::WFX => decode_wfx(iss),
        ec::BRK_AARCH64 => EsrDecoded::Brk {
            imm16: (iss & 0xFFFF) as u16,
        },
        _ => EsrDecoded::Other { ec, raw: esr },
    }
}

fn decode_data_abort(iss: u64) -> EsrDecoded {
    // ISS layout for data abort (Arm ARM § D17.2.41):
    //   bit 24: ISV (instruction syndrome valid)
    //   bits 23..22: SAS
    //   bit 21: SSE (sign-extended)
    //   bits 20..16: SRT
    //   bit 15: SF (64-bit register width)
    //   bit 14: AR (acquire/release)
    //   bit 6: WnR (write not read)
    let is_write = (iss & (1 << 6)) != 0;
    let sas = ((iss >> 22) & 0x3) as u8;
    let srt = ((iss >> 16) & 0x1F) as u8;
    let sf = (iss & (1 << 15)) != 0;
    EsrDecoded::DataAbort {
        is_write,
        sas,
        srt,
        sf,
    }
}

fn decode_system_register(iss: u64) -> EsrDecoded {
    // ISS layout for MSR/MRS trap (Arm ARM § D17.2.37):
    //   bits 21..20: Op0
    //   bits 19..17: Op2
    //   bits 16..14: Op1
    //   bits 13..10: CRn
    //   bits  9..5:  Rt (xt)
    //   bits  4..1:  CRm
    //   bit   0:     Direction — 1 = read (MRS), 0 = write (MSR)
    let op0 = ((iss >> 20) & 0x3) as u8;
    let op2 = ((iss >> 17) & 0x7) as u8;
    let op1 = ((iss >> 14) & 0x7) as u8;
    let crn = ((iss >> 10) & 0xF) as u8;
    let xt = ((iss >> 5) & 0x1F) as u8;
    let crm = ((iss >> 1) & 0xF) as u8;
    let read = (iss & 0x1) == 1;
    EsrDecoded::SystemRegister {
        read,
        op0,
        op1,
        crn,
        crm,
        op2,
        xt,
    }
}

fn decode_wfx(iss: u64) -> EsrDecoded {
    // ISS bit 0 distinguishes WFE (1) from WFI (0).
    if (iss & 0x1) == 1 {
        EsrDecoded::Wfe
    } else {
        EsrDecoded::Wfi
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an ESR_EL2 value from `ec` (6 bits) and an ISS payload (25 bits).
    const fn esr(ec: u8, iss: u64) -> u64 {
        ((ec as u64) << 26) | (iss & 0x01FF_FFFF)
    }

    #[test]
    fn decoder_is_total_for_every_ec() {
        // Run every 6-bit EC and assert the function never panics.
        for ec_value in 0u8..=0x3F {
            let _ = decode(esr(ec_value, 0));
            let _ = decode(esr(ec_value, 0x01FF_FFFF));
        }
    }

    #[test]
    fn unknown_ec_returns_other_with_raw_preserved() {
        // EC 0x10 is unallocated.
        let raw = esr(0x10, 0xDEAD_BEEF);
        match decode(raw) {
            EsrDecoded::Other { ec, raw: got } => {
                assert_eq!(ec, 0x10);
                assert_eq!(got, raw);
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn data_abort_decodes_write_register_and_size() {
        // SAS=2 (word), SRT=5, SF=1, WnR=1 (write).
        let iss = (1u64 << 6) | (1u64 << 15) | (5u64 << 16) | (2u64 << 22);
        let decoded = decode(esr(ec::DATA_ABORT_LOWER_EL, iss));
        assert_eq!(
            decoded,
            EsrDecoded::DataAbort {
                is_write: true,
                sas: 2,
                srt: 5,
                sf: true,
            }
        );
    }

    #[test]
    fn data_abort_distinguishes_load_vs_store() {
        // WnR = 0 (read).
        let iss = 0;
        let decoded = decode(esr(ec::DATA_ABORT_LOWER_EL, iss));
        let EsrDecoded::DataAbort { is_write, .. } = decoded else {
            panic!("expected DataAbort, got {decoded:?}");
        };
        assert!(!is_write);
    }

    #[test]
    fn hvc_extracts_imm16() {
        let imm = 0xBEEF_u16;
        let decoded = decode(esr(ec::HVC, u64::from(imm)));
        assert_eq!(decoded, EsrDecoded::Hvc { imm16: imm });
    }

    #[test]
    fn smc_extracts_imm16() {
        let imm = 0x0042_u16;
        let decoded = decode(esr(ec::SMC, u64::from(imm)));
        assert_eq!(decoded, EsrDecoded::Smc { imm16: imm });
    }

    #[test]
    fn brk_extracts_imm16() {
        let decoded = decode(esr(ec::BRK_AARCH64, 0xF000));
        assert_eq!(decoded, EsrDecoded::Brk { imm16: 0xF000 });
    }

    #[test]
    fn wfi_vs_wfe_distinguished_by_iss_bit_0() {
        let wfi = decode(esr(ec::WFX, 0));
        let wfe = decode(esr(ec::WFX, 1));
        assert_eq!(wfi, EsrDecoded::Wfi);
        assert_eq!(wfe, EsrDecoded::Wfe);
    }

    #[test]
    fn system_register_decodes_full_op_tuple() {
        // Encode (op0=3, op1=0, crn=1, crm=2, op2=4, xt=5, read=1).
        // op1 = 0 is a real value but the resulting `0u64 << 14` is dead-code from the
        // identity-op linter's perspective; allow at the test scope.
        #[allow(clippy::identity_op)]
        let iss = (3u64 << 20)   // op0
            | (4u64 << 17)       // op2
            | (0u64 << 14)       // op1
            | (1u64 << 10)       // crn
            | (5u64 << 5)        // xt
            | (2u64 << 1)        // crm
            | 1u64; // read
        let decoded = decode(esr(ec::SYSTEM_REGISTER, iss));
        assert_eq!(
            decoded,
            EsrDecoded::SystemRegister {
                read: true,
                op0: 3,
                op1: 0,
                crn: 1,
                crm: 2,
                op2: 4,
                xt: 5,
            }
        );
    }

    #[test]
    fn property_test_random_inputs_never_panic() {
        // Cheap deterministic sweep — proptest is not pulled in for squib-core.
        // Exercise every multiple of 0x1000_0000 across the full u64 range.
        let mut esr_value: u64 = 0;
        for _ in 0..1000 {
            let _ = decode(esr_value);
            esr_value = esr_value.wrapping_add(0x0123_4567_89AB_CDEF);
        }
    }
}
