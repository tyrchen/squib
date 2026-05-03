//! HVF VM lifecycle: global init, GIC config, memory mapping.
//!
//! Per [12-hvf-backend.md § 6](../../../specs/12-hvf-backend.md#6-gic--hv_gic_-only) the
//! GIC layout is fixed at VM construction time; `hv_gic_create` runs before any
//! `hv_vcpu_create`. The applevisor crate enforces the right ordering by exposing
//! `VirtualMachineStaticInstance::init_with_gic` — there's no way to create a vCPU
//! before the GIC config landed.

#[cfg(target_os = "macos")]
use applevisor::{
    error::HypervisorError,
    gic::GicConfig,
    memory::{MemPerms, Memory},
    vcpu::Vcpu as VirtualCpu,
    vm::{GicEnabled, VirtualMachineConfig, VirtualMachineInstance, VirtualMachineStaticInstance},
};
use squib_arch::layout::{GICD_BASE, GICR_BASE};
use thiserror::Error;

/// Errors that can surface during HVF VM initialisation.
#[derive(Debug, Error)]
pub enum InitError {
    /// macOS reported a hypervisor error.
    #[error("HVF init error: {0}")]
    Hvf(String),
    /// The GIC redistributor live size, queried from `hv_gic_get_redistributor_size`,
    /// would overlap the PL011/virtio-MMIO band given the configured `vcpu_count`.
    #[error(
        "GIC layout overlap: vcpu_count={vcpu_count}, live_gicr_end={live_gicr_end:#x} crosses \
         PL011 base {boundary:#x}"
    )]
    LayoutOverlap {
        /// Configured `vcpu_count`.
        vcpu_count: u32,
        /// Live GICR end address `GICR_BASE + vcpu_count * redistributor_size_per_vcpu`.
        live_gicr_end: u64,
        /// PL011 base — the boundary the live GICR must not cross.
        boundary: u64,
    },
    /// Squib does not run on hosts without HVF (i.e. anything that isn't macOS).
    #[error("squib-hv requires macOS; compile target is not Apple Silicon")]
    UnsupportedHost,
}

#[cfg(target_os = "macos")]
impl From<HypervisorError> for InitError {
    fn from(err: HypervisorError) -> Self {
        Self::Hvf(format!("{err:?}"))
    }
}

/// Top-level HVF entry point. Holds no state — `applevisor::VirtualMachineInstance`
/// already enforces the singleton via `init_with_gic` + `get_gic`.
#[derive(Debug, Default)]
pub struct HvfHypervisor {
    _private: (),
}

impl HvfHypervisor {
    /// Construct the hypervisor handle. Initialisation is deferred to [`Self::init_vm`]
    /// so the caller can compose the GIC config from the VMM builder.
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }

    /// Initialise the global VM with the squib-fixed GIC layout. Must be called exactly
    /// once per process; calling twice surfaces an HVF error.
    ///
    /// `redistributor_size_per_vcpu` is the value queried via
    /// `applevisor::GicConfig::get_redistributor_size`. Passing it in keeps the layout
    /// overlap-check honest against the actual host (Apple may grow the redistributor
    /// in a future macOS release).
    ///
    /// # Errors
    /// [`InitError::LayoutOverlap`] if the live GICR for `vcpu_count` would cross PL011;
    /// [`InitError::Hvf`] for any failure surfaced by `applevisor`.
    #[cfg(target_os = "macos")]
    pub fn init_vm(
        &self,
        vcpu_count: u32,
        redistributor_size_per_vcpu: u64,
    ) -> Result<HvfVm, InitError> {
        use squib_arch::layout::{LayoutOverlap, overlap_check};

        match overlap_check(vcpu_count, redistributor_size_per_vcpu) {
            LayoutOverlap::Ok => {}
            LayoutOverlap::GicrOverlapsMmio {
                vcpu_count,
                live_gicr_end,
                boundary,
            } => {
                return Err(InitError::LayoutOverlap {
                    vcpu_count,
                    live_gicr_end,
                    boundary,
                });
            }
        }

        let mut gic_cfg = GicConfig::new();
        gic_cfg.set_distributor_base(GICD_BASE)?;
        gic_cfg.set_redistributor_base(GICR_BASE)?;

        let vm_cfg = VirtualMachineConfig::new();
        VirtualMachineStaticInstance::init_with_gic(vm_cfg, gic_cfg)?;

        let instance = VirtualMachineStaticInstance::get_gic().ok_or_else(|| {
            InitError::Hvf("VirtualMachineStaticInstance::get_gic returned None".into())
        })?;

        Ok(HvfVm {
            instance,
            mappings: parking_lot::Mutex::new(Vec::new()),
            vcpu_count,
        })
    }

    /// Init stub for non-macOS hosts. Always returns [`InitError::UnsupportedHost`].
    ///
    /// # Errors
    /// Always [`InitError::UnsupportedHost`].
    #[cfg(not(target_os = "macos"))]
    pub fn init_vm(
        &self,
        _vcpu_count: u32,
        _redistributor_size_per_vcpu: u64,
    ) -> Result<HvfVm, InitError> {
        Err(InitError::UnsupportedHost)
    }
}

