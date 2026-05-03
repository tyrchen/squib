//! The hypervisor backend trait surface.
//!
//! `squib-vz` and `squib-hvf` each implement [`HypervisorBackend`] for their respective
//! macOS frameworks. The VMM crate is generic over `dyn HypervisorBackend` so the two
//! backends can be swapped at runtime via the `--hypervisor` CLI flag.

use crate::{
    error::Result,
    memory::{GuestMemoryRegion, GuestRange, Protection},
    vcpu::Vcpu,
};

/// Identifies the concrete backend implementation reporting [`BackendCapabilities`].
///
/// Squib ships exactly one production backend (HVF on Apple Silicon). The `Mock` variant
/// exists so the trait surface can be unit-tested without a live hypervisor. The enum is
/// `#[non_exhaustive]` to leave room for a future Apple replacement API without breaking
/// downstream callers.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
pub enum BackendKind {
    /// Apple Hypervisor.framework on Apple Silicon — squib's only production backend.
    Hvf,
    /// In-process mock backend used for tests; never selectable from the CLI.
    Mock,
}

/// What a backend can and cannot do, surfaced to the VMM at construction time.
///
/// The VMM consults this struct to decide whether to reject a configuration up-front
/// (e.g. `track_dirty_pages: true` on the VZ backend) or to apply a documented fallback.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)] // each bool is an independent capability flag
pub struct BackendCapabilities {
    /// Which backend produced these capabilities.
    pub kind: BackendKind,
    /// Maximum number of vCPUs this backend will allow per VM.
    pub max_vcpus: u32,
    /// `true` if the backend can produce a per-page dirty bitmap.
    pub dirty_page_tracking: bool,
    /// `true` if the backend can perform a postcopy / on-demand-paging restore.
    pub postcopy_restore: bool,
    /// `true` if the backend exposes vCPU exits to userspace (HVF/KVM-style); `false` if
    /// the device model is hidden inside the framework (VZ-style).
    pub exposes_vcpu_exits: bool,
    /// `true` if the backend supports custom virtio-MMIO devices.
    pub custom_mmio_devices: bool,
}

/// A live virtual machine handle owned by a [`HypervisorBackend`].
///
/// The trait is split from `HypervisorBackend` because a backend may host multiple VMs
/// (in principle) and because vCPU lifetimes are scoped to a single `Vm`.
pub trait Vm: Send + Sync {
    /// Map a region of guest-physical memory.
    fn map_memory(&self, region: &GuestMemoryRegion) -> Result<()>;

    /// Unmap a previously-mapped region.
    fn unmap_memory(&self, region: &GuestMemoryRegion) -> Result<()>;

    /// Change the protection of an already-mapped range. Used for software dirty-page
    /// tracking on backends without a native dirty bitmap.
    fn protect_memory(&self, range: GuestRange, prot: Protection) -> Result<()>;

    /// Create a vCPU with the given index.
    ///
    /// The returned `Vcpu` must be driven from a dedicated OS thread (HVF rule); the
    /// backend is permitted to enforce that by panicking on misuse, but callers should
    /// never depend on that.
    fn create_vcpu(&self, index: u32) -> Result<Box<dyn Vcpu>>;
}

/// The top-level entry point: a backend that can create VMs.
pub trait HypervisorBackend: Send + Sync {
    /// The concrete VM type this backend produces.
    type Vm: Vm;

    /// Returns the capabilities advertised by this backend on the current host.
    fn capabilities(&self) -> BackendCapabilities;

    /// Construct a new VM with `vcpu_count` vCPUs and `mem_size_mib` MiB of guest memory.
    ///
    /// Memory is not mapped at this point — the caller drives [`Vm::map_memory`] for each
    /// region. This split keeps the trait surface compatible with both KVM-style
    /// "register a slot" and HVF/VZ-style "hand the framework an mmap" backends.
    fn create_vm(&self, vcpu_count: u32, mem_size_mib: u64) -> Result<Self::Vm>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_struct_constructs() {
        let caps = BackendCapabilities {
            kind: BackendKind::Mock,
            max_vcpus: 4,
            dirty_page_tracking: false,
            postcopy_restore: false,
            exposes_vcpu_exits: true,
            custom_mmio_devices: true,
        };
        assert_eq!(caps.max_vcpus, 4);
    }
}
