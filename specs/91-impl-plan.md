---
title: 91-impl-plan — engineer-facing dependency-ordered build
type: impl-plan
status: draft
last_updated: 2026-05-03
depends_on: 90-roadmap.md, every component spec
supersedes: squib-impl-plan.md
---

# 91 · Implementation Plan — engineer-facing dependency-ordered build

Status: draft · Owner: squib · Depends on: every component spec; pairs with [90-roadmap.md](./90-roadmap.md)

## 0. Readiness assessment

What is ready, what isn't, and what blocks Phase 1 today.

**Ready (Phase 0 done):**
- Workspace skeleton (`Cargo.toml`, `rust-toolchain.toml` pinning Rust 1.95, workspace lints).
- `crates/core` (`squib-core`) with `HypervisorBackend`, `Vm`, `Vcpu`, `VmExit`, `GuestRange`, `Protection`, `Error` skeletons.
- `crates/api` (`squib-api`) skeleton with axum-on-UDS server stub.
- `apps/squib/src/cli.rs` covering the entire Firecracker-compatible flag set + squib extensions.
- 16 unit tests passing; `clippy -D warnings` clean; nightly fmt clean.

**Pending Phase 0 follow-ups before Phase 1 kicks off:**
- Drop `BackendKind::Vz` from `squib-core::backend` (HVF-only now per [99-key-decisions.md § D1](./99-key-decisions.md#d1-hvf-only-no-vz)).
- Drop `--hypervisor` from CLI (one backend).
- `squib.entitlements` plist with `com.apple.security.hypervisor` only by default (D17 — `com.apple.vm.networking` is added only for the bridged-enabled separately-signed build); `make sign` Makefile target.
- `MACOSX_DEPLOYMENT_TARGET=15.0` in `.cargo/config.toml`. Confirm `applevisor = "1.0"` is consumed with `features = ["macos-15-0"]` (D2/D18 — earlier draft used `macos-26-0`, which would have produced runtime symbol errors on macOS 15).
- `InstanceState` wire enum: collapse to upstream three values (`"Not started"` / `"Running"` / `"Paused"`) via `LifecyclePhase::wire_state`. Internal richer phases stay; the wire shape narrows. ([10-data-model.md § 2.2](./10-data-model.md#22-instanceinfo--get-).)
- `vcpu_count` validation upper bound = 32 (upstream `MAX_SUPPORTED_VCPUS`), not `hv_vm_get_max_vcpu_count()` (D19).
- Snapshot envelope = upstream `Snapshot<MicrovmState>` shape (D5 corrected) — get this right before Phase 5 starts, otherwise Phase 5 has to re-do the format.

**Open spikes (research-skill territory if ever):**
- None today; every load-bearing assumption has a memo in `docs/research/` or a D-record in [99-key-decisions.md](./99-key-decisions.md).

## 1. Why dependency order ≠ feature order

The roadmap groups work by **what gets unlocked for the user**: M2 brings networking + MMDS; M3 brings snapshots. Engineers building this in M2-then-M3 order would discover too late that:

- The MMIO bus and virtio transport (M1 territory) must be solid before any device-level testing of vmnet (M2). A flaky bus changes every diagnosis.
- Dirty page tracking (M3) imposes constraints on `Vm::protect_memory`'s implementation; if that contract isn't pinned in M0, the HVF backend gets retrofitted twice.
- The compat suite (M5) needs recorded transcripts that depend on the API server (M1) being feature-complete; engineers waiting on M5 to start the suite will run out of calendar time.

So this plan's phases are dependency-ordered. Phases close milestones, but a single phase may straddle two milestones, and a single milestone may need more than one phase.

## 2. Estimated total effort

~18 calendar weeks for one full-time developer, with explicit overhead pads. Two parallelizable streams (HVF/boot path + API server; devices + networking) collapse ~3 weeks. Realistic ship date: 18 weeks from Phase 1 kickoff.

Calibration: the prior plan estimated ~14 weeks; that assumed the VZ-default backend (~3 weeks to MVP). With HVF-only, the vCPU run-loop port + GIC integration adds ~2 weeks. The compat suite is bigger than originally scoped (24 fewer R/P rows now that VZ is out, but every F row still needs a passing test). Net: +4 weeks, honestly.

## 3. Phase 0 — Skeleton + risk retirement (DONE; weeks -1 to 0)

Already shipped. See § 0 above for the deliverables and pending follow-ups.

| #   | Deliverable | Lands in | Effort |
| --- | ----------- | -------- | ------ |
| 0.1 | Workspace skeleton, lints, toolchain pin | crates/, apps/, Cargo.toml | DONE |
| 0.2 | `squib-core` traits | crates/core | DONE |
| 0.3 | API server skeleton | crates/api | DONE |
| 0.4 | CLI parser | apps/squib | DONE |
| 0.5 | Drop `BackendKind::Vz`, drop `--hypervisor` | crates/core, apps/squib | 1 day |
| 0.6 | `squib.entitlements` + `make sign` | Makefile, build/ | 1 day |
| 0.7 | `MACOSX_DEPLOYMENT_TARGET=15.0` | .cargo/config.toml | 30 min |

**Exit gate**: § 0 follow-ups landed; `cargo build && cargo test` green on a fresh M-series Mac on macOS 15+.

## 4. Phase 1 — Foundation: HVF + boot path (weeks 1–6)

The spine. Without these, no other phase produces a runnable VM.

| #   | Task | Spec | Effort |
| --- | ---- | ---- | ------ |
| 1.1 | `squib-hv` HVF binding (Hypervisor / Vm / Vcpu skeletons) via `applevisor = "1.0"` | [12-hvf-backend.md](./12-hvf-backend.md) | 1 wk |
| 1.2 | vCPU run loop port from libkrun; ESR_EL2 decoder; full `VmExit` enum | [12-hvf-backend.md § 5](./12-hvf-backend.md#5-vcpu-run-loop), [13-arch-and-boot.md § 4](./13-arch-and-boot.md#4-esr_el2-decoder) | 1 wk |
| 1.3 | `squib-gic` wrapper around `hv_gic_*`; `squib-arch::psci` dispatch table | [12-hvf-backend.md § 6](./12-hvf-backend.md#6-gic--hv_gic_-only), [13-arch-and-boot.md § 5](./13-arch-and-boot.md#5-psci-dispatch) | 1 wk |
| 1.4 | `squib-loader` (PE / Image / gz / zst); `squib-arch::layout`; `set_boot_regs` | [13-arch-and-boot.md § 2,7,8](./13-arch-and-boot.md#2-memory-layout-concrete) | 1 wk |
| 1.5 | `squib-fdt` builder via `vm-fdt` | [13-arch-and-boot.md § 6](./13-arch-and-boot.md#6-fdt-skeleton) | 1 wk |
| 1.6 | `squib-vmm::builder::build_microvm_for_boot`; first kernel boot to busybox | [13-arch-and-boot.md § 9](./13-arch-and-boot.md#9-boot-orchestration) | 1 wk |
| 1.7 | Bench harness skeleton (criterion, `crates/vmm/benches/`) | [71-performance-budgets.md § 7](./71-performance-budgets.md#7-bench-harness) | 0.5 wk |

**Exit criteria**: `cargo run -- --config-file examples/hello.json` boots an aarch64 demo VM in under 1 s and the serial output shows `/sbin/init` running. Closes M0.

## 5. Phase 2 — API server + JSON config (weeks 1–4, parallel with Phase 1)

Parallelizable with Phase 1. Drives every endpoint against a stub VMM until the boot path lands at week 6.

| #   | Task | Spec | Effort |
| --- | ---- | ---- | ------ |
| 2.1 | `squib-api` axum-on-UDS; `Server: Firecracker API` middleware; body limit | [20-firecracker-api.md § 2,3](./20-firecracker-api.md#2-server-shape) | 1 wk |
| 2.2 | Every request / response struct with serde + `validator` rules | [10-data-model.md § 2,3](./10-data-model.md#2-http-wire-envelope), [21-api-compat-matrix.md § 2](./21-api-compat-matrix.md#2-field-level-compatibility) | 1.5 wk |
| 2.3 | `RuntimeApiController` state machine; pre-boot vs post-boot admissibility | [11-runtime-core.md § 3](./11-runtime-core.md#3-lifecycle), [20-firecracker-api.md § 4](./20-firecracker-api.md#4-state-machine) | 1 wk |
| 2.4 | `--config-file` static-config replay path | [20-firecracker-api.md § 6](./20-firecracker-api.md#6-static-config-file---config-file) | 0.5 wk |
| 2.5 | Per-endpoint unit + integration tests; record-replay against upstream `getting-started.md` curl sequence | [72-testing-strategy.md § 2,3](./72-testing-strategy.md#2-pyramid) | 1 wk |

**Exit criteria**: every endpoint returns the documented status for at least one happy-path call; the upstream `getting-started.md` sequence runs against a stub-VMM squib through `InstanceStart` (which still returns "VMM not yet wired" until Phase 1 lands). Phase 2 + Phase 1 together close M1's API surface dimension.

## 6. Phase 3 — Devices and MMDS (weeks 4–9)

Once the bus and transport are up (4 weeks in; depends on Phase 1's `Vm::map_memory`), every virtio device fans out in parallel.

| #   | Task | Spec | Effort |
| --- | ---- | ---- | ------ |
| 3.1 | `squib-bus` MMIO bus + `BusDevice` trait; ported from libkrun | [14-virtio-and-devices.md § 2](./14-virtio-and-devices.md#2-bus) | 0.5 wk |
| 3.2 | `squib-virtio` MMIO transport ported from cloud-hypervisor | [14-virtio-and-devices.md § 3](./14-virtio-and-devices.md#3-virtio-mmio-transport) | 1 wk |
| 3.3 | virtio-block (sync + async engines) | [14-virtio-and-devices.md § 4.1](./14-virtio-and-devices.md#41-virtio-block) | 1 wk |
| 3.4 | virtio-net frontend (host backend stubbed) | [14-virtio-and-devices.md § 4.2](./14-virtio-and-devices.md#42-virtio-net) | 1 wk |
| 3.5 | virtio-vsock (UDS multiplex; TSI behind opt-in flag) | [14-virtio-and-devices.md § 4.3](./14-virtio-and-devices.md#43-virtio-vsock) | 1 wk |
| 3.6 | virtio-balloon, virtio-rng, virtio-console, virtio-pmem, virtio-mem, boot-timer | [14-virtio-and-devices.md § 4.4–4.8](./14-virtio-and-devices.md#44-virtio-balloon) | 1.5 wk |
| 3.7 | `squib-mmds`: port dumbo + mmds; bind to virtio-net interceptor | [15-mmds.md](./15-mmds.md) | 1 wk |

**Exit criteria**: a guest with rootfs + net (loopback for now) + vsock + balloon + entropy boots, opens a network connection, runs a workload, and `curl 169.254.169.254/foo` returns MMDS data. Closes M1's device-frontend dimension.

## 7. Phase 4 — Networking (weeks 6–10)

Begins when the device frontends from Phase 3 are stable enough to integrate against.

| #   | Task | Spec | Effort |
| --- | ---- | ---- | ------ |
| 4.1 | `squib-net::sys` hand-rolled FFI to `vmnet.framework` | [30-networking.md § 3](./30-networking.md#3-vmnet-binding-squib-netsys) | 1 wk |
| 4.2 | `--network=shared` (NAT) end-to-end | [30-networking.md § 2](./30-networking.md#2-modes) | 1 wk |
| 4.3 | `--network=bridged` gated on entitlement; ships disabled | [30-networking.md § 5](./30-networking.md#5-bridged-mode) | 0.5 wk |
| 4.4 | `--network=userspace` (gvproxy child process) | [30-networking.md § 4](./30-networking.md#4-userspace-mode-gvproxy) | 1 wk |

**Exit criteria**: `curl example.com` from inside a squib guest works in `shared` and `userspace` modes; bridged is exercised on a separately-signed build. Closes M2.

## 8. Phase 5 — Snapshots (weeks 10–14)

The hardest engineering. Postcopy via Mach exception ports is novel for the macOS Rust ecosystem; budget accordingly.

| #   | Task | Spec | Effort |
| --- | ---- | ---- | ------ |
| 5.1 | State file + memory file (Full); `--describe-snapshot` | [16-snapshots.md § 2,3](./16-snapshots.md#2-state-file), [10-data-model.md § 6](./10-data-model.md#6-snapshot-file-format) | 1 wk |
| 5.2 | vCPU + GIC state save/restore; PSCI-state normalization on restore | [16-snapshots.md § 2](./16-snapshots.md#2-state-file) | 1 wk |
| 5.3 | Dirty page tracking via `hv_vm_protect`-and-fault | [16-snapshots.md § 4](./16-snapshots.md#4-dirty-page-tracking) | 1 wk |
| 5.4 | Diff snapshot path | [16-snapshots.md § 4](./16-snapshots.md#4-dirty-page-tracking) | 1 wk |
| 5.5 | Postcopy via Mach exception ports (squib-host::pager) | [16-snapshots.md § 5](./16-snapshots.md#5-postcopy--lazy-restore) | 1 wk |

**Exit criteria**: Full and Diff snapshot round-trip in CI; Uffd path passes a "lazy load" test where most pages are never touched. Closes M3.

## 9. Phase 6 — Jailer + codesign + distribution (weeks 11–15, partial parallel)

| #   | Task | Spec | Effort |
| --- | ---- | ---- | ------ |
| 6.1 | `squib-jail` with upstream flag set + Darwin behaviour | [40-jailer.md](./40-jailer.md) | 1 wk |
| 6.2 | `squib.entitlements`; hardened runtime; `make sign / verify / notarize` | [70-security.md § 9](./70-security.md#9-code-signing--entitlements) | 1 wk |
| 6.3 | Notarytool integration in CI (post-merge async) | [70-security.md § 9](./70-security.md#9-code-signing--entitlements) | 0.5 wk |
| 6.4 | Homebrew formula prep; `.pkg` builder | [00-prd.md § 9 S4](./00-prd.md#9-soft-requirements) | 1 wk |
| 6.5 | Buffer | | 0.5 wk |

**Exit criteria**: `make notarize` produces a stapleable `.pkg` that installs and runs on a fresh Apple Silicon Mac. Closes M4.

## 10. Phase 7 — Compat suite + perf + polish (weeks 15–18)

| #   | Task | Spec | Effort |
| --- | ---- | ---- | ------ |
| 7.1 | `tests/firecracker-compat/`: ingest recorded HTTP transcripts; per-deviation tests for P/A/R rows | [72-testing-strategy.md § 3](./72-testing-strategy.md#3-compat-suite) | 1 wk |
| 7.2 | Bench numbers on every P-axis published in `docs/perf/` | [71-performance-budgets.md](./71-performance-budgets.md) | 0.5 wk |
| 7.3 | Boot-time tuning (kernel config, initramfs) | [71-performance-budgets.md § 2](./71-performance-budgets.md#2-targets-10) | 1 wk |
| 7.4 | Soak: `firectl`, `firecracker-go-sdk`, `firecracker-containerd` end-to-end | [72-testing-strategy.md § 3](./72-testing-strategy.md#3-compat-suite) | 0.5 wk |
| 7.5 | Doc pass; bug-fix swarm | | 1 wk |

**Exit criteria**: 1.0 tag. Closes M5.

## 11. Cross-cutting workstreams

These run continuously alongside the phases above.

### 11.1 Testing

Per [72-testing-strategy.md](./72-testing-strategy.md):

- Unit tests in `#[cfg(test)] mod tests` per file. `rstest` for parameterized; `proptest` for invariants.
- Integration tests in `tests/` per crate, plus top-level `apps/squib/tests/` driving the binary via `assert_cmd`.
- Compat suite (Phase 7).
- Snapshot golden tests in `tests/fixtures/snapshots/`.
- Performance tests with criterion (Phase 1 onward).
- CI matrix: macOS 15 + macOS 26.

### 11.2 Security

Per [70-security.md](./70-security.md):

- `cargo audit` + `cargo deny check` on every CI run.
- Boundary input lints denied in `squib-api` and the VMM event loop's dispatch.
- Code-signing in CI: ad-hoc-signed for tests, Developer ID for releases.
- Security review at week 15 freeze.
- `#![forbid(unsafe_code)]` everywhere except `squib-hv` and `squib-net::sys`; each `unsafe` block annotated `// SAFETY:`.

### 11.3 Documentation

- `docs/` mirrors upstream Firecracker's structure (`getting-started.md`, `device-api.md`, `mmds/`, `vsock.md`, `snapshotting/`, `logger.md`, `metrics.md`) with squib-specific sections marked.
- `docs/api-deviations.md` enumerates every P / A / R row from [21-api-compat-matrix.md](./21-api-compat-matrix.md) with reproductions.
- `docs/macos-setup.md` covers entitlements, code-signing, gvproxy install, network-mode tradeoffs.

### 11.4 Upstream tracking

- Pin to Firecracker minor version (currently 1.16). Each release cycle:
  1. Diff `firecracker.yaml` against last pin.
  2. File issues for new endpoints / fields.
  3. Run the compat suite against the new pin's recorded transcripts.

A nightly cron CI lane catches unannounced upstream additions.

## 12. Risks called out by phase

| Phase | Risk | Mitigation |
|-------|------|------------|
| 1 | `applevisor` API surface gap (something missing for our run-loop) | Drop to `applevisor-sys` raw FFI for that call; isolated by [12-hvf-backend.md § 4](./12-hvf-backend.md#4-threading-rules) |
| 1 | HVF version skew between macOS 15 and 26 | CI matrix covers both; behavioral tests for known-tricky paths (vtimer, sysreg trap) |
| 2 | API schema drift between manually-typed structs and `firecracker.yaml` | Round-trip property test: parse-then-serialize swagger examples and assert byte-equality |
| 3 | virtio-vsock TSI port from libkrun is gnarly | Keep TSI off by default per [99-key-decisions.md § D8](./99-key-decisions.md#d8-tsi-vsock-off-by-default); ship plain virtio-vsock first |
| 4 | `com.apple.vm.networking` denial blocks bridged users | Already mitigated by gvproxy fallback; document expectations early |
| 5 | `hv_vm_protect` TLB cost under high dirty rate | 2 MiB granularity default + adaptive heuristic per [99-key-decisions.md § D11](./99-key-decisions.md#d11-dirty-tracking-2-mib-default-with-host-page-fallback) |
| 5 | Mach exception port edge cases break LLDB | Save and forward to prior handlers; LLDB-attach test in CI |
| 6 | Notarytool stalls release | Notarize post-merge async, decoupled from tag |
| 7 | Boot time misses 400 ms | Hand-tuned kernel config + minimal initramfs in `examples/` |

## 13. What "done" looks like at 1.0

- Compat coverage ≥ 95% of upstream Firecracker `docs/api_requests/` examples passing unmodified.
- p50 boot to `/sbin/init` ≤ 400 ms on M2 Pro / M3, measured and published.
- Memory overhead ≤ 15 MiB at idle.
- Notarized `.pkg` and Homebrew formula available.
- `firectl`, `firecracker-go-sdk`, `firecracker-containerd` integration tests pass.
- At least one orchestrator's CI runs squib on macOS Apple Silicon (in flight; targets 1.0+1).
- `docs/api-deviations.md` published, every deviation tested.
- 1.0 git tag and release notes.

## 14. What makes this order *correct*, not just plausible

Two principles drive the phase order. State them so a reviewer can argue with the principles instead of nitpicking task ordering:

- **Land contracts before consumers.** The `Vm::protect_memory` shape (Phase 1) determines whether dirty tracking (Phase 5) can be implemented at all. If Phase 5 were attempted first, Phase 1's HVF backend would be designed for the wrong contract and refactored later. Same for `MmdsInterceptor` shape vs virtio-net frontend.
- **Pay design costs once, in the foundation.** Multi-vCPU PSCI dispatch, VmExit shape, FaultMessage envelope — adding any of these later is a refactor of every call site. They land in the spine even if M0 only uses the trivial case.

## 15. Cross-references

- ← Depends on: [90-roadmap.md](./90-roadmap.md), every component spec
- → Pairs with: [90-roadmap.md](./90-roadmap.md) (stakeholder-facing milestones)
- ↔ Decisions: [99-key-decisions.md](./99-key-decisions.md)
