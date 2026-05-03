//! aarch64 register set + initial-register helper.
//!
//! `set_boot_regs` writes the four registers that bring vCPU 0 to the kernel entry point.
//! Per [13-arch-and-boot.md § 8](../../../specs/13-arch-and-boot.md#8-vcpu-initial-registers):
//!
//! ```text
//!     PC      = kernel_load_addr
//!     X0      = fdt_addr
//!     X1..X3  = 0
//!     PSTATE  = 0x3C5    // EL1h, DAIF masked, M[3:0] = EL1h, F=I=A=D=1
//! ```
//!
//! Secondary vCPUs come up via PSCI `CPU_ON` with the same triple; the dispatcher in
//! `psci` produces the inputs and the host backend (`squib-hv`) calls `set_boot_regs` on
//! the target vCPU.

/// PSTATE value the boot registers initialise — EL1h with DAIF masked.
///
/// Bit layout (Arm ARM § C5.2.18):
/// - `M[3:0] = 0b0101` (EL1h)
/// - `F = 1, I = 1, A = 1, D = 1` (all interrupts/aborts masked initially)
pub const BOOT_PSTATE: u64 = 0x3C5;

/// Reg enum — the registers the trait surface exposes get/set on.
///
/// Mirrors the spec in [11-runtime-core.md § 2](../../../specs/11-runtime-core.md#2-interface):
/// X0..X30, SP_EL1, PC, PSTATE.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum Reg {
    /// General-purpose register `Xn` where `n ∈ 0..=30`.
    X(u8),
    /// Stack pointer at EL1.
    SpEl1,
    /// Program counter.
    Pc,
    /// Processor state register.
    PState,
}

impl Reg {
    /// Build [`Reg::X`] with index validation.
    ///
    /// # Errors
    /// Returns `None` for indices ≥ 31.
    #[must_use]
    pub const fn x(index: u8) -> Option<Self> {
        if index <= 30 {
            Some(Self::X(index))
        } else {
            None
        }
    }
}

/// The four register values written at boot for vCPU 0 (and via PSCI `CPU_ON` for
/// secondaries).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct BootRegs {
    /// Kernel entry point (PC).
    pub kernel_load_addr: u64,
    /// FDT base address (placed in X0 per `arm64/booting.rst`).
    pub fdt_addr: u64,
    /// PSTATE — defaults to [`BOOT_PSTATE`].
    pub pstate: u64,
}

impl BootRegs {
    /// Build a [`BootRegs`] for the boot vCPU.
    #[must_use]
    pub const fn new(kernel_load_addr: u64, fdt_addr: u64) -> Self {
        Self {
            kernel_load_addr,
            fdt_addr,
            pstate: BOOT_PSTATE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_pstate_matches_spec() {
        assert_eq!(BOOT_PSTATE, 0x3C5);
    }

    #[test]
    fn x_constructor_accepts_valid_indices() {
        for i in 0..=30u8 {
            assert!(matches!(Reg::x(i), Some(Reg::X(_))));
        }
    }

    #[test]
    fn x_constructor_rejects_out_of_range() {
        assert!(Reg::x(31).is_none());
        assert!(Reg::x(255).is_none());
    }

    #[test]
    fn boot_regs_default_pstate() {
        let regs = BootRegs::new(0x8020_0000, 0xBFE0_0000);
        assert_eq!(regs.pstate, BOOT_PSTATE);
        assert_eq!(regs.kernel_load_addr, 0x8020_0000);
        assert_eq!(regs.fdt_addr, 0xBFE0_0000);
    }
}
