//! The portable [`VmExit`] enum.
//!
//! This subsumes the union of KVM's `VcpuExit` variants and the exit shapes returned by
//! macOS Hypervisor.framework / Virtualization.framework. Each backend decodes its own
//! native exit struct into one of these variants; the VMM dispatch loop in `squib-vmm`
//! is therefore backend-agnostic.

use smallvec::SmallVec;

/// A bounded vector of bytes carrying an MMIO/PIO payload — fits in one cache line for the
/// common 1–8 byte access patterns.
pub type ExitData = SmallVec<[u8; 8]>;

/// The reason a vCPU returned from its run loop.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum VmExit {
    /// Memory-mapped I/O access. The MMIO bus router in `squib-vmm` dispatches to the
    /// appropriate device emulation.
    Mmio {
        /// The guest-physical address being accessed.
        addr: u64,
        /// `true` for a write, `false` for a read.
        write: bool,
        /// Payload bytes (read: filled by the device; write: the bytes the guest wrote).
        data: ExitData,
    },

    /// Port-mapped I/O access. x86-only; on other architectures this variant never appears.
    Pio {
        /// The I/O port number.
        port: u16,
        /// `true` for `OUT`, `false` for `IN`.
        write: bool,
        /// Payload bytes (1, 2, or 4 wide).
        data: ExitData,
    },

    /// The guest issued a hypercall (HVC on aarch64, VMCALL on x86).
    Hypercall {
        /// Hypercall function number.
        func: u64,
        /// Up to six argument registers from the calling convention.
        args: [u64; 6],
    },

    /// The guest executed `WFI` (wait-for-interrupt). The vCPU should sleep until an
    /// interrupt is injected.
    Wfi,
    /// The guest executed `WFE` (wait-for-event). x86's `HLT` maps to [`VmExit::Hlt`] instead.
    Wfe,
    /// The guest executed `HLT` (x86). Equivalent to WFI in practice.
    Hlt,
    /// The guest issued a system reset.
    Reset,
    /// The guest powered down.
    Shutdown,

    /// A debug exception (single-step, breakpoint). Surfaced when the optional `gdb` feature
    /// is wired up; otherwise the backend treats this as an internal error.
    Debug(DebugInfo),

    /// The hypervisor reported an internal failure that does not fit any of the above
    /// variants. The string is for human consumption only; programmatic consumers should
    /// treat this as fatal.
    InternalError(String),
}

/// Information surfaced on a debug exit.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DebugInfo {
    /// Program counter at the point of the exception.
    pub pc: u64,
    /// Encoded exception syndrome (architecture-specific bit layout).
    pub syndrome: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmio_payload_inlines_for_small_widths() {
        let data: ExitData = SmallVec::from_slice(&[0xAB, 0xCD]);
        assert!(!data.spilled());
    }

    #[test]
    fn variants_are_clone() {
        let exit = VmExit::Wfi;
        let _again = exit.clone();
    }
}
