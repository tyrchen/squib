//! Dirty page tracking — `Box<[AtomicU64]>` shadow bitmap, adaptive heuristic.
//!
//! Per [16-snapshots.md § 4](../../../specs/16-snapshots.md#4-dirty-page-tracking):
//! after a clean checkpoint, the VMM strips `HV_MEMORY_WRITE` from the entire tracked
//! range; guest writes generate ESR EC=0x24, WnR=1 exits; the vCPU exit handler sets
//! the corresponding bit in this bitmap and re-grants WRITE on the page (or 2 MiB
//! block).
//!
//! Three sizes interact (D21, codified in [`squib_arch::PageGeometry`]):
//! - `HOST_PAGE_SIZE` = 16 KiB on Apple Silicon.
//! - `HVF_STAGE2_GRANULE` = 16 KiB (matches the host page).
//! - `TRACKING_PAGE_SIZE` = 2 MiB default; adaptive step-down to 16 KiB for hot regions per the
//!   heuristic in 16 § 4.2.
//!
//! The shadow bitmap is per-RAM-region, sized for the **default** (2 MiB) tracking
//! granularity. When the heuristic steps down, we promote the same region to a
//! finer-grained bitmap; the coarse bitmap's set bits propagate as "all pages in
//! that 2 MiB block are dirty" so the diff snapshot doesn't drop coverage during the
//! transition.

use std::{
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use squib_arch::PageGeometry;
use tracing::{debug, info};

use crate::error::SnapshotError;

/// Default fault-rate threshold for adaptive step-down: 32 faults per 100 ms per
/// 2 MiB tracking block. Matches 16 § 4.2.
pub const DEFAULT_STEP_DOWN_THRESHOLD: u32 = 32;

/// Sliding-window length for the step-down heuristic.
pub const ADAPTIVE_WINDOW: Duration = Duration::from_millis(100);

/// Tracking granularity for one RAM region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackingGranule {
    /// 2 MiB tracking pages — default cheap mode.
    Coarse,
    /// 16 KiB (host-page) tracking pages — adaptive hot mode.
    Fine,
}

impl TrackingGranule {
    /// Page size in bytes for this granule.
    #[must_use]
    pub const fn page_size(self, geom: PageGeometry) -> u64 {
        match self {
            Self::Coarse => geom.tracking_page_default,
            Self::Fine => geom.tracking_page_hot,
        }
    }
}

/// `Box<[AtomicU64]>` shadow bitmap of dirty tracking pages.
///
/// The map covers `[ram_start, ram_start + ram_size)`. `set_dirty` is called from
/// the vCPU exit handler; `drain` is called by the snapshot writer. Both are
/// lock-free.
#[derive(Debug)]
pub struct DirtyBitmap {
    /// Inclusive base of the tracked range.
    ram_start: u64,
    /// Total size of the tracked range in bytes.
    ram_size: u64,
    /// Page size in bytes (16 KiB or 2 MiB).
    page_size: u64,
    /// Number of bits in the bitmap (= number of pages).
    page_count: u64,
    /// Storage: each `AtomicU64` carries 64 bits.
    words: Box<[AtomicU64]>,
}

impl DirtyBitmap {
    /// Build a bitmap covering `[ram_start, ram_start + ram_size)` at `page_size`.
    ///
    /// # Errors
    /// [`SnapshotError::InvalidPath`] (re-used for "rejected configuration") if
    /// `page_size` is not a power of two, the range is empty, or the range overflows.
    pub fn new(ram_start: u64, ram_size: u64, page_size: u64) -> Result<Self, SnapshotError> {
        if !page_size.is_power_of_two() {
            return Err(SnapshotError::InvalidPath(format!(
                "page_size must be a power of two: {page_size}"
            )));
        }
        if ram_size == 0 {
            return Err(SnapshotError::InvalidPath(
                "dirty bitmap requires a non-empty RAM range".into(),
            ));
        }
        if ram_start.checked_add(ram_size).is_none() {
            return Err(SnapshotError::InvalidPath(format!(
                "RAM range overflows: start={ram_start:#x}, size={ram_size:#x}"
            )));
        }
        // Round up the page count, so a partial trailing page is still tracked.
        let page_count = ram_size.div_ceil(page_size);
        let word_count = usize::try_from(page_count.div_ceil(64)).map_err(|_| {
            SnapshotError::InvalidPath(format!(
                "bitmap word count exceeds usize::MAX (page_count={page_count})"
            ))
        })?;
        let words: Vec<AtomicU64> = (0..word_count).map(|_| AtomicU64::new(0)).collect();
        Ok(Self {
            ram_start,
            ram_size,
            page_size,
            page_count,
            words: words.into_boxed_slice(),
        })
    }

