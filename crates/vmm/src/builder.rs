//! Boot orchestration — `build_microvm_for_boot`.
//!
//! The single boot entry point per
//! [13-arch-and-boot.md § 9](../../../specs/13-arch-and-boot.md#9-boot-orchestration):
//!
//! ```text
//! 1. Resolve vcpu_count, mem_size_mib, kernel_path, initrd_path, boot_args.
//! 2. Open kernel; auto-detect compression; decompress as needed.
//! 3. Parse boot header; resolve text_offset; compute kernel_load_addr.
//! 4. mmap guest memory (anonymous, RWX); register with HVF via Vm::map_memory.
//! 5. Load kernel via squib-loader.
//! 6. If initrd: write initrd at 0x9000_0000 (or kernel_end + 256 MiB, whichever larger).
//! 7. Build FDT in last 2 MiB of RAM (squib-fdt).
//! 8. Vm::create_gic(vcpu_count); place GICD/GICR at fixed addresses.
//! 9. Create vCPUs; each spawns its OS thread, parks at CPU_OFF except vCPU 0.
//! 10. On vCPU 0: set_boot_regs(kernel_load_addr, fdt_addr).
//! 11. Return (vcpus, devices, mmds, gic, snapshot_handle).
//! ```
//!
//! Phase 1.6 lands the orchestrator skeleton: kernel loading, FDT composition, layout
//! verification, memory plan, and the boot-register triple. Phase 3+ wire devices and
//! Phase 5 wires snapshots into this same orchestrator. The HVF-side `mmap` + vCPU
//! spawn happens behind the `target_os = "macos"` cfg; the planning artifacts surfaced
//! by [`BootArtifacts`] let non-Apple-Silicon CI nodes still validate everything
//! except the live HVF calls.

use std::path::Path;

use squib_arch::{
    BootRegs,
    layout::{DRAM_BASE, FDT_MAX_SIZE, INITRD_FALLBACK_OFFSET},
};
use squib_core::{Error as CoreError, MAX_SUPPORTED_VCPUS};
use squib_fdt::{FdtBuildArgs, InitrdRange, MemoryRegion};
use squib_loader::{LoadedKernel, MaxSizes, load_from_path_with_caps};
use thiserror::Error;

use crate::resources::{InitrdSource, KernelSource, VmResources};

/// Errors that can surface from the boot builder.
#[derive(Debug, Error)]
pub enum BootError {
    /// `vcpu_count` is outside the valid range (`1..=32`).
    #[error("vcpu_count {0} is outside the valid range (1..={MAX_SUPPORTED_VCPUS})")]
    InvalidVcpuCount(u32),

    /// Configured RAM size is below the minimum that fits the kernel + reserved bands.
    #[error("mem_size_mib too small: kernel needs at least {needed_mib} MiB, got {actual_mib}")]
    RamTooSmall {
        /// Minimum required size in MiB.
        needed_mib: u64,
        /// Configured size in MiB.
        actual_mib: u64,
    },

    /// Loader-side failure (I/O, magic, decompression bomb).
    #[error("kernel loader error: {0}")]
    Loader(String),

    /// FDT-build failure.
    #[error("FDT build error: {0}")]
    Fdt(String),

    /// HVF-side failure during memory map / vCPU create / GIC create.
    #[error("HVF error: {0}")]
    Hvf(String),

    /// Initrd would not fit between the kernel end and the FDT region.
    #[error(
        "initrd does not fit (size {size}, kernel_end {kernel_end:#x}, fdt_base {fdt_base:#x})"
    )]
    InitrdNoFit {
        /// Initrd size.
        size: u64,
        /// Kernel end address.
        kernel_end: u64,
        /// FDT base address.
        fdt_base: u64,
    },

    /// I/O error reading initrd.
    #[error("initrd I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<squib_loader::LoaderError> for BootError {
    fn from(err: squib_loader::LoaderError) -> Self {
        Self::Loader(err.to_string())
    }
}

impl From<squib_fdt::FdtError> for BootError {
    fn from(err: squib_fdt::FdtError) -> Self {
        Self::Fdt(err.to_string())
    }
}

#[cfg(target_os = "macos")]
impl From<squib_hv::vmm::InitError> for BootError {
    fn from(err: squib_hv::vmm::InitError) -> Self {
        Self::Hvf(err.to_string())
    }
}

