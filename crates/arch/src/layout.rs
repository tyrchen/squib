//! aarch64 memory layout — pinned constants for squib's microvm.
//!
//! The fixed layout is sized for the 32-vCPU worst case to defeat the GICR-vs-virtio-MMIO
//! overlap that an earlier draft introduced (D22). The const-evaluated overlap-check at
//! the bottom of this file ensures any future regression breaks the build, not the boot.
//!
//! See [13-arch-and-boot.md § 2](../../../specs/13-arch-and-boot.md#2-memory-layout-concrete)
//! and [99-key-decisions.md § D22](../../../specs/99-key-decisions.md#d22).

use squib_core::MAX_SUPPORTED_VCPUS;

/// MSI region base. HVF's GIC interception only kicks in once
/// distributor + redistributor + MSI are all configured (skipping MSI
/// causes HVF to silently treat GIC accesses as unmapped, which
/// surfaces as data aborts to the host). We park MSI well below the
/// GICD at a 16 MiB-aligned address. The MSI region itself is sized
/// by `hv_gic_get_msi_region_size` (typically 64 KiB on macOS 15+);
/// reserve 16 MiB of headroom in case Apple grows it.
pub const MSI_REGION_BASE: u64 = 0x0500_0000;

/// GIC distributor base address.
pub const GICD_BASE: u64 = 0x0800_0000;

/// Maximum GICD region we reserve. The live size is queried from
/// `hv_gic_get_distributor_size`; this is the placement-only cap.
pub const GICD_RESERVED_SIZE: u64 = 0x0001_0000;

/// GIC redistributor window base.
///
/// The live region is `[GICR_BASE, GICR_BASE + vcpu_count *
/// hv_gic_get_redistributor_size)`. The reserved window is sized for the worst case
/// (`MAX_SUPPORTED_VCPUS` × 128 KiB = 4 MiB) plus headroom up to `PL011_BASE`.
pub const GICR_BASE: u64 = 0x080A_0000;

/// End of the GICR reservation window. The live GICR live region must not exceed this.
pub const GICR_RESERVED_END: u64 = 0x0E0A_0000;

/// PL011 UART base. FDT SPI cell 1 → INTID 33, level-high.
pub const PL011_BASE: u64 = 0x0E0A_0000;

/// PL011 UART region size.
pub const PL011_SIZE: u64 = 0x1000;

/// virtio-MMIO region base.
///
/// 32 × 4 KiB slots. FDT SPI cells 16..47 → raw INTIDs 48..79 (edge-rising).
pub const VIRTIO_MMIO_BASE: u64 = 0x0F00_0000;

/// Total virtio-MMIO region size: 32 slots × 4 KiB.
pub const VIRTIO_MMIO_SIZE: u64 = 32 * 0x1000;

/// First DRAM byte. Matches Firecracker's `DRAM_MEM_START`.
pub const DRAM_BASE: u64 = 0x8000_0000;

/// Maximum DRAM extent — `0x00FF_8000_0000` matches upstream `DRAM_MEM_MAX_SIZE` of 1022 GiB
/// above `DRAM_BASE`.
pub const DRAM_MAX_END: u64 = 0x00FF_8000_0000 + DRAM_BASE;

/// Kernel image is loaded at `DRAM_BASE + KERNEL_LOAD_OFFSET`. Matches Firecracker's reserved
/// 2 MiB for system metadata at the head of DRAM.
pub const KERNEL_LOAD_OFFSET: u64 = 0x0020_0000;

/// Initrd start offset: `DRAM_BASE + 256 MiB`, or `kernel_end + 16 MiB` rounded up to the
/// 2 MiB grain — whichever is larger. The constant here is the lower bound; the builder
/// computes the actual offset against the kernel size.
pub const INITRD_FALLBACK_OFFSET: u64 = 0x1000_0000;

/// FDT region size — at most 2 MiB per `arm64/booting.rst`.
pub const FDT_MAX_SIZE: u64 = 0x0020_0000;

/// Worst-case GICR live size (`MAX_SUPPORTED_VCPUS × 128 KiB`).
///
/// The actual live size on the host is `vcpu_count × hv_gic_get_redistributor_size`,
/// which Apple may grow in a future macOS release. This constant is the floor that the
/// reserved layout window must accommodate.
pub const GICR_REDISTRIBUTOR_SIZE_PER_VCPU: u64 = 0x0002_0000;
const _GICR_LIVE_MAX: u64 = (MAX_SUPPORTED_VCPUS as u64) * GICR_REDISTRIBUTOR_SIZE_PER_VCPU;