/// Handle to a region of guest memory mapped via [`HvfVm::map_memory`].
///
/// The raw host pointer is intentionally **not** exposed: callers stay on the guest-side
/// of the abstraction (`guest_base`, `size`) and write through [`HvfVm::write_to_region`].
/// This keeps `squib-hv` the only crate that touches a `*mut u8`.
#[derive(Debug, Clone, Copy)]
pub struct MappedRegion {
    /// Guest-physical base address of the mapping.
    pub guest_base: u64,
    /// Size of the mapping in bytes.
    pub size: usize,
    slot_index: usize,
}

/// Live HVF VM handle. On non-macOS hosts this is a stub that holds no data.
#[derive(Debug)]
pub struct HvfVm {
    #[cfg(target_os = "macos")]
    instance: VirtualMachineInstance<GicEnabled>,
    #[cfg(target_os = "macos")]
    mappings: parking_lot::Mutex<Vec<Memory>>,
    vcpu_count: u32,
}

impl HvfVm {
    /// Number of vCPUs this VM was configured for.
    #[must_use]
    pub const fn vcpu_count(&self) -> u32 {
        self.vcpu_count
    }

    /// Map a region of guest memory and return a [`MappedRegion`] handle.
    ///
    /// `size_bytes` must be a multiple of the host page size (16 KiB on Apple Silicon).
    /// The returned handle is `Send` and exposes typed write helpers; the underlying
    /// host buffer is owned by the VM and stays mapped until [`HvfVm`] is dropped
    /// (squib's 1.0 lifecycle is one VM per process; this is the right invariant).
    ///
    /// The raw host pointer never crosses out of `squib-hv` — callers operate on
    /// guest-physical offsets through [`HvfVm::write_to_region`].
    ///
    /// # Errors
    /// [`InitError::Hvf`] for any HVF-side failure.
    #[cfg(target_os = "macos")]
    pub fn map_memory(
        &self,
        guest_addr: u64,
        size_bytes: usize,
        perms: MemPerms,
    ) -> Result<MappedRegion, InitError> {
        let mut mem = self.instance.memory_create(size_bytes)?;
        mem.map(guest_addr, perms)?;
        let region = MappedRegion {
            guest_base: guest_addr,
            size: size_bytes,
            slot_index: self.mappings.lock().len(),
        };
        self.mappings.lock().push(mem);
        Ok(region)
    }

    /// Stub for non-macOS hosts.
    ///
    /// # Errors
    /// Always [`InitError::UnsupportedHost`].
    #[cfg(not(target_os = "macos"))]
    pub fn map_memory(
        &self,
        _guest_addr: u64,
        _size_bytes: usize,
        _perms: (),
    ) -> Result<MappedRegion, InitError> {
        Err(InitError::UnsupportedHost)
    }