    /// Total tracked range start.
    #[must_use]
    pub const fn ram_start(&self) -> u64 {
        self.ram_start
    }

    /// Total tracked range size in bytes.
    #[must_use]
    pub const fn ram_size(&self) -> u64 {
        self.ram_size
    }

    /// Page size (in bytes).
    #[must_use]
    pub const fn page_size(&self) -> u64 {
        self.page_size
    }

    /// Number of tracked pages.
    #[must_use]
    pub const fn page_count(&self) -> u64 {
        self.page_count
    }

    /// `log2(page_size)` — used by callers that compute the FAR-to-bit-index shift.
    #[must_use]
    pub fn page_shift(&self) -> u32 {
        self.page_size.trailing_zeros()
    }

    /// Compute the bit index of the page containing `addr`.
    ///
    /// Returns `None` if `addr` is outside the tracked range.
    #[must_use]
    pub fn page_index_of(&self, addr: u64) -> Option<u64> {
        if addr < self.ram_start {
            return None;
        }
        let offset = addr - self.ram_start;
        if offset >= self.ram_size {
            return None;
        }
        Some(offset >> self.page_shift())
    }

    /// Set the dirty bit for the page containing `addr`. Idempotent.
    ///
    /// Returns `true` if the bit transitioned from clean to dirty.
    pub fn set_dirty(&self, addr: u64) -> bool {
        let Some(page) = self.page_index_of(addr) else {
            return false;
        };
        self.set_dirty_by_index(page)
    }

    /// Set a specific page's dirty bit. Same return semantics as [`Self::set_dirty`].
    pub fn set_dirty_by_index(&self, page: u64) -> bool {
        if page >= self.page_count {
            return false;
        }
        let word_idx = (page / 64) as usize;
        let bit = page % 64;
        let mask = 1u64 << bit;
        let prev = self.words[word_idx].fetch_or(mask, Ordering::Relaxed);
        prev & mask == 0
    }

    /// Read whether `addr` is dirty (without clearing).
    #[must_use]
    pub fn is_dirty(&self, addr: u64) -> bool {
        let Some(page) = self.page_index_of(addr) else {
            return false;
        };
        self.is_dirty_by_index(page)
    }

    /// Same as [`Self::is_dirty`] but accepts a pre-computed page index.
    #[must_use]
    pub fn is_dirty_by_index(&self, page: u64) -> bool {
        if page >= self.page_count {
            return false;
        }
        let word_idx = (page / 64) as usize;
        let bit = page % 64;
        let word = self.words[word_idx].load(Ordering::Acquire);
        word & (1u64 << bit) != 0
    }

    /// Atomically swap each word to zero, returning a list of dirty page indices.
    ///
    /// The acquire ordering on the swap pairs with the release on `set_dirty`'s
    /// `fetch_or` (Relaxed is sufficient there because the only consumer is this
    /// drain on the snapshot path, and the drain seq-cst-like ordering is owed by
    /// the surrounding pause/resume vCPU dance, not by the bitmap itself; we use
    /// Acquire here to synchronize-with any future code that relaxes the surrounding
    /// vCPU coordination).
    pub fn drain(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for (word_idx, word) in self.words.iter().enumerate() {
            let mut bits = word.swap(0, Ordering::Acquire);
            while bits != 0 {
                let bit = u64::from(bits.trailing_zeros());
                bits &= bits - 1;
                let page = (word_idx as u64) * 64 + bit;
                if page < self.page_count {
                    out.push(page);
                }
            }
        }
        out
    }

    /// Drain into a callback, avoiding the intermediate Vec allocation. Used by the
    /// memory-file writer's hot path.
    pub fn drain_into<F: FnMut(u64)>(&self, mut callback: F) {
        for (word_idx, word) in self.words.iter().enumerate() {
            let mut bits = word.swap(0, Ordering::Acquire);
            while bits != 0 {
                let bit = u64::from(bits.trailing_zeros());
                bits &= bits - 1;
                let page = (word_idx as u64) * 64 + bit;
                if page < self.page_count {
                    callback(page);
                }
            }
        }
    }

