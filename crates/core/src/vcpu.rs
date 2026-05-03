//! vCPU trait and the architecture-tagged register file.

use crate::{error::Result, exit::VmExit};

/// Architecture-tagged register file.
///
/// Different backends own different register subsets — a KVM x86 vCPU exposes CR0/CR3/EFER/MSRs
/// that don't exist on aarch64 HVF, and vice versa. We model that with a sum type and let each
/// backend convert to/from its native struct in its own crate.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Regs {
    /// `x86_64` register file. Field set is intentionally minimal in `squib-core`; backend
    /// crates carry richer typed structs for their own use.
    X86_64 {
        /// Instruction pointer.
        rip: u64,
        /// Stack pointer.
        rsp: u64,
        /// 16 general-purpose registers in the canonical order
        /// (RAX, RBX, RCX, RDX, RSI, RDI, RBP, R8, R9, R10, R11, R12, R13, R14, R15, FLAGS).
        gprs: [u64; 16],
    },

    /// `aarch64` register file (EL1). PC + 31 GPRs (X0..X30) + `SP_EL1` + `PSTATE`.
    Aarch64 {
        /// Program counter.
        pc: u64,
        /// Stack pointer at EL1.
        sp_el1: u64,
        /// 31 general-purpose registers (X0..X30).
        gprs: [u64; 31],
        /// Processor state register.
        pstate: u64,
    },
}

/// An interrupt to be injected into a vCPU.
///
/// Number space and meaning depend on architecture:
/// - x86: vector 0..=255, with `level=true` for level-triggered.
/// - aarch64: SPI/PPI/SGI ID, `level` ignored (GIC line is implicit).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
pub struct Irq {
    /// The architecture-specific interrupt identifier.
    pub id: u32,
    /// `true` for level-triggered, `false` for edge-triggered.
    pub level: bool,
}

impl Irq {
    /// Build an edge-triggered interrupt by id.
    pub const fn edge(id: u32) -> Self {
        Self { id, level: false }
    }

    /// Build a level-triggered interrupt by id.
    pub const fn level(id: u32) -> Self {
        Self { id, level: true }
    }
}

/// A virtual CPU running inside a [`Vm`](crate::backend::Vm).
///
/// Each implementation of `Vcpu` is bound to a single OS thread for the duration of its
/// lifetime, mirroring the `hv_vcpu_create`/`hv_vcpu_run` pthread-affinity rule on macOS
/// Hypervisor.framework. Callers must not move a `Vcpu` between threads.
pub trait Vcpu: Send {
    /// Run the vCPU until the next exit and return the reason.
    fn run(&mut self) -> Result<VmExit>;

    /// Cancel a concurrent [`Self::run`] call. On HVF this maps to `hv_vcpus_exit`; on
    /// KVM it sends `SIGRTMIN`. Idempotent and safe to call from any thread.
    fn cancel(&self);

    /// Read the architecture-tagged register file.
    fn get_regs(&self) -> Result<Regs>;

    /// Overwrite the architecture-tagged register file.
    fn set_regs(&mut self, regs: &Regs) -> Result<()>;

    /// Inject an interrupt that will be delivered to the guest at the next entry.
    fn inject_irq(&mut self, irq: Irq) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn irq_constructors_set_level() {
        assert!(!Irq::edge(33).level);
        assert!(Irq::level(33).level);
    }

    #[test]
    fn aarch64_regs_have_31_gprs() {
        let r = Regs::Aarch64 {
            pc: 0,
            sp_el1: 0,
            gprs: [0; 31],
            pstate: 0,
        };
        match r {
            Regs::Aarch64 { gprs, .. } => assert_eq!(gprs.len(), 31),
            Regs::X86_64 { .. } => unreachable!(),
        }
    }
}
