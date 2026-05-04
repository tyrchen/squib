//! Dirty-bitmap micro-benchmarks — Phase 7.2 P6 axis substrate.
//!
//! Per [71-performance-budgets.md §
//! 6.1](../../../specs/71-performance-budgets.md#61-diff-snapshot-save) the **Diff snapshot save**
//! budget (`P6 ≤ 50 ms` for a 1 GiB / 1% dirty workload) decomposes into:
//!
//! 1. Walk the dirty bitmap (`drain_into`).
//! 2. `pwrite` only the dirty pages.
//! 3. State-blob encode + bitmap drain.
//!
//! This bench exercises the bitmap operations alone (no IO) so a regression in
//! the lock-free `set_dirty` / `drain_into` path is caught before it shows up as
//! a snapshot-time hit.
//!
//! Run with:
//!   `cargo bench --features bench -p squib-snapshot --bench dirty_bitmap`

use criterion::{Criterion, criterion_group, criterion_main};
use squib_snapshot::DirtyBitmap;

const RAM_BASE: u64 = 0x4000_0000;
const RAM_1GIB: u64 = 1 << 30;
const PAGE_2MIB: u64 = 2 << 20;

fn bench_set_dirty_2mib_random(c: &mut Criterion) {
    // 1 GiB / 2 MiB = 512 pages. The hot loop touches 64 random pages — a
    // `~13%` dirty rate, comfortably above the spec's 1% reference workload.
    let bm = DirtyBitmap::new(RAM_BASE, RAM_1GIB, PAGE_2MIB).expect("bitmap");
    let pages: Vec<u64> = (0..64).map(|i| RAM_BASE + i * PAGE_2MIB).collect();
    c.bench_function("dirty_bitmap/set_dirty_64x_2mib", |b| {
        b.iter(|| {
            for &addr in &pages {
                bm.set_dirty(addr);
            }
        });
    });
}

fn bench_drain_2mib_after_64_dirty(c: &mut Criterion) {
    c.bench_function("dirty_bitmap/drain_after_64_dirty_2mib", |b| {
        b.iter_with_setup(
            || {
                let bm = DirtyBitmap::new(RAM_BASE, RAM_1GIB, PAGE_2MIB).expect("bitmap");
                for i in 0..64 {
                    bm.set_dirty(RAM_BASE + i * PAGE_2MIB);
                }
                bm
            },
            |bm| {
                let mut count = 0u64;
                bm.drain_into(|_idx| count += 1);
                count
            },
        );
    });
}

fn bench_drain_clean_1gib(c: &mut Criterion) {
    // Empty-drain on a 1 GiB / 2 MiB bitmap: only 8 words of `AtomicU64::swap`
    // touched. Sanity-check that a clean drain stays well under a microsecond.
    c.bench_function("dirty_bitmap/drain_clean_1gib_2mib", |b| {
        b.iter_with_setup(
            || DirtyBitmap::new(RAM_BASE, RAM_1GIB, PAGE_2MIB).expect("bitmap"),
            |bm| {
                bm.drain_into(|_idx| {});
            },
        );
    });
}

criterion_group!(
    benches,
    bench_set_dirty_2mib_random,
    bench_drain_2mib_after_64_dirty,
    bench_drain_clean_1gib,
);
criterion_main!(benches);