    /// Fold all coarse-block dirty bits into a finer-grained bitmap.
    ///
    /// When the heuristic steps down a region from 2 MiB to 16 KiB, the coarse
    /// bitmap's set bits each correspond to a 2 MiB block: every fine page in that
    /// block becomes dirty. This preserves snapshot coverage across the transition.
    ///
    /// `fine` must cover the same `[ram_start, ram_start + ram_size)` and have a
    /// strictly smaller `page_size`.
    ///
    /// # Errors
    /// [`SnapshotError::InvalidPath`] if the geometry doesn't match.
    pub fn fold_into_finer(&self, fine: &DirtyBitmap) -> Result<(), SnapshotError> {
        if fine.ram_start != self.ram_start || fine.ram_size != self.ram_size {
            return Err(SnapshotError::InvalidPath(
                "fold_into_finer requires identical ranges".into(),
            ));
        }
        if fine.page_size >= self.page_size {
            return Err(SnapshotError::InvalidPath(
                "fold_into_finer expects a strictly smaller page size".into(),
            ));
        }
        if !self.page_size.is_multiple_of(fine.page_size) {
            return Err(SnapshotError::InvalidPath(
                "coarse page must be a multiple of fine page".into(),
            ));
        }
        let ratio = self.page_size / fine.page_size;
        for (word_idx, word) in self.words.iter().enumerate() {
            let bits = word.load(Ordering::Acquire);
            if bits == 0 {
                continue;
            }
            let mut bits = bits;
            while bits != 0 {
                let bit = u64::from(bits.trailing_zeros());
                bits &= bits - 1;
                let coarse_page = (word_idx as u64) * 64 + bit;
                if coarse_page >= self.page_count {
                    break;
                }
                let fine_start = coarse_page * ratio;
                let fine_end = (fine_start + ratio).min(fine.page_count);
                for fine_page in fine_start..fine_end {
                    fine.set_dirty_by_index(fine_page);
                }
            }
        }
        Ok(())
    }
}

/// Per-region adaptive controller: tracks **per-2-MiB-block** fault rate and
/// signals when a region should step down from 2 MiB to 16 KiB tracking.
///
/// Per [16-snapshots.md § 4.2](../../../specs/16-snapshots.md#42-bitmap-sizing-and-adaptive-heuristic)
/// the threshold is "32 faults / 100 ms / 2 MiB block" — per-block, not aggregate
/// across the region. A uniform-low-rate workload that touches many blocks must
/// not trigger step-down; only a workload concentrated in *some* block must.
#[derive(Debug)]
pub struct AdaptiveController {
    threshold: u32,
    window: Duration,
    /// One counter per coarse (2 MiB) block in the region. The hot-block detector
    /// walks the slice on each tick.
    block_faults: Box<[AtomicU32]>,
    /// Sequence-of-window guard. The window itself is reset by the supervising
    /// thread that polls [`Self::tick`].
    last_reset: parking_lot::Mutex<Instant>,
    granule: parking_lot::RwLock<TrackingGranule>,
}

impl AdaptiveController {
    /// Build a controller with the spec-default threshold (32 faults / 100 ms /
    /// 2 MiB block) for `block_count` 2-MiB blocks.
    #[must_use]
    pub fn new(block_count: u64) -> Self {
        Self::with_threshold(block_count, DEFAULT_STEP_DOWN_THRESHOLD)
    }

    /// Build with a custom threshold (used by `[squib].snapshot.dirty_step_down_threshold`
    /// in user config — § 4.2).
    #[must_use]
    pub fn with_threshold(block_count: u64, threshold: u32) -> Self {
        let len = usize::try_from(block_count).unwrap_or(usize::MAX);
        let blocks: Vec<AtomicU32> = (0..len).map(|_| AtomicU32::new(0)).collect();
        Self {
            threshold,
            window: ADAPTIVE_WINDOW,
            block_faults: blocks.into_boxed_slice(),
            last_reset: parking_lot::Mutex::new(Instant::now()),
            granule: parking_lot::RwLock::new(TrackingGranule::Coarse),
        }
    }

    /// Active granule.
    pub fn granule(&self) -> TrackingGranule {
        *self.granule.read()
    }