/// What the orchestrator returns on a successful boot.
///
/// Phase 1.6 ships the planning artifacts (loaded kernel, FDT bytes, computed
/// addresses); the HVF-live handle (`HvfVm`, `HvfVcpu`s, `Gic`) is populated only on
/// macOS targets. Later phases extend this struct with device handles and the snapshot
/// handle.
#[derive(Debug)]
pub struct BootArtifacts {
    /// The loaded (decompressed) kernel image.
    pub kernel: LoadedKernel,
    /// Computed guest-physical address where the kernel image is loaded.
    pub kernel_load_addr: u64,
    /// FDT bytes ready to be written into guest RAM at [`Self::fdt_base`].
    pub fdt_bytes: Vec<u8>,
    /// Guest-physical FDT base. Always in the last 2 MiB of RAM, 8-byte aligned.
    pub fdt_base: u64,
    /// Initrd plan (start + end), if one was configured.
    pub initrd_range: Option<InitrdRange>,
    /// Boot register triple for vCPU 0.
    pub boot_regs: BootRegs,
    /// The vmm-internal computed boot-args (D23 applied).
    pub effective_boot_args: String,
    /// Plan-only: live HVF handles only on macOS targets.
    #[cfg(target_os = "macos")]
    pub hvf_vm: Option<squib_hv::HvfVm>,
}

/// Build the planning artifacts for a boot. On macOS this also creates the live HVF VM
/// and maps guest memory; on non-macOS hosts everything except the HVF calls runs and
/// the result still validates the configuration.
///
/// This function does **not** spawn vCPU threads or write the kernel into guest RAM;
/// those steps belong on the macOS HVF path and require host hardware to verify.
/// Phase 1.6 lands the skeleton; the post-1.6 `boot_microvm` (vCPU thread spawn +
/// register init + run loop) extends this same flow.
///
/// # Errors
/// [`BootError`] for any input or HVF-side validation failure.
pub fn build_microvm_for_boot(resources: &VmResources) -> Result<BootArtifacts, BootError> {
    if resources.vcpu_count == 0 || resources.vcpu_count > MAX_SUPPORTED_VCPUS {
        return Err(BootError::InvalidVcpuCount(resources.vcpu_count));
    }

    // 2 + 3. Load kernel.
    let kernel_path = match &resources.kernel {
        KernelSource::Path(p) => p.clone(),
    };
    let kernel = load_from_path_with_caps(&kernel_path, MaxSizes::default())?;

    // Validate the kernel fits in RAM and compute the load address.
    let mem_bytes = resources.mem_size_bytes();
    let kernel_load_addr = kernel
        .check_fits_in_ram(mem_bytes)
        .map_err(BootError::from)?;
    let kernel_end = kernel_load_addr.saturating_add(kernel.image_bytes());

    // 5/6. Plan initrd placement.
    let initrd_range = if let Some(initrd) = &resources.initrd {
        Some(plan_initrd_range(initrd, mem_bytes, kernel_end)?)
    } else {
        None
    };

    // 7. Plan FDT placement: last 2 MiB of RAM, 8-byte aligned. We compute the base
    // first so we can carry it into the boot regs.
    let ram_end = DRAM_BASE.saturating_add(mem_bytes);
    let fdt_base = ram_end - FDT_MAX_SIZE;

    // Reject if FDT would overlap initrd.
    if let Some(range) = initrd_range
        && range.end > fdt_base
    {
        return Err(BootError::InitrdNoFit {
            size: range.end - range.start,
            kernel_end,
            fdt_base,
        });
    }

    // 7. Build FDT.
    let memory_region = MemoryRegion::dram(mem_bytes);
    let effective_boot_args =
        squib_fdt::compose_boot_args(&resources.boot_args, resources.root_partuuid.as_deref());
    let fdt_bytes = squib_fdt::build(&FdtBuildArgs::new(
        resources.vcpu_count,
        memory_region,
        &effective_boot_args,
        initrd_range,
        &resources.virtio_devices,
    ))?;
    if (fdt_bytes.len() as u64) > FDT_MAX_SIZE {
        return Err(BootError::Fdt(format!(
            "FDT exceeds the 2 MiB cap: {} bytes",
            fdt_bytes.len()
        )));
    }

    // 10. Boot regs for vCPU 0.
    let boot_regs = BootRegs::new(kernel_load_addr, fdt_base);

    // 4 + 8 + 9 — HVF-side init runs only on macOS.
    #[cfg(target_os = "macos")]
    let hvf_vm = init_hvf_path(
        resources,
        mem_bytes,
        &kernel,
        kernel_load_addr,
        fdt_base,
        &fdt_bytes,
    )?;

    Ok(BootArtifacts {
        kernel,
        kernel_load_addr,
        fdt_bytes,
        fdt_base,
        initrd_range,
        boot_regs,
        effective_boot_args,
        #[cfg(target_os = "macos")]
        hvf_vm,
    })
}

