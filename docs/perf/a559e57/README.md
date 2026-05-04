# Perf numbers — `a559e57` (Phase 7 baseline)

Captured during Phase 7's bench harness wiring (M5-track). Each row is a real
criterion measurement; "deferred" rows are listed so the gap is visible at a
glance, not hidden.

## Host

- **CPU**: Apple M4 Max (Mac16,5)
- **OS**: macOS 15.7.3 (Sequoia)
- **Build**: `cargo bench --features bench` against the squib master at
  `a559e57`. Stub VMM in scope; no live HVF on the bench path yet.

## Substrate measurements (P6 / P7 rolls these up)

| Bench                                       | Mean (ns) | Comment |
|---------------------------------------------|----------:|---------|
| `dirty_bitmap/set_dirty_64x_2mib`           |       118 | 64× `AtomicU64::fetch_or` — ~1.84 ns/page on a 1 GiB / 2 MiB-tracking layout. |
| `dirty_bitmap/drain_after_64_dirty_2mib`    |        46 | Walks 8 64-bit words; calls back ~64 dirty bits. Linear in dirty pages, not in RAM size. |
| `dirty_bitmap/drain_clean_1gib_2mib`        |        24 | 8 `AtomicU64::swap` reads, no callbacks. Bounded by cache-line residency. |
| `build_microvm_for_boot/planning`           |    23 407 | Validates `VmResources` and synthesises the layout; HVF init is single-shot per process and excluded. |

## What this implies for P6 (Diff snap ≤ 50 ms / 1 GiB / 1%)

Per [`specs/71-performance-budgets.md § 6.1`](../../../specs/71-performance-budgets.md#61-diff-snapshot-save):

- 1% dirty rate on 1 GiB / 2 MiB tracking ≈ **5 dirty 2 MiB blocks**.
- Bitmap walk cost ≈ 46 ns (already amortised below the noise floor).
- File IO at 5 page-aligned `pwrite`s ≈ 12–50 ms on Apple SSD per the spec.
- ~20–30 ms for state-blob encoding via bitcode + CRC.

So the bitmap is **definitively not the bottleneck**. The end-to-end measurement
remains deferred until Phase 1's live vCPU run-loop tail lands; once it does, the
existing `criterion` harness captures the full envelope.

## Deferred axes

P1 / P2 / P3 / P4 / P5 / P7 (live boot, RSS, exit dispatch, network, block IO,
postcopy first response) all need a live VMM event loop. The Phase 1 cross-phase
blockers tracked in
[`specs/93-improvements-review.md`](../../../specs/93-improvements-review.md)
gate every one of these. When that lands, the bench harness is already in place
to drive them.

## Raw criterion output

`make bench-publish` copies `target/criterion/<bench>/new/estimates.json` (and
the HTML report) into this directory under per-bench subdirs. Re-run on a
representative host (M2 Pro / M3) before publishing release numbers in the
top-level README.