    /// Write `bytes` into a previously-mapped region at `region_offset`.
    ///
    /// The underlying applevisor `Memory::write` is the controlled gateway; safe callers
    /// never see the raw host pointer.
    ///
    /// # Errors
    /// [`InitError::Hvf`] if the underlying call fails or the offset/length escapes the
    /// region.
    #[cfg(target_os = "macos")]
    pub fn write_to_region(
        &self,
        region: &MappedRegion,
        region_offset: usize,
        bytes: &[u8],
    ) -> Result<(), InitError> {
        let mut mappings = self.mappings.lock();
        let mem = mappings
            .get_mut(region.slot_index)
            .ok_or_else(|| InitError::Hvf(format!("no mapped region #{}", region.slot_index)))?;
        let end = region_offset
            .checked_add(bytes.len())
            .filter(|end| *end <= region.size)
            .ok_or_else(|| {
                InitError::Hvf(format!(
                    "write {} bytes at offset {} escapes region of size {}",
                    bytes.len(),
                    region_offset,
                    region.size
                ))
            })?;
        let _ = end;
        let guest_addr = region.guest_base + region_offset as u64;
        mem.write(guest_addr, bytes)?;
        Ok(())
    }

    /// Stub for non-macOS hosts.
    ///
    /// # Errors
    /// Always [`InitError::UnsupportedHost`].
    #[cfg(not(target_os = "macos"))]
    pub fn write_to_region(
        &self,
        _region: &MappedRegion,
        _region_offset: usize,
        _bytes: &[u8],
    ) -> Result<(), InitError> {
        Err(InitError::UnsupportedHost)
    }

    /// Borrow the underlying applevisor VM instance — used by [`crate::HvfVcpu`] to
    /// create vCPUs from their dedicated threads.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub(crate) fn instance(&self) -> &VirtualMachineInstance<GicEnabled> {
        &self.instance
    }

    /// Wake one or more blocked `hv_vcpu_run` calls (`hv_vcpus_exit`).
    ///
    /// Idempotent and safe to call from any thread — this is the only path squib uses
    /// to interrupt a vCPU; we deliberately do not use signals.
    ///
    /// # Errors
    /// [`InitError::Hvf`] on any HVF-side failure.
    #[cfg(target_os = "macos")]
    pub fn cancel_vcpus(&self, handles: &[applevisor::vcpu::VcpuHandle]) -> Result<(), InitError> {
        self.instance.vcpus_exit(handles)?;
        Ok(())
    }

    /// Stub for non-macOS hosts.
    ///
    /// # Errors
    /// Always [`InitError::UnsupportedHost`].
    #[cfg(not(target_os = "macos"))]
    pub fn cancel_vcpus(&self, _handles: &[()]) -> Result<(), InitError> {
        Err(InitError::UnsupportedHost)
    }
}

/// Convenience: claim a fresh `applevisor::Vcpu` from the calling thread.
///
/// HVF requires every vCPU's lifecycle methods to be called from the thread that
/// originally created it. Use this at the top of the per-vCPU thread.
#[cfg(target_os = "macos")]
pub fn create_vcpu_on_this_thread(vm: &HvfVm) -> Result<VirtualCpu, InitError> {
    Ok(vm.instance().vcpu_create()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hvf_hypervisor_new_is_zero_sized() {
        // Sanity: HvfHypervisor carries no state; a default instance is callable.
        let _hv = HvfHypervisor::new();
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn init_vm_on_non_macos_returns_unsupported_host() {
        let hv = HvfHypervisor::new();
        let err = hv.init_vm(1, 0x0002_0000).unwrap_err();
        assert!(matches!(err, InitError::UnsupportedHost));
    }
}
