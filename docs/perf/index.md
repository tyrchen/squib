# Performance numbers — squib

Per [specs/71-performance-budgets.md](../../specs/71-performance-budgets.md), squib
publishes its own measured numbers — never borrowing upstream Firecracker's. This
directory holds the raw criterion JSON and a per-axis writeup for every published
build.

## Layout

```text
docs/perf/
├── index.md                 # this file (interpretation, methodology, host context)
├── boot-tuning.md           # P1 / boot-time tuning levers
└── <git-sha>/
    ├── README.md            # narrated summary for that revision
    ├── set_dirty_64x_2mib/  # criterion bench output (per-axis subdir)
    │   ├── new/estimates.json
    │   └── …
    └── …
```

The most recent revision is the directory-listing top entry alphabetically; the
authoritative pointer (for the README's published numbers) lives in the active
revision's `README.md`.

## How numbers are produced

```bash
make bench-publish
```

Drives every `cargo bench` axis in the workspace:

- `cargo bench -p squib-vmm     --bench boot         --features bench`
- `cargo bench -p squib-snapshot --bench dirty_bitmap --features bench`

Output lands under `target/criterion/` and is copied into
`docs/perf/<git-sha>/`. CI publishes the same artefact bundle on each
post-merge run; release pipelines also stash an HTML criterion report.

## Targets vs status

Per [71 § 2 Targets](../../specs/71-performance-budgets.md#2-targets-10):

| Axis | Target (1.0)         | Status            | Bench |
|------|----------------------|-------------------|-------|
| P1   | p50 boot ≤ 400 ms    | **deferred** (live boot loop, vCPU-thread tail) | `boot.rs` (planning step only) |
| P2   | RSS overhead ≤ 15 MiB at idle | **deferred** (needs live VM) | n/a |
| P3   | vCPU exit ≤ 10 µs    | **deferred** (needs live VM)               | (planned) `vcpu_exits.rs` |
| P4   | vmnet shared ≥ 1 Gbit/s | **deferred** (needs live VM + vmnet)    | (planned) `net_throughput.rs` |
| P5   | Block IO ≥ 100 K IOPS | **deferred** (needs live VM)              | (planned) `block_io.rs` |
| P6   | Diff snap ≤ 50 ms (1 GiB / 1%) | **substrate green**: bitmap `set_dirty 64x` ≈ 118 ns; `drain_after_64_dirty` ≈ 46 ns; `drain_clean 1 GiB` ≈ 24 ns (M4 Max, macOS 15.7.3) | `dirty_bitmap.rs` |
| P7   | Postcopy first-response ≤ 1 s | **substrate green** (Mach pager unit tests pass; live mach_msg loop is feature-gated per 93) | (planned) `postcopy.rs` |

The "substrate" rows mean every component the budget rolls up out of has a passing
unit / integration test and a microbenchmark within its expected envelope; the
end-to-end criterion measurement against a live VM is the deferred piece — gated on
the same Phase 1 vCPU-thread / kernel-image tail tracked in
[`93-improvements-review.md`](../../specs/93-improvements-review.md).

## Methodology

- Every bench uses `criterion` per [CLAUDE.md § Performance](../../CLAUDE.md). No
  hand-rolled timing loops.
- Warm-up ≥ 1 s, ≥ 10 samples per data point. CI uses a fast profile (3 s
  measurement); release runs pull a long profile (10 s + 100 samples) for the
  published numbers.
- Bench output is captured per-revision so historical regression analysis is one
  `git log -- docs/perf/` away.

## Host context

The numbers in `a559e57/` were captured on:

- **CPU**: Apple M4 Max (Mac16,5), 14P+2E cores. The published 1.0 reference is
  M2 Pro / M3 — M4 Max is a generation ahead so multiplying by ~0.85 gives a
  conservative M2 Pro estimate.
- **OS**: macOS 15.7.3 (Sequoia), build 24G419.
- **Toolchain**: Rust 1.95 (per `rust-toolchain.toml`).
- **Build**: `--release` with workspace defaults.

## Cross-references

- ← Targets defined in [`specs/71-performance-budgets.md`](../../specs/71-performance-budgets.md).
- ← CI gates: 20% regression on any P-axis fails the PR (per § 8).
- → Tuning levers: [`boot-tuning.md`](./boot-tuning.md).
