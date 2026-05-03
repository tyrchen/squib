//! Per-vCPU IRQ shadow bitset.
//!
//! Devices inject interrupts from the VMM event loop or worker threads; the vCPU thread
//! drains the shadow on its next `pre_run_housekeeping` and asserts each pending INTID
//! against HVF. The bitset is a `Box<[AtomicU64]>` per vCPU — lock-free, cache-friendly,
//! and the size lets us cover the entire SPI range (1019 INTIDs) in 16 words.
//!
//! See [12-hvf-backend.md § 8](../../../specs/12-hvf-backend.md#8-behaviour-edges) and
//! [71-performance-budgets.md §
//! 4](../../../specs/71-performance-budgets.md#4-vcpu-exit-dispatch-p3).

use std::sync::atomic::{AtomicU64, Ordering};

/// Highest INTID we track per vCPU. GICv3 SPI range tops out at 1019; we round up to
/// 1024 to keep the bitset a neat 16 × `u64` words.
pub const MAX_TRACKED_INTID: u32 = 1024;

const WORD_BITS: u32 = 64;
const WORDS: usize = (MAX_TRACKED_INTID / WORD_BITS) as usize;

/// Lock-free per-vCPU IRQ shadow.
///
/// `inject` flips a bit with `fetch_or(Relaxed)` from any thread; the vCPU thread drains
/// each word with `swap(0, Acquire)` to read-and-clear. The Acquire on drain pairs with
/// any prior Release on the device side that signalled the queue cursor.
#[derive(Debug)]
pub struct IrqShadow {
    words: Box<[AtomicU64]>,
}

impl IrqShadow {
    /// Allocate a fresh per-vCPU shadow with all bits cleared.
    #[must_use]
    pub fn new() -> Self {
        let mut v = Vec::with_capacity(WORDS);
        for _ in 0..WORDS {
            v.push(AtomicU64::new(0));
        }
        Self {
            words: v.into_boxed_slice(),
        }
    }

    /// Mark `intid` pending. Safe to call from any thread.
    ///
    /// Returns `true` if the call set the bit (i.e. it was not already pending). Returns
    /// `false` if the bit was already set or `intid >= MAX_TRACKED_INTID` (out-of-range
    /// injects are a programming error and silently dropped — the caller validated the
    /// INTID against the GIC's SPI range before reaching this layer).
    pub fn inject(&self, intid: u32) -> bool {
        let Some((word, mask)) = bit_position(intid) else {
            tracing::error!(intid, "IRQ shadow: intid out of range, dropping inject");
            return false;
        };
        let prev = self.words[word].fetch_or(mask, Ordering::Relaxed);
        prev & mask == 0
    }

    /// Drain pending INTIDs into a callback. The callback is invoked once per pending
    /// INTID with the raw INTID value; the implementation arranges for each word to be
    /// read-and-cleared exactly once.
    pub fn drain<F: FnMut(u32)>(&self, mut emit: F) {
        for (idx, word) in self.words.iter().enumerate() {
            let mut bits = word.swap(0, Ordering::Acquire);
            // `idx < WORDS = MAX_TRACKED_INTID / WORD_BITS`, so the multiplication fits.
            let base = u32::try_from(idx).expect("WORDS bounded") * WORD_BITS;
            while bits != 0 {
                let bit = bits.trailing_zeros();
                emit(base + bit);
                bits &= !(1u64 << bit);
            }
        }
    }

    /// Returns `true` if no INTIDs are currently pending.
    ///
    /// Best-effort observation, not a synchronization point — a parallel `inject` may
    /// flip a bit between the load and the caller acting on the answer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|w| w.load(Ordering::Relaxed) == 0)
    }
}

impl Default for IrqShadow {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn bit_position(intid: u32) -> Option<(usize, u64)> {
    if intid >= MAX_TRACKED_INTID {
        return None;
    }
    let word = (intid / WORD_BITS) as usize;
    let mask = 1u64 << (intid % WORD_BITS);
    Some((word, mask))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_shadow_is_empty() {
        let shadow = IrqShadow::new();
        assert!(shadow.is_empty());
    }

    #[test]
    fn inject_then_drain_emits_each_intid_exactly_once() {
        let shadow = IrqShadow::new();
        for intid in [33, 48, 49, 100, 1000, 1023] {
            assert!(shadow.inject(intid));
        }
        let mut seen = Vec::new();
        shadow.drain(|id| seen.push(id));
        seen.sort_unstable();
        assert_eq!(seen, vec![33, 48, 49, 100, 1000, 1023]);
        // Drained → empty.
        assert!(shadow.is_empty());
    }

    #[test]
    fn duplicate_inject_is_idempotent() {
        let shadow = IrqShadow::new();
        assert!(shadow.inject(33));
        assert!(
            !shadow.inject(33),
            "second inject must observe already-set bit"
        );
        let mut count = 0;
        shadow.drain(|_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn out_of_range_inject_is_silently_dropped() {
        let shadow = IrqShadow::new();
        assert!(!shadow.inject(MAX_TRACKED_INTID));
        assert!(!shadow.inject(u32::MAX));
        assert!(shadow.is_empty());
    }

    #[test]
    fn cross_thread_injection_is_observed_after_drain() {
        use std::{sync::Arc, thread};

        let shadow = Arc::new(IrqShadow::new());
        let mut handles = Vec::new();
        for thread_id in 0..4u32 {
            let shadow = Arc::clone(&shadow);
            handles.push(thread::spawn(move || {
                for intid in 0..16u32 {
                    shadow.inject(thread_id * 64 + 64 + intid);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let mut count = 0;
        shadow.drain(|_| count += 1);
        assert_eq!(count, 4 * 16);
        assert!(shadow.is_empty());
    }

    #[test]
    fn bit_position_round_trips_for_full_range() {
        for intid in 0..MAX_TRACKED_INTID {
            let (word, mask) = bit_position(intid).unwrap();
            assert!(word < WORDS);
            // Mask must have exactly one bit set.
            assert_eq!(mask.count_ones(), 1);
        }
    }
}
