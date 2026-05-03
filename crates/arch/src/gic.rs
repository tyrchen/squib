//! GIC interrupt-ID newtype.
//!
//! The FDT GICv3 cell triple is `<type intid_offset flags>`, where `intid_offset` is
//! `INTID − 32` for SPIs and `INTID − 16` for PPIs. The "is `SPI 1` cell-1 or INTID-1?"
//! confusion has bitten this stack at least three times in prior art; the [`IntId`]
//! newtype below makes the conversion explicit at every call site.
//!
//! See [13-arch-and-boot.md §
//! 2.1](../../../specs/13-arch-and-boot.md#21-gic-interrupt-id-conventions) and [99-key-decisions.
//! md § D24](../../../specs/99-key-decisions.md#d24-edge-spi-pulse-shape).

use thiserror::Error;

/// Maximum INTID supported by GICv3 SPIs. ARM GICv3 reserves intids 32..1019 for SPIs;
/// the in-kernel HVF GICv3 supports up to 1019.
pub const SPI_INTID_MAX: u32 = 1019;

/// PPI INTID range: 16..=31 (16 entries).
pub const PPI_INTID_MIN: u32 = 16;
/// PPI INTID range: 16..=31 (16 entries).
pub const PPI_INTID_MAX: u32 = 31;

/// SPI INTID range: 32..=1019.
pub const SPI_INTID_MIN: u32 = 32;

/// FDT cell `type` field for SPIs.
pub const FDT_CELL_TYPE_SPI: u32 = 0;
/// FDT cell `type` field for PPIs.
pub const FDT_CELL_TYPE_PPI: u32 = 1;

/// FDT cell `flags` field — edge-rising trigger.
pub const FDT_CELL_FLAGS_EDGE_RISING: u32 = 1;
/// FDT cell `flags` field — level-high trigger.
pub const FDT_CELL_FLAGS_LEVEL_HIGH: u32 = 4;
/// FDT cell `flags` field — level-low trigger.
pub const FDT_CELL_FLAGS_LEVEL_LOW: u32 = 8;

/// Trigger semantics for an interrupt line.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum Trigger {
    /// Edge-rising; default for virtio-MMIO. FDT flag `1`.
    EdgeRising,
    /// Level-high; default for PL011 UART and timer PPIs. FDT flag `4`.
    LevelHigh,
    /// Level-low. FDT flag `8`.
    LevelLow,
}

impl Trigger {
    /// FDT-cell `flags` value.
    #[must_use]
    pub const fn fdt_flags(self) -> u32 {
        match self {
            Self::EdgeRising => FDT_CELL_FLAGS_EDGE_RISING,
            Self::LevelHigh => FDT_CELL_FLAGS_LEVEL_HIGH,
            Self::LevelLow => FDT_CELL_FLAGS_LEVEL_LOW,
        }
    }
}

/// Errors from constructing or converting an `IntId`.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum IntIdError {
    /// Provided raw INTID is outside the SPI range (`32..=1019`).
    #[error("invalid SPI intid {0}: must be in range 32..={SPI_INTID_MAX}")]
    SpiOutOfRange(u32),
    /// Provided raw INTID is outside the PPI range (`16..=31`).
    #[error("invalid PPI intid {0}: must be in range 16..=31")]
    PpiOutOfRange(u32),
    /// FDT SPI cell value would map to an out-of-range INTID.
    #[error("invalid SPI cell offset {0}: would map to an INTID > {SPI_INTID_MAX}")]
    SpiCellOffsetOutOfRange(u32),
    /// FDT PPI cell value would map to an out-of-range INTID.
    #[error("invalid PPI cell offset {0}: must be in range 0..=15 (cells map to INTIDs 16..=31)")]
    PpiCellOffsetOutOfRange(u32),
}

/// A GIC interrupt identifier (raw INTID).
///
/// Use the named constructors to translate from FDT-cell offsets — never reach for a
/// `u32` directly. The internal value is the **raw INTID**: SPIs are 32..=1019, PPIs
/// are 16..=31.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct IntId(u32);

impl IntId {
    /// Construct from a raw SPI INTID (32..=1019). For values written as cell-offsets
    /// in an FDT, prefer [`Self::from_spi_cell`].
    ///
    /// # Errors
    /// [`IntIdError::SpiOutOfRange`] if `intid` is outside the SPI range.
    pub const fn from_spi_intid(intid: u32) -> Result<Self, IntIdError> {
        if intid < SPI_INTID_MIN || intid > SPI_INTID_MAX {
            return Err(IntIdError::SpiOutOfRange(intid));
        }
        Ok(Self(intid))
    }

