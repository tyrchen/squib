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
#[cfg(target_os = "macos")]
use squib_arch::layout::{GICD_BASE, GICR_BASE};
use thiserror::Error;

/// Number of top-end SPI INTIDs squib reserves as the HVF MSI window.
///
/// HVF treats the MSI range as MSI-exclusive: any INTID inside it can only be
/// triggered via `hv_gic_send_msi`. `hv_gic_set_spi` for those IDs returns
/// `BAD_ARGUMENT`. Squib's legacy + virtio-MMIO devices use plain
/// edge-triggered SPIs at low INTIDs (PL011=33, virtio slots start at 48), so
/// the MSI window must sit *above* them. We park MSI at the top of the range —
/// leaves the low ~900 INTIDs free for SPI use, which is plenty for a 1.0 VMM.
const MSI_INTID_RESERVE: u32 = 64;

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

        // HVF's GIC interception only kicks in once *all three* base
        // addresses are configured: distributor, redistributor, MSI
        // region. Skipping MSI causes HVF to silently fall back to "GIC
        // disabled" semantics — distributor/redistributor reads then
        // miss stage-2 mappings and surface as data aborts to the host
        // (PIDR2 reads at GICR+0xFFE8 read as zero, kernel panics with
        // PSCI SYSTEM_RESET).
        //
        // We park MSI at `MSI_REGION_BASE` (squib_arch::layout) — below
        // GICD, away from DRAM and the virtio MMIO band. The MSI
        // interrupt range covers the full SPI window we negotiated with
        // HVF (so guest-side virtio-MSI, when wired up later, has IDs
        // to allocate from).
        let mut gic_cfg = GicConfig::new();
        gic_cfg.set_distributor_base(GICD_BASE)?;
        gic_cfg.set_redistributor_base(GICR_BASE)?;
        gic_cfg.set_msi_region_base(squib_arch::layout::MSI_REGION_BASE)?;
        let (spi_base, spi_count) = GicConfig::get_spi_interrupt_range()?;
        let msi_count = spi_count.min(MSI_INTID_RESERVE);
        let msi_base = spi_base + spi_count - msi_count;
        gic_cfg.set_msi_interrupt_range(msi_base, msi_count)?;

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

/// Portable [`squib_core::GuestMemory`] view over an `applevisor::Memory`
/// region. Constructed via [`HvfVm::first_region_as_guest_memory`]; held
/// by virtio devices that need to read / write descriptor chains and
/// payload buffers in guest memory.
///
/// Cloning is cheap (`Arc` clone). Reads / writes funnel through a
/// per-region `parking_lot::Mutex` to keep `applevisor::Memory::write`'s
/// `&mut self` requirement intact under concurrent device traffic.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct HvfGuestMemory {
    inner: std::sync::Arc<parking_lot::Mutex<Memory>>,
}

// SAFETY: same rationale as the `Send`/`Sync` impls on `HvfVm` —
// the underlying host buffer is shared memory; access is serialized
// through `parking_lot::Mutex<Memory>`; the only non-Send token is the
// `*const c_void` raw pointer inside `Memory`, which is sound to share
// because the pointed-to memory is process-wide.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
unsafe impl Send for HvfGuestMemory {}
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
unsafe impl Sync for HvfGuestMemory {}

#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub struct HvfGuestMemory;

#[cfg(target_os = "macos")]
impl squib_core::GuestMemory for HvfGuestMemory {
    fn read(&self, addr: squib_core::GuestAddress, buf: &mut [u8]) -> squib_core::Result<()> {
        let mem = self.inner.lock();
        mem.read(addr.raw(), buf).map_err(|e| {
            squib_core::Error::backend(format!(
                "HvfGuestMemory::read({}, {} bytes): {e:?}",
                addr,
                buf.len()
            ))
        })
    }

    fn write(&self, addr: squib_core::GuestAddress, buf: &[u8]) -> squib_core::Result<()> {
        let mut mem = self.inner.lock();
        mem.write(addr.raw(), buf).map_err(|e| {
            squib_core::Error::backend(format!(
                "HvfGuestMemory::write({}, {} bytes): {e:?}",
                addr,
                buf.len()
            ))
        })
    }
}

#[cfg(not(target_os = "macos"))]
impl squib_core::GuestMemory for HvfGuestMemory {
    fn read(&self, _addr: squib_core::GuestAddress, _buf: &mut [u8]) -> squib_core::Result<()> {
        Err(squib_core::Error::Unsupported("HvfGuestMemory needs macOS"))
    }

    fn write(&self, _addr: squib_core::GuestAddress, _buf: &[u8]) -> squib_core::Result<()> {
        Err(squib_core::Error::Unsupported("HvfGuestMemory needs macOS"))
    }
}

