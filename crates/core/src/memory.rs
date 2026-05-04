//! Guest-physical address ranges and memory protection types.

use core::fmt;

use serde::{Deserialize, Serialize};

// `parking_lot` is the project's standard non-poisoning lock per CLAUDE.md
// `§ Async & Concurrency`. Used for the test-only `SliceGuestMemory`.
use crate::error::{Error, Result};

/// A guest-physical address.
///
/// Newtype around `u64` so the type system stops us from confusing host-virtual and
/// guest-physical addresses at API boundaries.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct GuestAddress(pub u64);

impl GuestAddress {
    /// Returns the underlying `u64` value.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Returns the address aligned down to the given power-of-two `align`.
    ///
    /// # Errors
    /// Returns [`Error::InvalidArgument`] if `align` is not a power of two.
    pub fn align_down(self, align: u64) -> Result<Self> {
        if !align.is_power_of_two() {
            return Err(Error::InvalidArgument(format!(
                "alignment must be a power of two: {align}"
            )));
        }
        Ok(Self(self.0 & !(align - 1)))
    }

    /// Returns the address advanced by `offset`, saturating at `u64::MAX`.
    #[inline]
    #[must_use]
    pub const fn saturating_add(self, offset: u64) -> Self {
        Self(self.0.saturating_add(offset))
    }
}

impl fmt::Display for GuestAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

impl From<u64> for GuestAddress {
    #[inline]
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// A half-open guest-physical range `[base, base + size)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuestRange {
    /// The starting guest-physical address (inclusive).
    pub base: GuestAddress,
    /// The size of the range in bytes.
    pub size: u64,
}

impl GuestRange {
    /// Construct a new [`GuestRange`].
    ///
    /// # Errors
    /// Returns [`Error::InvalidArgument`] if `base + size` would overflow.
    pub fn new(base: GuestAddress, size: u64) -> Result<Self> {
        base.0.checked_add(size).ok_or_else(|| {
            Error::InvalidArgument(format!("range overflow: base={base} size={size}"))
        })?;
        Ok(Self { base, size })
    }

    /// Returns the first address past the range (exclusive end).
    #[inline]
    pub fn end(self) -> GuestAddress {
        GuestAddress(self.base.0 + self.size)
    }

    /// Returns true if `addr` falls inside the range.
    #[inline]
    pub fn contains(self, addr: GuestAddress) -> bool {
        addr.0 >= self.base.0 && addr.0 < self.base.0 + self.size
    }
}

/// Memory protection bits a backend may set on a guest range.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // matches POSIX RWX semantics; bitflags would be overkill
pub struct Protection {
    /// Guest may read.
    pub read: bool,
    /// Guest may write.
    pub write: bool,
    /// Guest may execute.
    pub execute: bool,
}

impl Protection {
    /// Read-only.
    pub const READ: Self = Self::new(true, false, false);
    /// Read-write.
    pub const READ_WRITE: Self = Self::new(true, true, false);
    /// Read-execute (e.g. kernel text).
    pub const READ_EXECUTE: Self = Self::new(true, false, true);
    /// Read-write-execute.
    pub const READ_WRITE_EXECUTE: Self = Self::new(true, true, true);

    /// Construct a protection set from individual flags.
    pub const fn new(read: bool, write: bool, execute: bool) -> Self {
        Self {
            read,
            write,
            execute,
        }
    }
}

/// A region of guest memory backed by a host-side mmap, registered with the hypervisor.
#[derive(Debug)]
#[non_exhaustive]
pub struct GuestMemoryRegion {
    /// The guest-physical range this region covers.
    pub range: GuestRange,
    /// The slot identifier the backend assigns; meaningful only to the backend.
    pub slot: u32,
    /// The default protection for the region at registration time.
    pub protection: Protection,
}

impl GuestMemoryRegion {
    /// Construct a new region.
    pub fn new(range: GuestRange, slot: u32, protection: Protection) -> Self {
        Self {
            range,
            slot,
            protection,
        }
    }
}

