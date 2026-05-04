//! MMIO address-space router for squib's microvm.
//!
//! `Bus` owns a set of devices keyed by half-open address ranges and dispatches
//! `read` / `write` calls (sourced from the VMM exit handler) to the device that
//! owns the target range. Per
//! [14-virtio-and-devices.md § 2](../../../specs/14-virtio-and-devices.md#2-bus),
//! the underlying container is `BTreeMap<BusRange, Arc<Mutex<dyn BusDevice>>>`:
//! cache-locality on lookup beats `DashMap` here because inserts happen at boot
//! and lookups dominate the hot path.
//!
//! ## Invariants
//!
//! - **No overlap.** `Bus::insert` rejects any range that overlaps an existing range with
//!   [`BusError::Overlap`]. The MMIO map is fully bounded at boot; regressions here would mean two
//!   devices fighting for the same MMIO BAR.
//! - **Single owner per dispatch.** Each `read` / `write` is forwarded to *one* device under its
//!   mutex; cross-device traffic is impossible by construction (devices serialize their own queues,
//!   the bus serializes by address range). This pins I-DEV-2 from the spec: the bus dispatches
//!   reads/writes to exactly one device or returns [`BusError::NoDevice`], which the upstream MMIO
//!   exit handler surfaces to the guest as a data abort.
//!
//! ## Concurrency
//!
//! Per CLAUDE.md `§ Async & Concurrency`, we considered `DashMap` for the
//! device map; the access pattern (rare insert at boot, frequent lookup)
//! favours `BTreeMap` for cache locality and ordered range queries. The
//! per-device `Mutex` is `parking_lot::Mutex` because we never `Send` a guard
//! across an await point and want the cheaper, no-poison semantics.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod range;

use std::{collections::BTreeMap, fmt, sync::Arc};

use parking_lot::{Mutex, RwLock};
use thiserror::Error;

pub use crate::range::BusRange;

/// Trait for devices that respond to MMIO reads or writes within a bounded
/// address range.
///
/// Implementors only see the offset relative to their range base; the bus is
/// responsible for translation. `read` / `write` MUST NOT block on guest-driven
/// I/O — the device thread holds the per-device mutex while running the
/// handler, and a blocked handler stalls every other guest exit on that bus.
pub trait BusDevice: Send + fmt::Debug {
    /// Handle a guest read at `offset` (relative to the device's range base).
    ///
    /// The device fills `data` with little-endian bytes. Reads outside the
    /// device's documented register layout MUST be silently zero-filled — this
    /// matches QEMU and Firecracker behaviour and avoids panicking on guest
    /// driver probes.
    fn read(&mut self, offset: u64, data: &mut [u8]);

    /// Handle a guest write at `offset` (relative to the device's range base).
    ///
    /// Writes outside the documented register layout MUST be silently dropped
    /// for the same reason as `read`.
    fn write(&mut self, offset: u64, data: &[u8]);

    /// Human-readable label for log lines and debug dumps.
    fn debug_label(&self) -> &str;
}

/// Errors produced by [`Bus`] operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BusError {
    /// The proposed range overlaps an already-registered range.
    #[error("bus: overlap with existing range at {existing}")]
    Overlap {
        /// The existing range that would be overlapped.
        existing: BusRange,
    },
    /// `insert` was given a zero-length range.
    #[error("bus: zero-length range is invalid")]
    ZeroLengthRange,
    /// `insert` was given a range that overflows `u64`.
    #[error("bus: range overflow (base + len > u64::MAX)")]
    RangeOverflow,
    /// `read` / `write` addressed a region that is not registered with the bus.
    /// Surfaced to the guest as a data abort by the MMIO exit handler.
    #[error("bus: no device covers address {addr:#x}")]
    NoDevice {
        /// The unmapped guest-physical address.
        addr: u64,
    },
}

/// MMIO address-space router.
///
/// See the crate-level documentation for the design contract; in short, this
/// is a `BTreeMap<BusRange, Arc<Mutex<dyn BusDevice>>>` with overlap rejection
/// on insert and a `range(..=key).next_back()` lookup on dispatch.
#[derive(Debug, Default)]
pub struct Bus {
    devices: RwLock<BTreeMap<BusRange, Arc<Mutex<dyn BusDevice>>>>,
}