/// macOS-only: bring up the HVF VM, configure the GIC, and map guest memory.
///
/// vCPU thread spawn + actual kernel write into guest RAM are deliberately deferred to
/// the post-Phase-1.6 boot driver — those steps require host hardware verification and
/// will land alongside the first kernel-boot smoke test.
#[cfg(target_os = "macos")]
fn init_hvf_path(
    resources: &VmResources,
    mem_bytes: u64,
    _kernel: &LoadedKernel,
    _kernel_load_addr: u64,
    _fdt_base: u64,
    _fdt_bytes: &[u8],
) -> Result<Option<squib_hv::HvfVm>, BootError> {
    use applevisor::memory::MemPerms;
    use squib_gic::GicSizes;
    use squib_hv::HvfHypervisor;

    let sizes =
        GicSizes::query().map_err(|e| BootError::Hvf(format!("GicSizes::query failed: {e}")))?;
    let hv = HvfHypervisor::new();
    let vm = hv.init_vm(resources.vcpu_count, sizes.redistributor_per_vcpu)?;

    // Map a single region for DRAM. Real-world configs may split into multiple
    // regions later; one region is sufficient for the boot-path skeleton. The
    // returned MappedRegion is the only handle to the host-side mapping; the raw
    // pointer never crosses the squib-hv crate boundary.
    let mem_bytes_usize = usize::try_from(mem_bytes).map_err(|_| BootError::RamTooSmall {
        needed_mib: 0,
        actual_mib: resources.mem_size_mib,
    })?;
    let _region = vm
        .map_memory(DRAM_BASE, mem_bytes_usize, MemPerms::RWX)
        .map_err(BootError::from)?;

    // The kernel + FDT writes happen through the host-mapped pointer in the post-1.6
    // boot driver. Wiring them here would force the function to handle a `*mut u8`,
    // and Phase 1.6 deliberately keeps that surface narrow.

    Ok(Some(vm))
}

/// Compute the initrd `[start, end)` range in guest physical addresses.
///
/// Spec: "DRAM+256 MiB or kernel_end + 16 MiB rounded up to 2 MiB-aligned, whichever is
/// larger". Errors if the initrd cannot fit between `kernel_end` and `ram_end -
/// FDT_MAX_SIZE`.
fn plan_initrd_range(
    initrd: &InitrdSource,
    mem_bytes: u64,
    kernel_end: u64,
) -> Result<InitrdRange, BootError> {
    let InitrdSource::Path(path) = initrd;
    let path: &Path = path;
    let metadata = std::fs::metadata(path)?;
    let size = metadata.len();
    let fallback = DRAM_BASE.saturating_add(INITRD_FALLBACK_OFFSET);
    let aligned_after_kernel = align_up_2mib(kernel_end.saturating_add(16 * 1024 * 1024));
    let start = fallback.max(aligned_after_kernel);
    let end = start.saturating_add(size);

    let ram_end = DRAM_BASE.saturating_add(mem_bytes);
    let fdt_base = ram_end - FDT_MAX_SIZE;
    if end > fdt_base {
        return Err(BootError::InitrdNoFit {
            size,
            kernel_end,
            fdt_base,
        });
    }

    Ok(InitrdRange { start, end })
}

fn align_up_2mib(addr: u64) -> u64 {
    const ALIGN: u64 = 2 * 1024 * 1024;
    addr.saturating_add(ALIGN - 1) & !(ALIGN - 1)
}