/// Read / write access to guest memory across one or more registered regions.
///
/// Implementations are responsible for routing addresses to the underlying
/// host-mapped buffer; callers (virtio devices, snapshot writer, debugger)
/// only see guest-physical addresses.
///
/// All methods MUST be byte-accurate: a partial read or write is a hard error,
/// not a silent truncation. This matches the virtio spec's expectation that
/// descriptor chains either fully resolve or surface as a malformed-payload
/// error to the device handler.
pub trait GuestMemory: Send + Sync + fmt::Debug {
    /// Read `buf.len()` bytes from `addr` into `buf`.
    ///
    /// # Errors
    /// [`Error::MemoryOutOfRange`] if the request escapes any registered
    /// region, including a request that straddles a region boundary.
    fn read(&self, addr: GuestAddress, buf: &mut [u8]) -> Result<()>;

    /// Write `buf.len()` bytes from `buf` into guest memory at `addr`.
    ///
    /// # Errors
    /// Same as [`Self::read`].
    fn write(&self, addr: GuestAddress, buf: &[u8]) -> Result<()>;

    /// Read a little-endian `u16` from `addr`.
    ///
    /// # Errors
    /// Same as [`Self::read`].
    fn read_u16_le(&self, addr: GuestAddress) -> Result<u16> {
        let mut b = [0u8; 2];
        self.read(addr, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    /// Read a little-endian `u32` from `addr`.
    ///
    /// # Errors
    /// Same as [`Self::read`].
    fn read_u32_le(&self, addr: GuestAddress) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    /// Read a little-endian `u64` from `addr`.
    ///
    /// # Errors
    /// Same as [`Self::read`].
    fn read_u64_le(&self, addr: GuestAddress) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    /// Write a little-endian `u16` to `addr`.
    ///
    /// # Errors
    /// Same as [`Self::write`].
    fn write_u16_le(&self, addr: GuestAddress, value: u16) -> Result<()> {
        self.write(addr, &value.to_le_bytes())
    }

    /// Write a little-endian `u32` to `addr`.
    ///
    /// # Errors
    /// Same as [`Self::write`].
    fn write_u32_le(&self, addr: GuestAddress, value: u32) -> Result<()> {
        self.write(addr, &value.to_le_bytes())
    }

    /// Write a little-endian `u64` to `addr`.
    ///
    /// # Errors
    /// Same as [`Self::write`].
    fn write_u64_le(&self, addr: GuestAddress, value: u64) -> Result<()> {
        self.write(addr, &value.to_le_bytes())
    }
}

/// In-process [`GuestMemory`] implementation backed by a single contiguous
/// `Vec<u8>` keyed at a fixed `base` guest address.
///
/// Useful for unit tests that need a real `GuestMemory` without standing up
/// the HVF backend. Production hypervisor backends provide their own
/// implementation that routes to `mmap`-backed regions.
#[derive(Debug)]
pub struct SliceGuestMemory {
    base: GuestAddress,
    bytes: parking_lot::RwLock<Vec<u8>>,
}

impl SliceGuestMemory {
    /// Build a `[base, base + size)` region zero-filled at construction.
    #[must_use]
    pub fn new(base: GuestAddress, size: usize) -> Self {
        Self {
            base,
            bytes: parking_lot::RwLock::new(vec![0u8; size]),
        }
    }

    /// Build a region pre-populated with `bytes` starting at `base`.
    #[must_use]
    pub fn from_bytes(base: GuestAddress, bytes: Vec<u8>) -> Self {
        Self {
            base,
            bytes: parking_lot::RwLock::new(bytes),
        }
    }

    /// Inclusive base address.
    #[must_use]
    pub fn base(&self) -> GuestAddress {
        self.base
    }

    /// Region size in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        self.bytes.read().len()
    }

    fn offset_of(&self, addr: GuestAddress, len: usize) -> Result<usize> {
        let base = self.base.raw();
        let size = u64::try_from(self.bytes.read().len()).unwrap_or(u64::MAX);
        let end = base.saturating_add(size);
        let len_u64 = u64::try_from(len)
            .map_err(|_| Error::MemoryOutOfRange(format!("addr {addr} + len {len} overflows")))?;
        let req_end = addr
            .raw()
            .checked_add(len_u64)
            .ok_or_else(|| Error::MemoryOutOfRange(format!("addr {addr} + len {len} overflows")))?;
        if addr.raw() < base || req_end > end {
            return Err(Error::MemoryOutOfRange(format!(
                "addr {addr} + len {len} escapes [{base:#x}, {end:#x})"
            )));
        }
        usize::try_from(addr.raw() - base).map_err(|_| {
            Error::MemoryOutOfRange(format!("offset {} exceeds usize::MAX", addr.raw() - base))
        })
    }
}

impl GuestMemory for SliceGuestMemory {
    fn read(&self, addr: GuestAddress, buf: &mut [u8]) -> Result<()> {
        let off = self.offset_of(addr, buf.len())?;
        let bytes = self.bytes.read();
        buf.copy_from_slice(&bytes[off..off + buf.len()]);
        Ok(())
    }

