//! Live HVF backend for `squib_virtio::devices::mem::MemHotplugBackend`.
//!
//! virtio-mem plug/unplug events translate into stage-2 page-table mutations
//! that must reach HVF — the in-memory test backend the device crate ships
//! by default just records calls. This module bridges the trait to the live
//! `applevisor::Memory` API:
//!
//! - **plug**: `vm.instance().memory_create(len)` allocates a host-side `mmap` region;
//!   `mem.map(guest_base, MemPerms::RW)` installs the stage-2 mapping. The handle is held in
//!   `regions` map so future unplug calls can find it.
//! - **unplug**: look up the region by `guest_base`, call `mem.unmap()` on the inner
//!   `applevisor::Memory`, drop the `Arc<Mutex<Memory>>` so the host allocation goes away. The Drop
//!   impl on `Memory` would call `unmap` again as a safety net, but doing it explicitly surfaces
//!   any `HypervisorError` to the device caller (which translates it into the `RESP_NACK` response
//!   the guest's virtio-mem driver expects).
//!
//! Per [14-virtio-and-devices.md §
//! 4.7](../../../specs/14-virtio-and-devices.md#47-virtio-pmem-and-virtio-mem) and the I-DEV-4
//! amendment in [93-improvements-review.md], the device coalesces N contiguous block plug/unplug
//! calls into one backend call — one HVF stage-2 mutation per coalesced range. That's cheaper on
//! Apple Silicon than N per-block calls.
//!
//! ## Concurrency
//!
//! virtio-mem requests are processed serially by the device thread; only
//! one plug or unplug is in flight at a time. The internal `Mutex<HashMap>`
//! is therefore uncontended at runtime — it exists for `Send + Sync`
//! tropics and to defend against a future change that drives requests
//! from multiple threads.

#[cfg(target_os = "macos")]
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::sync::Arc;

#[cfg(target_os = "macos")]
use applevisor::memory::{MemPerms, Memory};
#[cfg(target_os = "macos")]
use parking_lot::Mutex;

use super::HvfVm;

/// Live HVF backend for virtio-mem hotplug.
///
/// Construct via [`Self::new`] with an `Arc<HvfVm>` the device manager already
/// holds. Pass the resulting `Arc<HvfMemBackend>` as the
/// `Arc<dyn squib_virtio::devices::mem::MemHotplugBackend>` argument to
/// `MemDevice::new`.
#[derive(Debug)]
pub struct HvfMemBackend {
    vm: Arc<HvfVm>,
    /// `guest_base → Arc<Mutex<Memory>>` per currently-plugged region. Each
    /// entry was created by a `plug` call and will be removed by the matching
    /// `unplug`. Keys are unique by construction (the device frontend never
    /// plugs the same range twice without unplugging it first).
    #[cfg(target_os = "macos")]
    regions: Mutex<HashMap<u64, Arc<Mutex<Memory>>>>,
}

// SAFETY: same rationale as the `Send`/`Sync` impls on `HvfVm` (see
// `vmm.rs`). `applevisor::Memory` carries a raw `*const c_void` host
// pointer that is plain memory shared across threads; the inner
// `parking_lot::Mutex<Memory>` serializes concurrent map/unmap.
// Sending the backend across threads preserves all HVF safety
// contracts because plug/unplug only touch the inner Memory through
// the mutex, never through the raw pointer.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
unsafe impl Send for HvfMemBackend {}
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
unsafe impl Sync for HvfMemBackend {}

#[cfg(target_os = "macos")]
impl HvfMemBackend {
    /// Build a fresh backend over `vm`. The backend holds an `Arc` to the VM
    /// so its lifetime is independent of the device-manager's other handles.
    #[must_use]
    pub fn new(vm: Arc<HvfVm>) -> Self {
        Self {
            vm,
            regions: Mutex::new(HashMap::new()),
        }
    }

    /// Number of currently-plugged regions (test helper).
    #[must_use]
    pub fn plugged_region_count(&self) -> usize {
        self.regions.lock().len()
    }

    fn plug_inner(&self, guest_base: u64, len: u64) -> Result<(), String> {
        let len_usize =
            usize::try_from(len).map_err(|_| format!("plug len {len} overflows usize"))?;
        let mut mem = self.vm.instance().memory_create(len_usize).map_err(|e| {
            format!("HvfMemBackend::plug({guest_base:#x}, {len}): memory_create: {e:?}")
        })?;
        mem.map(guest_base, MemPerms::RW)
            .map_err(|e| format!("HvfMemBackend::plug({guest_base:#x}, {len}): map: {e:?}"))?;
        let mut regions = self.regions.lock();
        if regions
            .insert(guest_base, Arc::new(Mutex::new(mem)))
            .is_some()
        {
            return Err(format!(
                "HvfMemBackend::plug: guest_base {guest_base:#x} already mapped (virtio-mem \
                 invariant violation)"
            ));
        }
        Ok(())
    }

    fn unplug_inner(&self, guest_base: u64, len: u64) -> Result<(), String> {
        let mut regions = self.regions.lock();
        let mem_arc = regions
            .remove(&guest_base)
            .ok_or_else(|| format!("HvfMemBackend::unplug({guest_base:#x}, {len}): not plugged"))?;
        // Drop the regions guard before locking the inner Memory mutex to
        // avoid holding two locks simultaneously (per CLAUDE.md § Async).
        drop(regions);
        let mut mem = mem_arc.lock();
        mem.unmap()
            .map_err(|e| format!("HvfMemBackend::unplug({guest_base:#x}, {len}): unmap: {e:?}"))?;
        // mem dropping here would call `unmap` again silently, but it's a
        // no-op because `unmap()` consumed `guest_addr`. Explicit drop just
        // for clarity.
        drop(mem);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl squib_virtio::devices::mem::MemHotplugBackend for HvfMemBackend {
    fn plug(&self, guest_base: u64, len: u64) -> Result<(), String> {
        self.plug_inner(guest_base, len)
    }
    fn unplug(&self, guest_base: u64, len: u64) -> Result<(), String> {
        self.unplug_inner(guest_base, len)
    }
}

/// Stub for non-macOS hosts. The trait still needs to be implementable so
/// `squib-vmm` compiles cross-platform; the live backend is only meaningful
/// on Apple Silicon.
#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub struct HvfMemBackend {
    _vm: Arc<HvfVm>,
}

#[cfg(not(target_os = "macos"))]
use std::sync::Arc;

#[cfg(not(target_os = "macos"))]
impl HvfMemBackend {
    /// Stub constructor on non-macOS hosts.
    #[must_use]
    pub fn new(vm: Arc<HvfVm>) -> Self {
        Self { _vm: vm }
    }

    /// Always 0 on the stub backend.
    #[must_use]
    pub const fn plugged_region_count(&self) -> usize {
        0
    }
}

#[cfg(not(target_os = "macos"))]
impl squib_virtio::devices::mem::MemHotplugBackend for HvfMemBackend {
    fn plug(&self, _guest_base: u64, _len: u64) -> Result<(), String> {
        Err("HvfMemBackend requires macOS".into())
    }
    fn unplug(&self, _guest_base: u64, _len: u64) -> Result<(), String> {
        Err("HvfMemBackend requires macOS".into())
    }
}
