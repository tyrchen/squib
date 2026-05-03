//! Resource description for VM construction.
//!
//! `VmResources` is the single value flowing into [`crate::build_microvm_for_boot`].
//! It is the in-memory shape of the configuration the API server / `--config-file`
//! replay path produces: validated, owned, and ready to consume. Everything in here
//! is squib-internal; no `Serialize` / `Deserialize` derives so wire-shape drift is
//! impossible by construction (the wire layer goes through `Raw<T>` → `T` `TryFrom`).

use std::path::PathBuf;

use squib_fdt::VirtioSlot;

/// Where the kernel image lives. The loader handles PE / raw / gz / zst detection.
#[derive(Debug, Clone)]
pub enum KernelSource {
    /// Path on the host filesystem.
    Path(PathBuf),
}

/// Where the initrd lives, if any.
#[derive(Debug, Clone)]
pub enum InitrdSource {
    /// Path on the host filesystem.
    Path(PathBuf),
}

/// Validated resources for `build_microvm_for_boot`.
#[derive(Debug, Clone)]
pub struct VmResources {
    /// Number of vCPUs (1..=32 per D19).
    pub vcpu_count: u32,
    /// Guest RAM size in MiB (caller validated against host RAM minus overhead).
    pub mem_size_mib: u64,
    /// Where the kernel image lives.
    pub kernel: KernelSource,
    /// Optional initrd.
    pub initrd: Option<InitrdSource>,
    /// User-provided `boot_args` from `/boot-source`. The FDT builder applies the D23
    /// composition rule (append `console=`, `panic=`, `root=PARTUUID=` if absent).
    pub boot_args: String,
    /// Optional root partition UUID — from the drive marked `is_root_device` if it
    /// carries a `partuuid`. Threaded through to the FDT for `root=PARTUUID=...`.
    pub root_partuuid: Option<String>,
    /// virtio-MMIO devices (slot allocation already chosen by the caller).
    pub virtio_devices: Vec<VirtioSlot>,
}

impl VmResources {
    /// `mem_size_mib` in bytes. Saturates at `u64::MAX`.
    #[must_use]
    pub fn mem_size_bytes(&self) -> u64 {
        self.mem_size_mib.saturating_mul(1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_size_bytes_converts_mib_correctly() {
        let res = VmResources {
            vcpu_count: 1,
            mem_size_mib: 256,
            kernel: KernelSource::Path("/dev/null".into()),
            initrd: None,
            boot_args: String::new(),
            root_partuuid: None,
            virtio_devices: Vec::new(),
        };
        assert_eq!(res.mem_size_bytes(), 256 * 1024 * 1024);
    }

    #[test]
    fn mem_size_bytes_saturates() {
        let res = VmResources {
            vcpu_count: 1,
            mem_size_mib: u64::MAX,
            kernel: KernelSource::Path("/dev/null".into()),
            initrd: None,
            boot_args: String::new(),
            root_partuuid: None,
            virtio_devices: Vec::new(),
        };
        assert_eq!(res.mem_size_bytes(), u64::MAX);
    }
}