impl Bus {
    /// Build an empty bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `device` to handle reads / writes in `[base, base + len)`.
    ///
    /// # Errors
    /// - [`BusError::ZeroLengthRange`] if `len == 0`.
    /// - [`BusError::RangeOverflow`] if `base + len` overflows.
    /// - [`BusError::Overlap`] if the new range overlaps an existing one.
    pub fn insert(
        &self,
        device: Arc<Mutex<dyn BusDevice>>,
        base: u64,
        len: u64,
    ) -> Result<(), BusError> {
        let new_range = BusRange::new(base, len)?;
        let mut guard = self.devices.write();
        for existing in guard.keys() {
            if existing.overlaps(&new_range) {
                return Err(BusError::Overlap {
                    existing: *existing,
                });
            }
        }
        // Insert is unique by construction: the overlap check above also
        // catches exact-same-range duplicates.
        guard.insert(new_range, device);
        Ok(())
    }

    /// Number of devices currently registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.devices.read().len()
    }

    /// `true` if no devices are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.devices.read().is_empty()
    }

    /// Forward a read at `addr` to the device that owns the covering range.
    ///
    /// The data slice is filled by the device handler; on `NoDevice` the slice
    /// is left untouched.
    ///
    /// # Errors
    /// [`BusError::NoDevice`] if no registered device covers `addr`.
    pub fn read(&self, addr: u64, data: &mut [u8]) -> Result<(), BusError> {
        let (offset, dev) = self.resolve(addr)?;
        dev.lock().read(offset, data);
        Ok(())
    }

    /// Forward a write at `addr` to the device that owns the covering range.
    ///
    /// # Errors
    /// [`BusError::NoDevice`] if no registered device covers `addr`.
    pub fn write(&self, addr: u64, data: &[u8]) -> Result<(), BusError> {
        let (offset, dev) = self.resolve(addr)?;
        dev.lock().write(offset, data);
        Ok(())
    }

    /// Look up the device covering `addr`. Returns `(offset_within_range,
    /// device_handle)` on success.
    fn resolve(&self, addr: u64) -> Result<(u64, Arc<Mutex<dyn BusDevice>>), BusError> {
        let guard = self.devices.read();
        // A range whose base is <= addr is a candidate; the largest such base
        // is the only one that can contain addr (BTreeMap is sorted by base).
        let probe = BusRange::probe(addr);
        let (range, dev) = guard
            .range(..=probe)
            .next_back()
            .ok_or(BusError::NoDevice { addr })?;
        if range.contains(addr) {
            Ok((addr - range.base(), dev.clone()))
        } else {
            Err(BusError::NoDevice { addr })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test device that records every read / write call so the test can
    /// inspect what the bus dispatched.
    #[derive(Debug, Default)]
    struct RecorderDevice {
        reads: Vec<(u64, usize)>,
        writes: Vec<(u64, Vec<u8>)>,
    }

    impl BusDevice for RecorderDevice {
        fn read(&mut self, offset: u64, data: &mut [u8]) {
            self.reads.push((offset, data.len()));
            let base = u8::try_from(offset & 0xFF).unwrap_or(0);
            for (i, b) in data.iter_mut().enumerate() {
                let i_low = u8::try_from(i & 0xFF).unwrap_or(0);
                *b = base.wrapping_add(i_low);
            }
        }
        fn write(&mut self, offset: u64, data: &[u8]) {
            self.writes.push((offset, data.to_vec()));
        }
        #[allow(clippy::unnecessary_literal_bound)]
        fn debug_label(&self) -> &str {
            "recorder"
        }
    }

    fn dev() -> Arc<Mutex<dyn BusDevice>> {
        Arc::new(Mutex::new(RecorderDevice::default()))
    }

    #[test]
    fn test_should_reject_zero_length_insert() {
        let bus = Bus::new();
        let err = bus.insert(dev(), 0x1000, 0).unwrap_err();
        assert!(matches!(err, BusError::ZeroLengthRange));
    }

    #[test]
    fn test_should_reject_range_overflow() {
        let bus = Bus::new();
        let err = bus.insert(dev(), u64::MAX, 2).unwrap_err();
        assert!(matches!(err, BusError::RangeOverflow));
    }

    #[test]
    fn test_should_register_disjoint_devices() {
        let bus = Bus::new();
        bus.insert(dev(), 0x0F00_0000, 0x1000).unwrap();
        bus.insert(dev(), 0x0F00_1000, 0x1000).unwrap();
        bus.insert(dev(), 0x0E0A_0000, 0x1000).unwrap(); // PL011
        assert_eq!(bus.len(), 3);
    }

    #[test]
    fn test_should_reject_overlap_at_boundary() {
        let bus = Bus::new();
        bus.insert(dev(), 0x1000, 0x1000).unwrap();
        let err = bus.insert(dev(), 0x1FFF, 0x10).unwrap_err();
        assert!(matches!(err, BusError::Overlap { .. }));
    }

    #[test]
    fn test_should_reject_exact_same_range() {
        let bus = Bus::new();
        bus.insert(dev(), 0x1000, 0x1000).unwrap();
        let err = bus.insert(dev(), 0x1000, 0x1000).unwrap_err();
        assert!(matches!(err, BusError::Overlap { .. }));
    }

    #[test]
    fn test_should_dispatch_read_to_owner_with_relative_offset() {
        let bus = Bus::new();
        let device = dev();
        bus.insert(Arc::clone(&device), 0x1000, 0x1000).unwrap();
        let mut buf = [0u8; 4];
        bus.read(0x1004, &mut buf).unwrap();
        assert_eq!(buf, [4, 5, 6, 7]);
    }

    #[test]
    fn test_should_dispatch_write_to_owner_with_relative_offset() {
        // Wrap the concrete recorder as `Arc<Mutex<RecorderDevice>>` so we
        // can inspect its captured state after the bus dispatches through
        // `Arc<Mutex<dyn BusDevice>>`.
        let recorder: Arc<Mutex<RecorderDevice>> = Arc::new(Mutex::new(RecorderDevice::default()));
        let bus = Bus::new();
        bus.insert(
            Arc::clone(&recorder) as Arc<Mutex<dyn BusDevice>>,
            0x1000,
            0x1000,
        )
        .unwrap();
        bus.write(0x1004, &[0xAA, 0xBB]).unwrap();
        let captured = recorder.lock().writes.clone();
        // The dispatched offset must be device-relative, not bus-absolute.
        assert_eq!(captured, vec![(4, vec![0xAA, 0xBB])]);
    }

    #[test]
    fn test_should_return_no_device_for_unmapped_address() {
        let bus = Bus::new();
        bus.insert(dev(), 0x1000, 0x100).unwrap();
        let mut buf = [0u8; 4];
        let err = bus.read(0x2000, &mut buf).unwrap_err();
        assert!(matches!(err, BusError::NoDevice { addr } if addr == 0x2000));
    }

    #[test]
    fn test_should_route_to_correct_device_among_many_without_crosstalk() {
        // Replicate the shape of the squib MMIO map: 32 virtio slots followed
        // by PL011. Each lookup must land on the right device — a read of
        // slot 5 must not bump PL011's recorder.
        let bus = Bus::new();
        let pl011 = Arc::new(Mutex::new(RecorderDevice::default()));
        bus.insert(
            Arc::clone(&pl011) as Arc<Mutex<dyn BusDevice>>,
            0x0E0A_0000,
            0x1000,
        )
        .unwrap();
        let slot5 = Arc::new(Mutex::new(RecorderDevice::default()));
        for slot in 0..32u64 {
            let base = 0x0F00_0000 + slot * 0x1000;
            if slot == 5 {
                bus.insert(
                    Arc::clone(&slot5) as Arc<Mutex<dyn BusDevice>>,
                    base,
                    0x1000,
                )
                .unwrap();
            } else {
                bus.insert(dev(), base, 0x1000).unwrap();
            }
        }
        let mut buf = [0u8; 4];
        bus.read(0x0E0A_0010, &mut buf).unwrap();
        bus.read(0x0F00_5008, &mut buf).unwrap();
        assert_eq!(pl011.lock().reads, vec![(0x10, 4)]);
        assert_eq!(slot5.lock().reads, vec![(0x8, 4)]);
    }

    #[test]
    fn test_should_return_no_device_for_address_above_last_range() {
        // Edge case for the `range(..=).next_back()` lookup: an address
        // strictly greater than the last range's end must not match the last
        // range.
        let bus = Bus::new();
        bus.insert(dev(), 0x1000, 0x100).unwrap();
        let mut buf = [0u8; 1];
        let err = bus.read(0x1100, &mut buf).unwrap_err();
        assert!(matches!(err, BusError::NoDevice { .. }));
    }

    #[test]
    fn test_should_return_no_device_for_address_below_first_range() {
        let bus = Bus::new();
        bus.insert(dev(), 0x1000, 0x100).unwrap();
        let mut buf = [0u8; 1];
        let err = bus.read(0x0FFF, &mut buf).unwrap_err();
        assert!(matches!(err, BusError::NoDevice { .. }));
    }
}