// === Compile-time overlap check (I-AB-6 in 13 § 10) ===
//
// Any future revert that re-introduces the GICR/virtio overlap from the original
// draft breaks the build, not the boot. Three independent assertions:
//   1. The live GICR for 32 vCPUs fits inside the reserved [GICR_BASE, GICR_RESERVED_END).
//   2. PL011 sits exactly at GICR_RESERVED_END (no slack we'd silently lose).
//   3. virtio-MMIO sits above PL011 + PL011_SIZE.
const _: () = {
    assert!(
        GICR_BASE + _GICR_LIVE_MAX <= GICR_RESERVED_END,
        "GICR live region for 32 vCPUs overflows the reserved GICR window (D22)"
    );
    assert!(
        PL011_BASE == GICR_RESERVED_END,
        "PL011 must abut the GICR reservation (D22)"
    );
    assert!(
        VIRTIO_MMIO_BASE >= PL011_BASE + PL011_SIZE,
        "virtio-MMIO base must sit above PL011 (D22 — overlap regression)"
    );
    assert!(
        VIRTIO_MMIO_BASE + VIRTIO_MMIO_SIZE <= DRAM_BASE,
        "virtio-MMIO region overlaps DRAM (D22 — overlap regression)"
    );
};

/// Page geometry constants — three sizes that must never be conflated.
///
/// On Apple Silicon the host page is 16 KiB, the HVF stage-2 granule is 16 KiB, and the
/// dirty-tracking page defaults to 2 MiB with adaptive step-down to 16 KiB for hot regions.
/// See [99-key-decisions.md § D21](../../../specs/99-key-decisions.md#d21).
#[derive(Debug, Clone, Copy)]
pub struct PageGeometry {
    /// Apple Silicon host page (16 KiB) — `vm_page_size` returns this.
    pub host_page: u64,
    /// HVF stage-2 translation granule (16 KiB on Apple Silicon).
    pub hvf_stage2_granule: u64,
    /// Default tracking-page granularity for dirty-page bitmaps (2 MiB).
    pub tracking_page_default: u64,
    /// Step-down tracking page used by the adaptive heuristic for hot regions (16 KiB).
    pub tracking_page_hot: u64,
}

impl PageGeometry {
    /// The Apple-Silicon page geometry. macOS 15+ on M-series.
    pub const APPLE_SILICON: Self = Self {
        host_page: 16 * 1024,
        hvf_stage2_granule: 16 * 1024,
        tracking_page_default: 2 * 1024 * 1024,
        tracking_page_hot: 16 * 1024,
    };
}

/// Frozen memory layout for a microvm.
#[derive(Debug, Clone, Copy)]
pub struct MemoryLayout {
    /// GIC distributor base.
    pub gicd_base: u64,
    /// GIC redistributor window base.
    pub gicr_base: u64,
    /// PL011 UART base.
    pub pl011_base: u64,
    /// virtio-MMIO region base (slot 0).
    pub virtio_mmio_base: u64,
    /// virtio-MMIO slot stride (4 KiB).
    pub virtio_mmio_stride: u64,
    /// Number of virtio-MMIO slots reserved.
    pub virtio_mmio_slots: u32,
    /// DRAM start.
    pub dram_base: u64,
    /// Page geometry.
    pub page_geometry: PageGeometry,
}

/// Canonical squib memory layout. There is exactly one — D22 is "fixed".
pub const MEMORY_LAYOUT: MemoryLayout = MemoryLayout {
    gicd_base: GICD_BASE,
    gicr_base: GICR_BASE,
    pl011_base: PL011_BASE,
    virtio_mmio_base: VIRTIO_MMIO_BASE,
    virtio_mmio_stride: 0x1000,
    virtio_mmio_slots: 32,
    dram_base: DRAM_BASE,
    page_geometry: PageGeometry::APPLE_SILICON,
};