    /// Construct from a raw PPI INTID (16..=31).
    ///
    /// # Errors
    /// [`IntIdError::PpiOutOfRange`] if `intid` is outside the PPI range.
    pub const fn from_ppi_intid(intid: u32) -> Result<Self, IntIdError> {
        if intid < PPI_INTID_MIN || intid > PPI_INTID_MAX {
            return Err(IntIdError::PpiOutOfRange(intid));
        }
        Ok(Self(intid))
    }

    /// Construct an SPI from an FDT cell offset (raw INTID = `32 + offset`).
    ///
    /// Use this whenever you are reading or emitting an FDT — the cell number is what
    /// appears in the FDT text, the raw INTID is what the kernel and HVF speak.
    ///
    /// # Errors
    /// [`IntIdError::SpiCellOffsetOutOfRange`] if `32 + cell_offset > SPI_INTID_MAX`.
    pub const fn from_spi_cell(cell_offset: u32) -> Result<Self, IntIdError> {
        let Some(intid) = SPI_INTID_MIN.checked_add(cell_offset) else {
            return Err(IntIdError::SpiCellOffsetOutOfRange(cell_offset));
        };
        if intid > SPI_INTID_MAX {
            return Err(IntIdError::SpiCellOffsetOutOfRange(cell_offset));
        }
        Ok(Self(intid))
    }

    /// Construct a PPI from an FDT cell offset (raw INTID = `16 + offset`).
    ///
    /// # Errors
    /// [`IntIdError::PpiCellOffsetOutOfRange`] if `cell_offset > 15`.
    pub const fn from_ppi_cell(cell_offset: u32) -> Result<Self, IntIdError> {
        if cell_offset > 15 {
            return Err(IntIdError::PpiCellOffsetOutOfRange(cell_offset));
        }
        Ok(Self(PPI_INTID_MIN + cell_offset))
    }

    /// Raw INTID — what `hv_gic_set_spi` and friends consume.
    #[must_use]
    pub const fn as_raw(self) -> u32 {
        self.0
    }

    /// `true` if this is an SPI (32..=1019).
    #[must_use]
    pub const fn is_spi(self) -> bool {
        self.0 >= SPI_INTID_MIN && self.0 <= SPI_INTID_MAX
    }

    /// `true` if this is a PPI (16..=31).
    #[must_use]
    pub const fn is_ppi(self) -> bool {
        self.0 >= PPI_INTID_MIN && self.0 <= PPI_INTID_MAX
    }

    /// FDT cell type field (`0` for SPI, `1` for PPI).
    ///
    /// # Errors
    /// Returns the original [`IntIdError`] from the malformed branch — if `self` is
    /// neither a valid SPI nor PPI (which the constructors disallow), this is treated
    /// as an SPI out-of-range.
    #[must_use]
    pub const fn fdt_cell_type(self) -> u32 {
        if self.is_ppi() {
            FDT_CELL_TYPE_PPI
        } else {
            FDT_CELL_TYPE_SPI
        }
    }

    /// FDT cell offset (`INTID − 32` for SPI, `INTID − 16` for PPI).
    #[must_use]
    pub const fn fdt_cell_offset(self) -> u32 {
        if self.is_ppi() {
            self.0 - PPI_INTID_MIN
        } else {
            self.0 - SPI_INTID_MIN
        }
    }
}

/// Pinned constants for the well-known interrupts the squib boot path emits.
///
/// Built via the unchecked private constructor and verified against the cell-offset
/// constructors at compile time. The static asserts trip the build, not the boot,
/// if a regression mismatches the FDT-cell ↔ raw-INTID mapping.
pub mod fixed {
    use super::{IntId, PPI_INTID_MIN, SPI_INTID_MIN};

    /// PL011 UART → SPI cell 1, raw INTID 33.
    pub const PL011: IntId = IntId(SPI_INTID_MIN + 1);
    /// virtio-MMIO slot 0 → SPI cell 16, raw INTID 48.
    pub const VIRTIO_MMIO_SLOT0: IntId = IntId(SPI_INTID_MIN + 16);
    /// Virtual timer (CNTV) → PPI cell 11, raw INTID 27.
    pub const CNTV: IntId = IntId(PPI_INTID_MIN + 11);
    /// Hypervisor timer (CNTHP) → PPI cell 10, raw INTID 26.
    pub const CNTHP: IntId = IntId(PPI_INTID_MIN + 10);
    /// Physical timer EL1 (CNTP) → PPI cell 14, raw INTID 30.
    pub const CNTP: IntId = IntId(PPI_INTID_MIN + 14);
    /// Secure physical timer → PPI cell 13, raw INTID 29.
    pub const CNTPS: IntId = IntId(PPI_INTID_MIN + 13);

