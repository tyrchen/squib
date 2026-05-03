//! Guest-physical address ranges and memory protection types.

use core::fmt;

use serde::{Deserialize, Serialize};

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
}