    /// Increment the fault counter for the given coarse block. Called by the
    /// dirty-tracking exit handler on every page fault re-grant.
    ///
    /// Out-of-range `block_idx` is silently ignored (defence-in-depth — the bitmap
    /// rejects out-of-range pages too).
    pub fn record_fault(&self, block_idx: u64) {
        let Ok(idx) = usize::try_from(block_idx) else {
            return;
        };
        let Some(slot) = self.block_faults.get(idx) else {
            return;
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }

    /// Check and (if we've crossed a window boundary) decide whether the granule
    /// should step down. Returns the (possibly-changed) active granule.
    ///
    /// Step-down fires when **any** block's fault counter exceeded the threshold
    /// in the last window. Step-down is one-way for the lifetime of one tracking
    /// session; ramping back up to 2 MiB requires a clean checkpoint.
    pub fn tick(&self) -> TrackingGranule {
        let mut last = self.last_reset.lock();
        let now = Instant::now();
        if now.duration_since(*last) < self.window {
            return self.granule();
        }
        // Atomically swap each block's counter to 0 and find the max in this
        // window. The swap pairs with `fetch_add`'s Relaxed ordering — Acquire
        // here is overkill but cheap and futureproof.
        let mut max_in_window: u32 = 0;
        for slot in &self.block_faults {
            let count = slot.swap(0, Ordering::Acquire);
            if count > max_in_window {
                max_in_window = count;
            }
        }
        *last = now;
        drop(last);

        // Promote to fine if any block was hot, never demote within a session.
        let mut current = self.granule.write();
        if max_in_window > self.threshold && *current == TrackingGranule::Coarse {
            info!(
                hottest_block_faults = max_in_window,
                threshold = self.threshold,
                "dirty tracker stepping down to fine granule (16 KiB)"
            );
            *current = TrackingGranule::Fine;
        } else {
            debug!(
                hottest_block_faults = max_in_window,
                granule = ?*current,
                "dirty tracker tick"
            );
        }
        *current
    }
}

impl Default for AdaptiveController {
    /// One-block default — useful only as a placeholder for tests that exercise
    /// the granule API without driving real fault traffic. Production code calls
    /// [`Self::new`] with the per-region block count.
    fn default() -> Self {
        Self::new(1)
    }
}

/// One RAM region under dirty-tracking. Combines a bitmap and an adaptive
/// controller; the bitmap is rebuilt at finer granularity if the controller signals
/// step-down.
#[derive(Debug)]
pub struct TrackedRegion {
    /// Active bitmap.
    bitmap: parking_lot::RwLock<std::sync::Arc<DirtyBitmap>>,
    /// Adaptive controller.
    controller: AdaptiveController,
    /// Page geometry (carries 16 KiB / 2 MiB constants).
    geometry: PageGeometry,
}

impl TrackedRegion {
    /// Build a region under coarse (2 MiB) tracking by default.
    ///
    /// # Errors
    /// As [`DirtyBitmap::new`].
    pub fn new(
        ram_start: u64,
        ram_size: u64,
        geometry: PageGeometry,
    ) -> Result<Self, SnapshotError> {
        let bitmap = DirtyBitmap::new(ram_start, ram_size, geometry.tracking_page_default)?;
        let block_count = bitmap.page_count();
        Ok(Self {
            bitmap: parking_lot::RwLock::new(std::sync::Arc::new(bitmap)),
            controller: AdaptiveController::new(block_count),
            geometry,
        })
    }

    /// Construct with a custom step-down threshold.
    ///
    /// # Errors
    /// As [`DirtyBitmap::new`].
    pub fn with_threshold(
        ram_start: u64,
        ram_size: u64,
        geometry: PageGeometry,
        threshold: u32,
    ) -> Result<Self, SnapshotError> {
        let bitmap = DirtyBitmap::new(ram_start, ram_size, geometry.tracking_page_default)?;
        let block_count = bitmap.page_count();
        Ok(Self {
            bitmap: parking_lot::RwLock::new(std::sync::Arc::new(bitmap)),
            controller: AdaptiveController::with_threshold(block_count, threshold),
            geometry,
        })
    }

    /// Borrow the active bitmap (cheap `Arc` clone).
    pub fn bitmap(&self) -> std::sync::Arc<DirtyBitmap> {
        self.bitmap.read().clone()
    }

    /// Active granule.
    pub fn granule(&self) -> TrackingGranule {
        self.controller.granule()
    }