/// Squib-internal: surface a [`BootError`] as a `squib_core::Error::Config` for the API.
impl From<BootError> for CoreError {
    fn from(err: BootError) -> Self {
        Self::InvalidArgument(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;

    fn synth_image(text_offset: u64, image_size: u64, payload_len: usize) -> Vec<u8> {
        let mut img = vec![0u8; payload_len.max(64)];
        img[8..16].copy_from_slice(&text_offset.to_le_bytes());
        img[16..24].copy_from_slice(&image_size.to_le_bytes());
        img[0x38..0x3C].copy_from_slice(b"ARM\x64");
        img
    }

    fn write_tmp(prefix: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("squib-vmm-test-{prefix}.bin"));
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn rejects_zero_vcpus() {
        let kernel = synth_image(0x80000, 0, 256);
        let path = write_tmp("zero-vcpu", &kernel);
        let res = VmResources {
            vcpu_count: 0,
            mem_size_mib: 128,
            kernel: KernelSource::Path(path),
            initrd: None,
            boot_args: String::new(),
            root_partuuid: None,
            virtio_devices: Vec::new(),
        };
        let err = build_microvm_for_boot(&res).unwrap_err();
        assert!(matches!(err, BootError::InvalidVcpuCount(0)));
    }

    #[test]
    fn rejects_vcpu_count_above_max() {
        let kernel = synth_image(0x80000, 0, 256);
        let path = write_tmp("over-max-vcpu", &kernel);
        let res = VmResources {
            vcpu_count: MAX_SUPPORTED_VCPUS + 1,
            mem_size_mib: 128,
            kernel: KernelSource::Path(path),
            initrd: None,
            boot_args: String::new(),
            root_partuuid: None,
            virtio_devices: Vec::new(),
        };
        let err = build_microvm_for_boot(&res).unwrap_err();
        assert!(matches!(
            err,
            BootError::InvalidVcpuCount(c) if c == MAX_SUPPORTED_VCPUS + 1
        ));
    }

    #[test]
    fn align_up_2mib_rounds_correctly() {
        assert_eq!(align_up_2mib(0), 0);
        assert_eq!(align_up_2mib(1), 2 * 1024 * 1024);
        assert_eq!(align_up_2mib(2 * 1024 * 1024), 2 * 1024 * 1024);
        assert_eq!(align_up_2mib(2 * 1024 * 1024 + 1), 4 * 1024 * 1024);
    }

    /// On non-macOS hosts we can fully validate the planning artifacts without HVF.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn planning_artifacts_for_minimal_boot_compose_correctly() {
        let kernel = synth_image(0x80000, 0x100_0000, 0x100_0000);
        let path = write_tmp("minimal-boot", &kernel);
        let res = VmResources {
            vcpu_count: 1,
            mem_size_mib: 128,
            kernel: KernelSource::Path(path),
            initrd: None,
            boot_args: "quiet".into(),
            root_partuuid: None,
            virtio_devices: Vec::new(),
        };
        let artifacts = build_microvm_for_boot(&res).unwrap();
        assert_eq!(artifacts.kernel_load_addr, DRAM_BASE + 0x20_0000);
        assert!(u64::try_from(artifacts.fdt_bytes.len()).is_ok_and(|len| len < FDT_MAX_SIZE));
        // FDT base = ram_end - 2 MiB.
        assert_eq!(
            artifacts.fdt_base,
            DRAM_BASE + 128 * 1024 * 1024 - FDT_MAX_SIZE
        );
        // Boot regs put kernel_load_addr in PC and FDT base in X0.
        assert_eq!(
            artifacts.boot_regs.kernel_load_addr,
            artifacts.kernel_load_addr
        );
        assert_eq!(artifacts.boot_regs.fdt_addr, artifacts.fdt_base);
        assert!(artifacts.effective_boot_args.contains("console=ttyAMA0"));
        assert!(artifacts.effective_boot_args.contains("panic=1"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn ram_too_small_is_rejected_via_loader() {
        // Kernel image_size = 256 MiB; configured RAM = 64 MiB.
        let kernel = synth_image(0x80_0000, 256 * 1024 * 1024, 256);
        let path = write_tmp("ram-too-small", &kernel);
        let res = VmResources {
            vcpu_count: 1,
            mem_size_mib: 64,
            kernel: KernelSource::Path(path),
            initrd: None,
            boot_args: String::new(),
            root_partuuid: None,
            virtio_devices: Vec::new(),
        };
        let err = build_microvm_for_boot(&res).unwrap_err();
        assert!(matches!(err, BootError::Loader(_)));
    }
}