/// Live HVF VM handle. On non-macOS hosts this is a stub that holds no data.
///
/// Mappings are wrapped in `Arc<parking_lot::Mutex<Memory>>` so device
/// threads can hold a shared handle to a region and read / write through
/// the safe `applevisor::Memory::read` / `write` API without touching the
/// raw host pointer. The outer mutex protects the mapping vector itself
/// (rare insert at boot); the per-region inner mutex serializes
/// concurrent device-thread access to a single region.
#[derive(Debug)]
pub struct HvfVm {
    #[cfg(target_os = "macos")]
    instance: VirtualMachineInstance<GicEnabled>,
    #[cfg(target_os = "macos")]
    mappings: parking_lot::Mutex<Vec<std::sync::Arc<parking_lot::Mutex<Memory>>>>,
    vcpu_count: u32,
}

// SAFETY: `HvfVm` is conceptually a process-wide handle. The underlying
// `applevisor::Memory` carries a raw `*const c_void` pointer to a host
// allocation that is **shared across all threads** (it backs guest
// physical memory mapped into the HVF stage-2 page tables). The pointer
// itself is plain memory; it doesn't carry thread-local state. The
// non-Sync interior (`Mutex<Vec<Memory>>`) is the only place that
// mutates the mapping table, and we only call into it from the main
// VMM thread (boot kernel/FDT writes) before the vCPU thread starts —
// after that the mappings stay live for the lifetime of `HvfVm`. The
// vCPU thread only needs `instance()` to call `vcpu_create()`, and
// `VirtualMachineInstance<Gic>` is already trivially Send (only
// `Option<Arc<()>>` + `PhantomData`). Sending an `Arc<HvfVm>` to the
// vCPU thread therefore preserves all HVF safety contracts. See
// [12-hvf-backend.md § 4](../../../specs/12-hvf-backend.md#4-threading-rules):
// HVF allows arbitrary-thread access to VM-level handles; only the
// per-vCPU calls are thread-affine, and `HvfVcpu` enforces that
// separately via `check_affinity`.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
unsafe impl Send for HvfVm {}
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
unsafe impl Sync for HvfVm {}

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
        let mut mappings = self.mappings.lock();
        let region = MappedRegion {
            guest_base: guest_addr,
            size: size_bytes,
            slot_index: mappings.len(),
        };
        mappings.push(std::sync::Arc::new(parking_lot::Mutex::new(mem)));
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
        let mappings = self.mappings.lock();
        let mem_arc = mappings
            .get(region.slot_index)
            .ok_or_else(|| InitError::Hvf(format!("no mapped region #{}", region.slot_index)))?
            .clone();
        drop(mappings);
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
        mem_arc.lock().write(guest_addr, bytes)?;
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

    /// Convenience: write `bytes` at `offset` into the first mapped region.
    ///
    /// The boot path maps a single contiguous DRAM region first, so this
    /// is the right primitive for kernel + FDT staging without exposing
    /// a `MappedRegion` handle to the caller.
    ///
    /// # Errors
    /// [`InitError::Hvf`] if no region is mapped or the write escapes
    /// the region.
    #[cfg(target_os = "macos")]
    pub fn write_to_first_region(
        &self,
        region_offset: usize,
        bytes: &[u8],
    ) -> Result<(), InitError> {
        let mappings = self.mappings.lock();
        let mem_arc = mappings
            .first()
            .ok_or_else(|| InitError::Hvf("no regions mapped".to_string()))?
            .clone();
        drop(mappings);
        let guest_addr = squib_arch::layout::DRAM_BASE + region_offset as u64;
        mem_arc.lock().write(guest_addr, bytes)?;
        Ok(())
    }

    /// Build a portable [`squib_core::GuestMemory`] handle backed by the
    /// first mapped region.
    ///
    /// Devices (virtio frontends, the dumbo TCP server) consume guest
    /// memory through this trait; calling them with the returned handle
    /// gives them a clone-shareable, thread-safe view that funnels through
    /// `applevisor::Memory::read` / `write` with a per-region mutex.
    #[cfg(target_os = "macos")]
    pub fn first_region_as_guest_memory(
        &self,
    ) -> Result<std::sync::Arc<HvfGuestMemory>, InitError> {
        let mappings = self.mappings.lock();
        let mem_arc = mappings
            .first()
            .ok_or_else(|| InitError::Hvf("no regions mapped".to_string()))?
            .clone();
        Ok(std::sync::Arc::new(HvfGuestMemory { inner: mem_arc }))
    }

    /// Stub for non-macOS hosts.
    ///
    /// # Errors
    /// Always [`InitError::UnsupportedHost`].
    #[cfg(not(target_os = "macos"))]
    pub fn first_region_as_guest_memory(
        &self,
    ) -> Result<std::sync::Arc<HvfGuestMemory>, InitError> {
        Err(InitError::UnsupportedHost)
    }

    /// Stub for non-macOS hosts.
    ///
    /// # Errors
    /// Always [`InitError::UnsupportedHost`].
    #[cfg(not(target_os = "macos"))]
    pub fn write_to_first_region(
        &self,
        _region_offset: usize,
        _bytes: &[u8],
    ) -> Result<(), InitError> {
        Err(InitError::UnsupportedHost)
    }

    /// Borrow the underlying applevisor VM instance — used by [`crate::HvfVcpu`] to
    /// create vCPUs from their dedicated threads.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn instance(&self) -> &VirtualMachineInstance<GicEnabled> {
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