    /// Mark `addr` dirty (called from the vCPU exit handler) and record a fault
    /// for the adaptive controller.
    ///
    /// The fault is attributed to the **coarse 2-MiB block** containing `addr`,
    /// even when the active bitmap has stepped down to fine granularity — that
    /// way the per-block counter has the same semantics as § 4.2 specifies it
    /// across the granule transition.
    ///
    /// Returns `true` if the bit transitioned from clean to dirty.
    pub fn mark_dirty(&self, addr: u64) -> bool {
        let coarse_block_idx = addr
            .checked_sub(self.bitmap.read().ram_start())
            .map(|off| off / self.geometry.tracking_page_default);
        if let Some(block_idx) = coarse_block_idx {
            self.controller.record_fault(block_idx);
        }
        let bitmap = self.bitmap.read().clone();
        bitmap.set_dirty(addr)
    }

    /// Tick the adaptive controller; promote to a finer-grained bitmap if needed.
    ///
    /// Called periodically (e.g. once per WFI loop or once per second by a
    /// dedicated tick thread). Idempotent.
    pub fn tick_adaptive(&self) -> TrackingGranule {
        let new_granule = self.controller.tick();
        let mut current = self.bitmap.write();
        let active_size = current.page_size();
        let target_size = new_granule.page_size(self.geometry);
        if target_size != active_size {
            let Ok(fine) = DirtyBitmap::new(current.ram_start(), current.ram_size(), target_size)
            else {
                return self.controller.granule();
            };
            // Best-effort fold; if it errors (it shouldn't for valid geometry) we
            // keep the existing bitmap.
            if current.fold_into_finer(&fine).is_ok() {
                *current = std::sync::Arc::new(fine);
            }
        }
        new_granule
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGION_BASE: u64 = 0x8000_0000;
    const REGION_SIZE: u64 = 0x1000_0000; // 256 MiB
    const PAGE_2M: u64 = 2 * 1024 * 1024;
    const PAGE_16K: u64 = 16 * 1024;

    #[test]
    fn test_should_count_pages_with_2mib_default() {
        let bm = DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_2M).unwrap();
        assert_eq!(bm.page_count(), 128); // 256 MiB / 2 MiB
    }

    #[test]
    fn test_should_track_a_dirty_page_at_an_aligned_address() {
        let bm = DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_2M).unwrap();
        let target = REGION_BASE + 5 * PAGE_2M;
        assert!(!bm.is_dirty(target));
        let transitioned = bm.set_dirty(target);
        assert!(transitioned);
        assert!(bm.is_dirty(target));

        let again = bm.set_dirty(target);
        assert!(!again, "set_dirty must report transition only once");
    }

