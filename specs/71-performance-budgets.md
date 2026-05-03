---
title: 71-performance-budgets — targets, bench harness, CI gates
type: design
status: draft
last_updated: 2026-05-03
depends_on: 00-prd.md, 12-hvf-backend.md, 16-snapshots.md
---

# 71 · Performance Budgets — targets, bench harness, CI gates

Status: draft · Owner: workspace · Depends on: [00-prd.md § 11](./00-prd.md#11-success-metrics), [12-hvf-backend.md](./12-hvf-backend.md), [16-snapshots.md](./16-snapshots.md)

## 1. Purpose

State the per-axis performance targets, the methodology for measuring them, and the CI gates that prevent regression. Numbers are **published**, not borrowed from upstream Firecracker.

Per [00-prd.md § 10 NFR3](./00-prd.md#10-non-functional-requirements): benchmarks ship from week 1, not retrofitted at the end.

## 2. Targets (1.0)

| # | Axis | Target | Hardware | How measured |
|---|------|--------|----------|--------------|
| P1 | Cold boot to `/sbin/init` exec | **p50 ≤ 400 ms** | M2 Pro / M3, macOS 15+ | `boot-timer` virtio device records monotonic time from `InstanceStart` to first guest wall-clock read post-`init` |
| P2 | Memory overhead per microVM at idle | **≤ 15 MiB** | as above | `vm.rss - guest_ram_size` after 5 s idle |
| P3 | vCPU exit dispatch latency | **≤ 10 µs / exit** for MMIO-light workloads | as above | criterion benchmark in `crates/vmm/benches/` |
| P4 | vmnet shared-mode throughput | **≥ 1 Gbit/s** sustained | as above | `iperf3 -P 4` between guest and host loopback |
| P5 | Block IO via tokio + spawn_blocking + `F_NOCACHE` | **≥ 100 K IOPS** on Apple SSD | as above | `fio --rw=randread --bs=4k --iodepth=32` |
| P6 | Diff snapshot save (1 GiB RAM, 1% dirty) | **≤ 50 ms** to disk | as above | criterion benchmark with synthetic dirty pattern |
| P7 | Postcopy restore (lazy, 90% pages cold) | **≤ 1 s** to first usable response | as above | end-to-end test |

## 3. Block IO

Hot path: `virtio-block` queue notification → tokio `spawn_blocking` → `pread`/`pwrite` against `F_NOCACHE`-opened fd → IRQ injection. Per CLAUDE.md § Performance:

- `bytes::Bytes` for descriptor payloads; never `Vec<u8>` allocation in the hot path.
- Pre-allocated submission queue per device.
- Tokio blocking pool sized to `num_cpus * 4` (default), bounded so a runaway VM cannot exhaust the runtime.

The libkrun-style `dispatch_io` engine is deferred (see [99-key-decisions.md § D7](./99-key-decisions.md#d7-block-io-tokio-spawn_blocking-not-dispatch_io)). Tokio + spawn_blocking carries us through 1.0; we revisit if P5 misses.

## 4. vCPU exit dispatch (P3)

Per-exit budget includes:

- HVF return-from-`hv_vcpu_run`.
- ESR_EL2 decode in `squib-arch`.
- `VmExit` enum construction.
- VMM dispatch (MMIO bus lookup or PSCI dispatch or sysreg trap handler).
- IRQ injection (if any) before resume.

Target ≤ 10 µs measured on a synthetic MMIO bench (guest issues 1 M loads against a stub virtio device; per-exit is total/1M). Achieved by:

- Fast-path the common ECs (data abort, HVC) without allocation.
- `BTreeMap` lookup on the bus: O(log N) over ≤ 32 entries → trivially cache-resident.
- IRQ shadow in a per-vCPU `Vec<bool>`, not a Mutex-guarded HashSet.

## 5. Network throughput (P4)

`vmnet` shared mode achieves Gbit/s on Apple Silicon when:

- Frame batching: read up to 32 frames per `vmnet_read` call, write up to 32 per `vmnet_write`.
- `BytesMut` pool sized to `MTU * 256` per direction; reused, never freed mid-flight.
- TX path on the device thread; RX path on the libdispatch queue, with the consumer being the device thread.
- Zero-copy where possible: the virtio descriptor's host pointer is the buffer that goes to vmnet (subject to MTU alignment).

`gvproxy` (userspace mode) targets ~300–400 Mbit/s — adequate for inner-dev-loop, not ≥ Gbit/s.

## 6. Snapshot performance (P6, P7)

### 6.1 Diff snapshot save

Walk the dirty bitmap, `pwrite` only dirty pages. With 2 MiB-default granularity and 1% dirty rate on 1 GiB RAM:

- ~5 dirty 2 MiB blocks = 10 MiB to write.
- APFS sequential write ≥ 1 GiB/s on Apple SSD → 10 ms file IO.
- ~30 ms overhead for state-blob encoding + bitmap drain.
- Total ≤ 50 ms target.

If the dirty rate spikes (workload-dependent), the heuristic drops to 4 KiB granularity for hot regions; per-page TLB shootdown cost is the limiter, not file IO.

### 6.2 Postcopy restore

Critical path: page fault → Mach exception → pager thread → `pread` from snapshot file → `mach_vm_protect` → reply. Per fault budget ≤ 100 µs. To meet the "≤ 1 s to first usable response" target:

- Pager pre-warms the stack page and the kernel `.text` region before vCPU 0 runs.
- The first guest page-fault on a cold page is the worst case; subsequent faults overlap with vCPU work.

## 7. Bench harness

`crates/vmm/benches/`:

- `boot.rs` — end-to-end boot timing, criterion `Bencher::iter` over reproducible config.
- `vcpu_exits.rs` — synthetic MMIO loop in a guest stub.
- `block_io.rs` — fio-shaped pattern.
- `net_throughput.rs` — iperf3-shaped, gated on `iperf3` install.
- `snapshot.rs` — Diff and Full save / restore.

Per CLAUDE.md § Performance, criterion is the only bench tool; no hand-rolled timing loops. Bench results land in `docs/perf/<git-sha>/<bench>.json`; a Makefile target diffs against the previous run.

## 8. CI gates

- **Per-PR**: criterion fast bench on each axis (≤ 5 s each); regressions ≥ 20% fail the PR.
- **Per-merge to main**: full criterion run; results stored in `docs/perf/`.
- **Per-release**: full bench under both ad-hoc-signed and notarized builds; the README is updated with the published numbers.

Numbers in this file are targets, not history. Actual measured numbers live in `docs/perf/`.

## 9. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-PERF-1 | Bench harness exists and runs from week 1, not later. | `crates/vmm/benches/` exists at end of [91-impl-plan.md](./91-impl-plan.md) Phase 1 |
| I-PERF-2 | Numbers are published; the README never quotes upstream Firecracker numbers as squib's. | Doc review |
| I-PERF-3 | A 20%+ regression on any P-axis fails the PR. | CI step |
| I-PERF-4 | No `Vec::new` / `Vec::push` per packet in the network hot path; allocator profiling is part of the bench harness. | `dhat` profile in CI bench step |

## 10. Cross-references

- ← Depends on: [00-prd.md](./00-prd.md), [12-hvf-backend.md](./12-hvf-backend.md), [16-snapshots.md](./16-snapshots.md), [30-networking.md](./30-networking.md)
- → Consumed by: [72-testing-strategy.md](./72-testing-strategy.md) (perf-as-CI), [91-impl-plan.md](./91-impl-plan.md) (Phase 1 bench harness)
- ↔ Related research: [docs/research/hvf-performance-and-snapshots.md](../docs/research/hvf-performance-and-snapshots.md)
