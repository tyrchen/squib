//! MMIO slot allocation for virtio-MMIO devices.
//!
//! Per [14-virtio-and-devices.md §
//! 5](../../../specs/14-virtio-and-devices.md#5-mmio-slot-allocation):
//!
//! - 32 slots total (D22 layout headroom).
//! - Each slot: `0x1000` bytes of MMIO at base `0x0F00_0000 + slot * 0x1000`.
//! - INTID per slot: `48 + slot` (FDT SPI cell `16 + slot`).
//!
//! The allocator hands out slots in insertion order; the per-class API caps
//! ([10-data-model.md § 2.3](../../../specs/10-data-model.md#23-schema-layer))
//! ensure a fully-loaded valid configuration consumes ≤ 28 slots, leaving
//! slack for diagnostic devices like `boot-timer` and runtime-added
//! virtio-mem regions.

use squib_arch::{IntId, IntIdError};
use thiserror::Error;

/// MMIO base for slot 0 (D22).
pub const VIRTIO_MMIO_BASE: u64 = 0x0F00_0000;
/// Per-slot region size in bytes.
pub const VIRTIO_MMIO_REGION_SIZE: u64 = 0x1000;
/// Hard cap on slots (D22 headroom).
pub const MMIO_SLOT_COUNT: u32 = 32;

/// Errors from the slot allocator.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SlotError {
    /// All 32 slots are taken; the per-class API caps prevent this for valid
    /// configurations.
    #[error("MMIO slot allocator exhausted (32 slots already taken)")]
    Exhausted,

    /// `IntId` derivation failed; should be impossible for `slot < 32` but
    /// surfaces the error rather than panicking.
    #[error(transparent)]
    IntId(#[from] IntIdError),
}

/// One allocated MMIO slot.
#[derive(Debug, Clone, Copy)]
pub struct Slot {
    /// Slot index (0..32).
    pub index: u32,
    /// Guest-physical MMIO base for this slot.
    pub base: u64,
    /// SPI INTID for the slot.
    pub intid: IntId,
}

impl Slot {
    /// Construct a slot from its index.
    ///
    /// # Errors
    /// - [`SlotError::Exhausted`] if `index >= 32`.
    /// - [`SlotError::IntId`] if INTID construction fails.
    pub fn from_index(index: u32) -> Result<Self, SlotError> {
        if index >= MMIO_SLOT_COUNT {
            return Err(SlotError::Exhausted);
        }
        let intid = IntId::from_spi_cell(16 + index)?;
        Ok(Self {
            index,
            base: VIRTIO_MMIO_BASE + u64::from(index) * VIRTIO_MMIO_REGION_SIZE,
            intid,
        })
    }
}

/// Sequential slot allocator. Hands out slots in insertion order.
#[derive(Debug, Default)]
pub struct SlotAllocator {
    next: u32,
}

impl SlotAllocator {
    /// Build an allocator starting at slot 0.
    #[must_use]
    pub fn new() -> Self {
        Self { next: 0 }
    }

    /// Allocate the next slot.
    ///
    /// # Errors
    /// [`SlotError::Exhausted`] if all 32 slots are already taken.
    pub fn allocate(&mut self) -> Result<Slot, SlotError> {
        let slot = Slot::from_index(self.next)?;
        self.next += 1;
        Ok(slot)
    }

    /// How many slots have been allocated so far.
    #[must_use]
    pub fn allocated(&self) -> u32 {
        self.next
    }

    /// How many slots remain.
    #[must_use]
    pub fn remaining(&self) -> u32 {
        MMIO_SLOT_COUNT - self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_assign_slot_0_to_base_0x0f00_0000_intid_48() {
        let s = Slot::from_index(0).unwrap();
        assert_eq!(s.base, 0x0F00_0000);
        assert_eq!(s.intid.as_raw(), 48);
    }

    #[test]
    fn test_should_assign_slot_1_to_base_0x0f00_1000_intid_49() {
        let s = Slot::from_index(1).unwrap();
        assert_eq!(s.base, 0x0F00_1000);
        assert_eq!(s.intid.as_raw(), 49);
    }

    #[test]
    fn test_should_assign_slot_31_to_base_0x0f01_f000_intid_79() {
        let s = Slot::from_index(31).unwrap();
        assert_eq!(s.base, 0x0F01_F000);
        assert_eq!(s.intid.as_raw(), 79);
    }

    #[test]
    fn test_should_reject_slot_above_ceiling() {
        let err = Slot::from_index(32).unwrap_err();
        assert!(matches!(err, SlotError::Exhausted));
    }

    #[test]
    fn test_should_allocate_sequentially_then_fail_after_32_takes() {
        let mut alloc = SlotAllocator::new();
        for expected in 0..32 {
            let s = alloc.allocate().unwrap();
            assert_eq!(s.index, expected);
        }
        assert_eq!(alloc.allocated(), 32);
        assert_eq!(alloc.remaining(), 0);
        assert!(matches!(alloc.allocate(), Err(SlotError::Exhausted)));
    }
}