    fn write(&self, addr: GuestAddress, buf: &[u8]) -> Result<()> {
        let off = self.offset_of(addr, buf.len())?;
        let mut bytes = self.bytes.write();
        bytes[off..off + buf.len()].copy_from_slice(buf);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_down_rejects_non_power_of_two() {
        let addr = GuestAddress(0x1234);
        assert!(addr.align_down(7).is_err());
    }

    #[test]
    fn align_down_rounds() {
        let addr = GuestAddress(0x1234);
        assert_eq!(addr.align_down(0x1000).unwrap().raw(), 0x1000);
    }

    #[test]
    fn range_overflow_is_rejected() {
        let err = GuestRange::new(GuestAddress(u64::MAX - 0x100), 0x200).unwrap_err();
        matches!(err, Error::InvalidArgument(_));
    }

    #[test]
    fn range_contains_endpoints() {
        let r = GuestRange::new(GuestAddress(0x1000), 0x1000).unwrap();
        assert!(r.contains(GuestAddress(0x1000)));
        assert!(r.contains(GuestAddress(0x1FFF)));
        assert!(!r.contains(GuestAddress(0x2000)));
    }

    #[test]
    fn protection_constants_round_trip() {
        let p = Protection::READ_WRITE;
        assert!(p.read && p.write && !p.execute);
    }

    #[test]
    fn slice_guest_memory_round_trips_typed_writes() {
        let mem = SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x1000);
        mem.write_u32_le(GuestAddress(0x4000_0010), 0xDEAD_BEEF)
            .unwrap();
        assert_eq!(
            mem.read_u32_le(GuestAddress(0x4000_0010)).unwrap(),
            0xDEAD_BEEF
        );
        mem.write_u16_le(GuestAddress(0x4000_0020), 0xABCD).unwrap();
        assert_eq!(mem.read_u16_le(GuestAddress(0x4000_0020)).unwrap(), 0xABCD);
    }

    #[test]
    fn slice_guest_memory_rejects_below_base() {
        let mem = SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x1000);
        let mut buf = [0u8; 4];
        let err = mem.read(GuestAddress(0x3FFF_FFFF), &mut buf).unwrap_err();
        assert!(matches!(err, Error::MemoryOutOfRange(_)));
    }

    #[test]
    fn slice_guest_memory_rejects_straddling_top() {
        let mem = SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x10);
        let mut buf = [0u8; 8];
        let err = mem.read(GuestAddress(0x4000_000C), &mut buf).unwrap_err();
        assert!(matches!(err, Error::MemoryOutOfRange(_)));
    }

    #[test]
    fn slice_guest_memory_rejects_overflow() {
        let mem = SliceGuestMemory::new(GuestAddress(u64::MAX - 0x10), 0x10);
        let mut buf = [0u8; 0x100];
        let err = mem
            .read(GuestAddress(u64::MAX - 0x8), &mut buf)
            .unwrap_err();
        assert!(matches!(err, Error::MemoryOutOfRange(_)));
    }
}
