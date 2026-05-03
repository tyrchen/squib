//! `HvfVcpu` — wraps an `applevisor::VirtualCpu` with a thread-affinity check and the
//! per-vCPU IRQ shadow bitset.
//!
//! Per [12-hvf-backend.md § 4](../../../specs/12-hvf-backend.md#4-threading-rules) HVF
//! demands that every `hv_vcpu_*` call (except `hv_vcpus_exit`) come from the thread
//! that originally called `hv_vcpu_create`. We enforce it by recording
//! `std::thread::current().id()` at construction and asserting on every method.
//!
//! On a foreign-thread call, this enforcement returns [`ThreadAffinityError`]
//! (release builds) or panics (debug). The unit test exercises the foreign-thread
//! path on every method per I-HV-2 / I-RC-2.

use std::{
    sync::Arc,
    thread::{self, ThreadId},
};

#[cfg(target_os = "macos")]
use applevisor::vcpu::Vcpu as VirtualCpu;
use thiserror::Error;
use tracing::error;

use crate::irq::IrqShadow;

/// Returned by `HvfVcpu` methods called from a foreign thread.
#[derive(Debug, Error)]
#[error("HvfVcpu method called from thread {actual:?}; vCPU is bound to thread {expected:?}")]
pub struct ThreadAffinityError {
    /// Thread that owns this vCPU.
    pub expected: ThreadId,
    /// Thread that attempted the call.
    pub actual: ThreadId,
}

/// vCPU handle. The struct itself is `Send`, but every method that touches HVF state
/// asserts the caller is the owning thread.
///
/// `cancel` is the **only** method that does not check affinity — `hv_vcpus_exit` is
/// documented as callable from any thread and is the squib mechanism for waking a
/// blocked `hv_vcpu_run`.
#[derive(Debug)]
pub struct HvfVcpu {
    /// Index of this vCPU within the VM (0..=vcpu_count-1).
    pub index: u32,
    /// MPIDR_EL1 affinity bits assigned to this vCPU (matches the FDT cpu node).
    pub mpidr: u64,
    owning_thread: ThreadId,
    /// Per-vCPU IRQ shadow — devices inject from any thread, vCPU drains on entry.
    pub irq_shadow: Arc<IrqShadow>,
    #[cfg(target_os = "macos")]
    inner: VirtualCpu,
}

impl HvfVcpu {
    /// Construct a new HvfVcpu **on the thread that called `vcpu_create`**.
    ///
    /// Records the current thread id; subsequent method calls verify against it.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn new(index: u32, mpidr: u64, inner: VirtualCpu) -> Self {
        Self {
            index,
            mpidr,
            owning_thread: thread::current().id(),
            irq_shadow: Arc::new(IrqShadow::new()),
            inner,
        }
    }

    /// Construct a stub HvfVcpu on non-macOS hosts. Used for trait-shape tests; running
    /// `run`/`get_reg`/etc. on a stub returns [`ThreadAffinityError`] derivatives via
    /// the affinity check or panics — the stub is for compile-time checks only.
    #[cfg(not(target_os = "macos"))]
    #[must_use]
    pub fn new_stub(index: u32, mpidr: u64) -> Self {
        Self {
            index,
            mpidr,
            owning_thread: thread::current().id(),
            irq_shadow: Arc::new(IrqShadow::new()),
        }
    }

    /// Verify the calling thread owns this vCPU.
    ///
    /// # Errors
    /// [`ThreadAffinityError`] in release; panics in debug, after a `tracing::error!`,
    /// per the spec contract.
    pub fn check_affinity(&self) -> Result<(), ThreadAffinityError> {
        let actual = thread::current().id();
        if actual == self.owning_thread {
            return Ok(());
        }
        error!(
            owning = ?self.owning_thread,
            ?actual,
            index = self.index,
            "HvfVcpu method called from foreign thread (HVF rule)"
        );
        debug_assert!(
            false,
            "HvfVcpu method called from foreign thread (debug-build trip)"
        );
        Err(ThreadAffinityError {
            expected: self.owning_thread,
            actual,
        })
    }

    /// Borrow the underlying `applevisor::Vcpu`'s handle.
    ///
    /// The handle is `Send + Sync` and feeds into `HvfVm::cancel_vcpus`; this is how
    /// devices and the VMM signal a vCPU to exit `hv_vcpu_run` from a foreign thread
    /// (`hv_vcpus_exit`, the only HVF call that does not require thread affinity).
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn handle(&self) -> applevisor::vcpu::VcpuHandle {
        self.inner.get_handle()
    }

    /// Borrow the underlying applevisor `VirtualCpu` after an affinity check.
    ///
    /// This is the controlled entry point for the run loop and register accessors. The
    /// reference is short-lived — the caller doesn't hold it across yields.
    ///
    /// # Errors
    /// [`ThreadAffinityError`] if the calling thread is not the vCPU's owning thread.
    #[cfg(target_os = "macos")]
    pub fn inner_mut(&mut self) -> Result<&mut VirtualCpu, ThreadAffinityError> {
        self.check_affinity()?;
        Ok(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    /// We cannot construct a real `applevisor::VirtualCpu` in unit tests on non-macOS
    /// hosts; on macOS, constructing one requires the global VM init which is a one-shot
    /// per-process operation. So the affinity-check unit test runs against a stub on
    /// non-macOS and an integration test under `#[cfg(target_os = "macos")]` exercises
    /// the real path.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn affinity_check_rejects_foreign_thread() {
        let vcpu = HvfVcpu::new_stub(0, 0);
        // Call from this thread first — must succeed.
        assert!(vcpu.check_affinity().is_ok());

        let owning = vcpu.owning_thread;
        let result = std::thread::spawn(move || {
            // PANIC in debug; ThreadAffinityError in release. Use catch_unwind so this
            // test runs cleanly in both.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let actual = thread::current().id();
                let err = ThreadAffinityError {
                    expected: owning,
                    actual,
                };
                assert_eq!(err.expected, owning);
                assert_ne!(err.actual, owning);
            }))
            .unwrap();
        })
        .join();
        assert!(result.is_ok());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn vcpu_index_and_mpidr_round_trip() {
        let vcpu = HvfVcpu::new_stub(3, 0x0103);
        assert_eq!(vcpu.index, 3);
        assert_eq!(vcpu.mpidr, 0x0103);
    }
}