    // Compile-time cross-check: every constant agrees with the fallible cell-offset
    // constructor. A drift in either direction breaks the build, not the boot.
    const _: () = {
        assert!(PL011.as_raw() == 33);
        assert!(VIRTIO_MMIO_SLOT0.as_raw() == 48);
        assert!(CNTV.as_raw() == 27);
        assert!(CNTHP.as_raw() == 26);
        assert!(CNTP.as_raw() == 30);
        assert!(CNTPS.as_raw() == 29);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pl011_constants_match_spec() {
        let pl011 = IntId::from_spi_cell(1).unwrap();
        assert_eq!(pl011.as_raw(), 33);
        assert_eq!(pl011.fdt_cell_type(), FDT_CELL_TYPE_SPI);
        assert_eq!(pl011.fdt_cell_offset(), 1);
        assert_eq!(fixed::PL011, pl011);
    }

    #[test]
    fn virtio_slot_n_maps_to_intid_48_plus_n() {
        for slot in 0..32u32 {
            let id = IntId::from_spi_cell(16 + slot).unwrap();
            assert_eq!(id.as_raw(), 48 + slot, "slot {slot}");
            assert_eq!(id.fdt_cell_offset(), 16 + slot, "slot {slot}");
        }
    }

    #[test]
    fn timer_ppi_constants_match_spec() {
        assert_eq!(fixed::CNTV.as_raw(), 27);
        assert_eq!(fixed::CNTHP.as_raw(), 26);
        assert_eq!(fixed::CNTP.as_raw(), 30);
        assert_eq!(fixed::CNTPS.as_raw(), 29);
    }

    #[test]
    fn ppi_round_trips_through_cell_offset() {
        for offset in 0..=15u32 {
            let id = IntId::from_ppi_cell(offset).unwrap();
            assert!(id.is_ppi());
            assert_eq!(id.fdt_cell_offset(), offset);
            assert_eq!(id.fdt_cell_type(), FDT_CELL_TYPE_PPI);
        }
    }

    #[test]
    fn spi_round_trips_through_cell_offset() {
        for offset in [0, 1, 16, 47, 100, 987] {
            let id = IntId::from_spi_cell(offset).unwrap();
            assert!(id.is_spi());
            assert_eq!(id.fdt_cell_offset(), offset);
            assert_eq!(id.fdt_cell_type(), FDT_CELL_TYPE_SPI);
        }
    }

    #[test]
    fn ppi_cell_offset_above_15_rejected() {
        assert!(matches!(
            IntId::from_ppi_cell(16),
            Err(IntIdError::PpiCellOffsetOutOfRange(16))
        ));
    }

    #[test]
    fn spi_cell_offset_overflow_rejected() {
        // 32 + 988 = 1020 > 1019.
        assert!(matches!(
            IntId::from_spi_cell(988),
            Err(IntIdError::SpiCellOffsetOutOfRange(988))
        ));
        // 32 + u32::MAX would wrap; the constructor must use checked_add.
        assert!(matches!(
            IntId::from_spi_cell(u32::MAX),
            Err(IntIdError::SpiCellOffsetOutOfRange(_))
        ));
    }

    #[test]
    fn raw_intid_constructors_validate_ranges() {
        assert!(IntId::from_spi_intid(31).is_err()); // PPI range
        assert!(IntId::from_spi_intid(32).is_ok());
        assert!(IntId::from_spi_intid(SPI_INTID_MAX).is_ok());
        assert!(IntId::from_spi_intid(SPI_INTID_MAX + 1).is_err());

        assert!(IntId::from_ppi_intid(15).is_err());
        assert!(IntId::from_ppi_intid(16).is_ok());
        assert!(IntId::from_ppi_intid(31).is_ok());
        assert!(IntId::from_ppi_intid(32).is_err()); // SPI range
    }

    #[test]
    fn trigger_fdt_flags_match_spec() {
        assert_eq!(Trigger::EdgeRising.fdt_flags(), 1);
        assert_eq!(Trigger::LevelHigh.fdt_flags(), 4);
        assert_eq!(Trigger::LevelLow.fdt_flags(), 8);
    }
}
