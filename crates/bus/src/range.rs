//! Half-open address ranges keyed into the bus map.
//!
//! Ordering is by `base` only — the `BTreeMap` lookup uses
//! `range(..=BusRange::probe(addr)).next_back()` to find the candidate range
//! whose base is closest to (and ≤) the probed address. The candidate's
//! `contains` check then disambiguates "candidate covers this address" from
//! "this address falls in a hole".

use core::{cmp::Ordering, fmt};

use crate::BusError;

/// A half-open `[base, base + len)` address range.
///
/// Ordering is solely on `base`; equality is solely on `base` so that a "probe"
/// range can be used as a lookup key without having to know the exact `len`.
#[derive(Debug, Clone, Copy)]
pub struct BusRange {
    base: u64,
    /// Length in bytes. Always > 0; `BusRange::new` rejects zero-length.
    len: u64,
}

impl BusRange {
    /// Construct a half-open range.
    ///
    /// # Errors
    /// - [`BusError::ZeroLengthRange`] if `len == 0`.
    /// - [`BusError::RangeOverflow`] if `base + len` overflows.
    pub fn new(base: u64, len: u64) -> Result<Self, BusError> {
        if len == 0 {
            return Err(BusError::ZeroLengthRange);
        }
        base.checked_add(len).ok_or(BusError::RangeOverflow)?;
        Ok(Self { base, len })
    }

    /// Build a length-1 range used as a lookup key against the `BTreeMap`.
    /// Never inserted; never compared for `eq` against a stored range with the
    /// same base because base equality alone is the equivalence.
    pub(crate) fn probe(addr: u64) -> Self {
        Self { base: addr, len: 1 }
    }

    /// Inclusive base address.
    #[inline]
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Length in bytes. Always > 0 by construction (`new` rejects zero), so
    /// no `is_empty` companion is needed.
    #[inline]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Exclusive end address. `base + len` cannot overflow because `new` checks.
    #[inline]
    pub fn end(&self) -> u64 {
        self.base + self.len
    }

    /// `true` iff `addr` falls inside this half-open range.
    #[inline]
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.base && addr < self.end()
    }

    /// `true` iff `self` and `other` share at least one address.
    #[inline]
    pub fn overlaps(&self, other: &Self) -> bool {
        self.base < other.end() && other.base < self.end()
    }
}

impl fmt::Display for BusRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:#x}, {:#x})", self.base, self.end())
    }
}

impl PartialEq for BusRange {
    fn eq(&self, other: &Self) -> bool {
        self.base == other.base
    }
}

impl Eq for BusRange {}

impl Ord for BusRange {
    fn cmp(&self, other: &Self) -> Ordering {
        self.base.cmp(&other.base)
    }
}

impl PartialOrd for BusRange {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_build_valid_range() {
        let r = BusRange::new(0x1000, 0x100).unwrap();
        assert_eq!(r.base(), 0x1000);
        assert_eq!(r.end(), 0x1100);
        assert_eq!(r.len(), 0x100);
    }

    #[test]
    fn test_should_reject_zero_length() {
        assert!(matches!(
            BusRange::new(0x1000, 0),
            Err(BusError::ZeroLengthRange)
        ));
    }

    #[test]
    fn test_should_reject_overflow() {
        assert!(matches!(
            BusRange::new(u64::MAX, 2),
            Err(BusError::RangeOverflow)
        ));
    }

    #[test]
    fn test_should_contain_first_and_reject_end() {
        let r = BusRange::new(0x1000, 0x100).unwrap();
        assert!(r.contains(0x1000));
        assert!(r.contains(0x10FF));
        assert!(!r.contains(0x1100));
    }

    #[test]
    fn test_should_detect_overlap_at_boundary() {
        let a = BusRange::new(0x1000, 0x100).unwrap();
        let b = BusRange::new(0x10FF, 0x10).unwrap();
        assert!(a.overlaps(&b));
        assert!(b.overlaps(&a));
    }

    #[test]
    fn test_should_not_overlap_adjacent_ranges() {
        let a = BusRange::new(0x1000, 0x100).unwrap();
        let b = BusRange::new(0x1100, 0x100).unwrap();
        assert!(!a.overlaps(&b));
        assert!(!b.overlaps(&a));
    }

    #[test]
    fn test_should_order_ranges_by_base() {
        let a = BusRange::new(0x1000, 0x100).unwrap();
        let b = BusRange::new(0x2000, 0x100).unwrap();
        assert!(a < b);
    }

    #[test]
    fn test_should_treat_same_base_as_equal_for_btreemap_keying() {
        let a = BusRange::new(0x1000, 0x100).unwrap();
        let b = BusRange::new(0x1000, 0x200).unwrap();
        assert_eq!(a, b);
    }
}