    #[test]
    fn test_should_round_an_unaligned_address_into_its_page() {
        let bm = DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_2M).unwrap();
        let aligned = REGION_BASE + 3 * PAGE_2M;
        let unaligned = aligned + 0x1234;
        bm.set_dirty(unaligned);
        assert!(bm.is_dirty(aligned));
    }

    #[test]
    fn test_should_reject_addresses_outside_the_tracked_range() {
        let bm = DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_2M).unwrap();
        assert!(!bm.set_dirty(REGION_BASE - 1));
        assert!(!bm.set_dirty(REGION_BASE + REGION_SIZE));
    }

    #[test]
    fn test_should_drain_dirty_pages_in_index_order() {
        let bm = DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_2M).unwrap();
        for &page in &[3u64, 100, 0, 64] {
            bm.set_dirty_by_index(page);
        }
        let mut drained = bm.drain();
        drained.sort_unstable();
        assert_eq!(drained, vec![0, 3, 64, 100]);

        // After drain the bitmap is clean.
        assert!(!bm.is_dirty(REGION_BASE));
        assert_eq!(bm.drain(), Vec::<u64>::new());
    }

    #[test]
    fn test_should_drain_into_callback_without_allocation() {
        let bm = DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_2M).unwrap();
        bm.set_dirty_by_index(0);
        bm.set_dirty_by_index(127);
        let mut count = 0;
        bm.drain_into(|_p| count += 1);
        assert_eq!(count, 2);
    }

    #[test]
    fn test_should_reject_non_power_of_two_page_size() {
        let res = DirtyBitmap::new(REGION_BASE, REGION_SIZE, 3000);
        assert!(matches!(res, Err(SnapshotError::InvalidPath(_))));
    }

    #[test]
    fn test_should_reject_zero_size_range() {
        let res = DirtyBitmap::new(REGION_BASE, 0, PAGE_2M);
        assert!(matches!(res, Err(SnapshotError::InvalidPath(_))));
    }

    #[test]
    fn test_should_fold_coarse_bits_into_fine_bitmap() {
        let coarse = DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_2M).unwrap();
        let fine = DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_16K).unwrap();
        coarse.set_dirty_by_index(2); // 2 MiB block 2 → fine pages 256..384
        coarse.fold_into_finer(&fine).unwrap();
        assert!(fine.is_dirty(REGION_BASE + 2 * PAGE_2M));
        assert!(fine.is_dirty(REGION_BASE + 2 * PAGE_2M + 100 * PAGE_16K));
        assert!(!fine.is_dirty(REGION_BASE + PAGE_2M));
    }

    #[test]
    fn test_should_reject_fold_with_mismatched_ranges() {
        let coarse = DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_2M).unwrap();
        let fine = DirtyBitmap::new(REGION_BASE + PAGE_16K, REGION_SIZE, PAGE_16K).unwrap();
        let res = coarse.fold_into_finer(&fine);
        assert!(matches!(res, Err(SnapshotError::InvalidPath(_))));
    }

    #[test]
    fn test_should_reject_fold_when_target_is_not_finer() {
        let a = DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_2M).unwrap();
        let b = DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_2M).unwrap();
        let res = a.fold_into_finer(&b);
        assert!(matches!(res, Err(SnapshotError::InvalidPath(_))));
    }

    #[test]
    fn test_should_promote_to_fine_when_one_block_exceeds_threshold() {
        let region =
            TrackedRegion::with_threshold(REGION_BASE, REGION_SIZE, PageGeometry::APPLE_SILICON, 2)
                .unwrap();
        assert_eq!(region.granule(), TrackingGranule::Coarse);
        // 10 faults all in one 2 MiB block — exceeds threshold (2) for that
        // block.
        for _ in 0..10 {
            region.mark_dirty(REGION_BASE);
        }
        // Fast-forward: pretend the window has elapsed.
        {
            let mut last = region.controller.last_reset.lock();
            *last = Instant::now()
                .checked_sub(ADAPTIVE_WINDOW * 2)
                .expect("test fixture: monotonic clock starts above the window length");
        }
        let g = region.tick_adaptive();
        assert_eq!(g, TrackingGranule::Fine);
        let bm = region.bitmap();
        assert_eq!(bm.page_size(), PAGE_16K);
    }

    #[test]
    fn test_should_not_promote_when_faults_are_spread_uniformly_across_blocks() {
        // 64 faults across 64 different 2 MiB blocks — each block sees one
        // fault. Threshold is 4 (per block), so step-down must NOT fire even
        // though aggregate fault count (64) is far above threshold. This is
        // the per-block-vs-aggregate regression the Phase 5 review caught.
        let region =
            TrackedRegion::with_threshold(REGION_BASE, REGION_SIZE, PageGeometry::APPLE_SILICON, 4)
                .unwrap();
        assert_eq!(region.granule(), TrackingGranule::Coarse);
        for block in 0..64u64 {
            region.mark_dirty(REGION_BASE + block * PAGE_2M);
        }
        {
            let mut last = region.controller.last_reset.lock();
            *last = Instant::now()
                .checked_sub(ADAPTIVE_WINDOW * 2)
                .expect("test fixture: monotonic clock starts above the window length");
        }
        let g = region.tick_adaptive();
        assert_eq!(
            g,
            TrackingGranule::Coarse,
            "step-down must use per-block counts, not aggregate fault count"
        );
    }

    #[test]
    fn test_should_be_thread_safe_under_concurrent_set_dirty() {
        // 8 threads × 1000 sets at scattered addresses.
        let bm = std::sync::Arc::new(DirtyBitmap::new(REGION_BASE, REGION_SIZE, PAGE_16K).unwrap());
        let mut handles = vec![];
        for t in 0..8u64 {
            let bm = bm.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..1000u64 {
                    let page_idx = t * 1000 + i;
                    let addr = REGION_BASE + page_idx * PAGE_16K;
                    if addr < REGION_BASE + REGION_SIZE {
                        bm.set_dirty(addr);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let drained = bm.drain();
        assert_eq!(drained.len(), 8000);
    }
}