/// Result of [`overlap_check`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LayoutOverlap {
    /// The live GICR region fits within the reserved window for the given vCPU count.
    Ok,
    /// The live GICR region for `vcpu_count` would overlap the PL011/virtio-MMIO band.
    /// `live_gicr_end` is `GICR_BASE + vcpu_count × redistributor_size_per_vcpu`.
    GicrOverlapsMmio {
        /// vCPU count that triggered the overlap.
        vcpu_count: u32,
        /// Computed live GICR end address.
        live_gicr_end: u64,
        /// PL011 base (the boundary the live GICR must not cross).
        boundary: u64,
    },
}

/// Runtime layout overlap check used by `squib-vmm::builder` before `hv_gic_create`.
///
/// Pairs with the const overlap-check above. The const-check is an absolute floor
/// (32 vCPUs × the constant 128 KiB redistributor stride); the runtime check uses the
/// host-reported `redistributor_size_per_vcpu`, since Apple may bump the size in a future
/// macOS release.
///
/// # Errors
/// Surfaces a structured [`LayoutOverlap::GicrOverlapsMmio`] result if the layout cannot
/// fit `vcpu_count` redistributors in the reserved GICR window. This must be surfaced as
/// `Error::Config` to the API layer; the VMM must not call `hv_gic_create` after a
/// non-OK result.
#[must_use]
pub fn overlap_check(vcpu_count: u32, redistributor_size_per_vcpu: u64) -> LayoutOverlap {
    let live_end =
        GICR_BASE.saturating_add(u64::from(vcpu_count).saturating_mul(redistributor_size_per_vcpu));
    if live_end > PL011_BASE {
        LayoutOverlap::GicrOverlapsMmio {
            vcpu_count,
            live_gicr_end: live_end,
            boundary: PL011_BASE,
        }
    } else {
        LayoutOverlap::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d22_reserved_window_has_correct_size() {
        // 96 MiB — the headroom the spec calls out.
        assert_eq!(GICR_RESERVED_END - GICR_BASE, 96 * 1024 * 1024);
    }

    #[test]
    fn worst_case_gicr_fits_under_pl011() {
        // 32 vCPUs × 128 KiB = 4 MiB, well under the 96 MiB reservation.
        const { assert!(_GICR_LIVE_MAX < GICR_RESERVED_END - GICR_BASE) };
    }

    #[test]
    fn pl011_abuts_gicr_window() {
        assert_eq!(PL011_BASE, GICR_RESERVED_END);
    }

    #[test]
    fn virtio_mmio_does_not_overlap_pl011() {
        const { assert!(VIRTIO_MMIO_BASE >= PL011_BASE + PL011_SIZE) };
    }

    #[test]
    fn virtio_mmio_does_not_overlap_dram() {
        const { assert!(VIRTIO_MMIO_BASE + VIRTIO_MMIO_SIZE <= DRAM_BASE) };
    }

    #[test]
    fn page_geometry_apple_silicon_matches_d21() {
        let p = PageGeometry::APPLE_SILICON;
        assert_eq!(p.host_page, 16 * 1024);
        assert_eq!(p.hvf_stage2_granule, 16 * 1024);
        assert_eq!(p.tracking_page_default, 2 * 1024 * 1024);
        assert_eq!(p.tracking_page_hot, 16 * 1024);
    }

    #[test]
    fn overlap_check_passes_at_max_vcpus() {
        assert_eq!(
            overlap_check(MAX_SUPPORTED_VCPUS, 0x0002_0000),
            LayoutOverlap::Ok
        );
    }

    #[test]
    fn overlap_check_fails_if_apple_grows_redistributor_excessively() {
        // Hypothetical: Apple bumps redistributor stride to 4 MiB. 32 × 4 MiB = 128 MiB
        // > 96 MiB reservation, so the runtime check must reject before hv_gic_create.
        let result = overlap_check(MAX_SUPPORTED_VCPUS, 4 * 1024 * 1024);
        assert!(matches!(result, LayoutOverlap::GicrOverlapsMmio { .. }));
    }

    #[test]
    fn overlap_check_zero_vcpus_is_trivially_ok() {
        assert_eq!(overlap_check(0, 0x0002_0000), LayoutOverlap::Ok);
    }

    #[test]
    fn overlap_check_does_not_overflow_on_huge_redistributor_size() {
        // Saturating arithmetic must not panic on adversarial inputs.
        let result = overlap_check(MAX_SUPPORTED_VCPUS, u64::MAX);
        assert!(matches!(result, LayoutOverlap::GicrOverlapsMmio { .. }));
    }
}
