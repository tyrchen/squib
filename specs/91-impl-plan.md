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
- `apps/squib-cli/src/cli.rs` covering the entire Firecracker-compatible flag set + squib extensions.
- 16 unit tests passing; `clippy -D warnings` clean; nightly fmt clean.

**Pending Phase 0 follow-ups before Phase 1 kicks off** — grouped by theme, each item links to the D-record that justifies it. None of these are "polish"; each one breaks something downstream if deferred.

*Backend & wire-shape corrections (must precede Phase 1):*

- Drop `BackendKind::Vz` from `squib-core::backend` and drop `--hypervisor` from the CLI — one backend (D1).
- `squib.entitlements` plist with `com.apple.security.hypervisor` only by default (D17). Bridged-enabled binary adds `com.apple.vm.networking` in a separately-signed build; gated by the `bridged` cargo feature in `squib-net`.
- `MACOSX_DEPLOYMENT_TARGET=15.0` in `.cargo/config.toml`; confirm `applevisor = "1.0"` is consumed with `features = ["macos-15-0"]`. Earlier draft used `macos-26-0`, which would surface as runtime symbol errors on macOS 15 (D2 / D18).
- `InstanceState` wire enum collapses to the upstream three values (`"Not started"` / `"Running"` / `"Paused"`) via `LifecyclePhase::wire_state` ([10-data-model.md § 2.2](./10-data-model.md#22-instanceinfo--get-)). Internal richer phases stay; the wire shape narrows.
- `vcpu_count` validation upper bound = 32 (upstream `MAX_SUPPORTED_VCPUS`), not `hv_vm_get_max_vcpu_count()` (D19).
- Snapshot envelope = upstream `Snapshot<MicrovmState>` shape (D5 corrected). Lock this in before Phase 5 starts, otherwise Phase 5 re-does the format.

*Memory & interrupt layout corrections (must precede Phase 1.4 / 1.5):*

- **Memory layout (D22)**: pin virtio-MMIO base at `0x0F00_0000`, PL011 at `0x0E0A_0000`, GICR window `[0x080A_0000, 0x0E0A_0000)`. Earlier draft put virtio at `0x0A00_0000` and silently overlapped GICR for `vcpu_count > 12`. The `squib-arch::layout` module ships with a `const`-evaluated overlap-check (`assert!(VIRTIO_MMIO_BASE > GICR_END_FOR_MAX_VCPU)`) so any future revert breaks the build, not the boot.
- **INTID conventions ([13 § 2.1](./13-arch-and-boot.md#21-gic-interrupt-id-conventions))**: codify the FDT-cell-vs-raw-INTID mapping in a `squib-arch::gic::IntId(u32)` newtype with `from_spi_cell` / `from_ppi_cell` / `as_raw` constructors. The FDT builder, IRQ allocator, and GIC wrapper all consume the newtype, so the recurring "is `SPI 1` cell-1 or INTID-1" confusion can't recur.

*Forward-compat plumbing (rolled into Phases 2 and 5 below):*

- **API timeout taxonomy (D26)**: the per-action-class timeouts in [70-security.md § 6](./70-security.md#6-resource-limits) and the 504 response code land in the controller (Phase 2.3) and the error envelope (Phase 2.1) simultaneously. Adding 504 later means re-touching every SDK-facing test.
- **Snapshot save atomicity (D25)**: `<id>.snap.tmp` / `<id>.mem.tmp` + fsync + rename is the only safe pattern for in-place overwrite. It's non-trivial to retrofit because every Writer call site has to be wrapped; design it in from week 1 of Phase 5 (5.1).
- **`SnapshotError` enum** ([11-runtime-core.md § 6](./11-runtime-core.md#6-error-types)): variants map 1:1 to wire `fault_message` strings, so a rename is a compat-suite golden change. Define the enum once in Phase 5.1, not opportunistically per call site.

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

Calibration: the prior plan estimated ~14 weeks; that assumed the VZ-default backend (~3 weeks to MVP). With HVF-only, the vCPU run-loop port + GIC integration adds ~2 weeks. The compat suite is bigger than originally scoped (24 fewer R/P rows now that VZ is out, but every F row still needs a passing test). Net: +4 weeks, honestly. The D22–D26 corrections add ~0.5 wk in Phase 5 (5.6) and are absorbed elsewhere by the existing slack — Phase 1 / Phase 2 already had the layout work scoped, just under-named.

## 3. Phase 0 — Skeleton + risk retirement (DONE; weeks -1 to 0)

Already shipped. See § 0 above for the deliverables and pending follow-ups.

| #   | Deliverable | Lands in | Effort |
| --- | ----------- | -------- | ------ |
| 0.1 | Workspace skeleton, lints, toolchain pin | crates/, apps/, Cargo.toml | DONE |
| 0.2 | `squib-core` traits | crates/core | DONE |
| 0.3 | API server skeleton | crates/api | DONE |
| 0.4 | CLI parser | apps/squib-cli | DONE |
| 0.5 | Drop `BackendKind::Vz`, drop `--hypervisor` | crates/core, apps/squib-cli | 1 day |
| 0.6 | `squib.entitlements` + `make sign` | Makefile, build/ | 1 day |
| 0.7 | `MACOSX_DEPLOYMENT_TARGET=15.0` | .cargo/config.toml | 30 min |

**Exit gate**: § 0 follow-ups landed; `cargo build && cargo test` green on a fresh M-series Mac on macOS 15+.

## 4. Phase 1 — Foundation: HVF + boot path (weeks 1–6)

The spine. Without these, no other phase produces a runnable VM.

| #   | Task | Spec | Effort |
| --- | ---- | ---- | ------ |
| 1.1 | `squib-hv` HVF binding (Hypervisor / Vm / Vcpu skeletons) via `applevisor = "1.0"`; thread-affinity check ([12 § 4](./12-hvf-backend.md#4-threading-rules)) | [12-hvf-backend.md](./12-hvf-backend.md) | 1 wk |
| 1.2 | vCPU run loop port from libkrun; ESR_EL2 decoder; full `VmExit` enum incl. SMC dispatch returning `PSCI_NOT_SUPPORTED` ([12 § 8](./12-hvf-backend.md#8-behaviour-edges)); per-vCPU `Box<[AtomicU64]>` IRQ shadow bitset ([71 § 4](./71-performance-budgets.md#4-vcpu-exit-dispatch-p3)) | [12-hvf-backend.md § 5,8](./12-hvf-backend.md#5-vcpu-run-loop), [13-arch-and-boot.md § 4](./13-arch-and-boot.md#4-esr_el2-decoder) | 1 wk |
| 1.3 | `squib-gic` wrapper around `hv_gic_*` (sizes queried via `hv_gic_get_*_size`, not hard-coded); `Gic::pulse_spi` for edge-rising lines (D24); `squib-arch::psci` dispatch table | [12-hvf-backend.md § 6](./12-hvf-backend.md#6-gic--hv_gic_-only), [13-arch-and-boot.md § 5](./13-arch-and-boot.md#5-psci-dispatch) | 1 wk |
| 1.4 | `squib-arch::layout` with **D22 const overlap-check** and the `IntId` newtype (FDT-cell ↔ raw-INTID); `squib-loader` (PE / Image / gz / zst); `set_boot_regs` | [13-arch-and-boot.md § 2,2.1,7,8](./13-arch-and-boot.md#2-memory-layout-concrete) | 1 wk |
| 1.5 | `squib-fdt` builder via `vm-fdt = "0.3"`; emits `IntId`-typed interrupts and the D23 boot-args composition rule | [13-arch-and-boot.md § 6,6.1](./13-arch-and-boot.md#6-fdt-skeleton) | 1 wk |
| 1.6 | `squib-vmm::builder::build_microvm_for_boot`; first kernel boot to busybox; smoke test at **vcpu_count = 32** to pin I-AB-6 | [13-arch-and-boot.md § 9](./13-arch-and-boot.md#9-boot-orchestration) | 1 wk |
| 1.7 | Bench harness skeleton (criterion, `crates/vmm/benches/`) including `boot.rs` driving the boot-timer device ([14 § 4.8](./14-virtio-and-devices.md#48-boot-timer)) | [71-performance-budgets.md § 7](./71-performance-budgets.md#7-bench-harness) | 0.5 wk |

**Exit criteria**: `cargo run -- --config-file examples/hello.json` boots an aarch64 demo VM in under 1 s and the serial output shows `/sbin/init` running. The 32-vCPU variant of the same config also boots and `nproc` reports 32 (smoke for D22). Closes M0.

## 5. Phase 2 — API server + JSON config (weeks 1–4, parallel with Phase 1)

Parallelizable with Phase 1. Drives every endpoint against a stub VMM until the boot path lands at week 6.

| #   | Task | Spec | Effort |
| --- | ---- | ---- | ------ |
| 2.1 | `squib-api` axum-on-UDS; `Server: Firecracker API` middleware; body limit; `ApiError` envelope incl. **504 Gateway Timeout** (D26) | [20-firecracker-api.md § 2,3](./20-firecracker-api.md#2-server-shape) | 1 wk |
| 2.2 | Every request / response struct with serde + `validator` rules; `Raw<T>` → `T` `TryFrom` newtypes; per-class collection caps (drives:8, NICs:8, pmem:4) | [10-data-model.md § 2,3](./10-data-model.md#2-http-wire-envelope), [21-api-compat-matrix.md § 2](./21-api-compat-matrix.md#2-field-level-compatibility) | 1.5 wk |
| 2.3 | `RuntimeApiController` state machine; pre-boot vs post-boot admissibility; `ArcSwap<ControllerSnapshot>` read-only fast path (D20); per-action-class timeout taxonomy ([70 § 6](./70-security.md#6-resource-limits)) emitting 504 on overrun | [11-runtime-core.md § 3](./11-runtime-core.md#3-lifecycle), [20-firecracker-api.md § 4,5](./20-firecracker-api.md#4-state-machine) | 1 wk |
| 2.4 | `--config-file` static-config replay path (in-process `ApiAction` sequence; same validation, same errors, same logging as the HTTP path) | [20-firecracker-api.md § 6](./20-firecracker-api.md#6-static-config-file---config-file) | 0.5 wk |
| 2.5 | Per-endpoint unit + integration tests; record-replay against upstream `getting-started.md` curl sequence; soak test interleaving long `PUT /snapshot/load` with rapid `GET /` to pin I-API-7 | [72-testing-strategy.md § 2,3](./72-testing-strategy.md#2-pyramid) | 1 wk |

**Exit criteria**: every endpoint returns the documented status for at least one happy-path call; the upstream `getting-started.md` sequence runs against a stub-VMM squib through `InstanceStart` (which still returns "VMM not yet wired" until Phase 1 lands). Per-action-class timeouts are enforced and verified to surface as 504 (not 500). Phase 2 + Phase 1 together close M1's API surface dimension.

### 5.6 Phase 2.6 — Embedding facade

This phaselet lands after the API controller and VMM loop exist. It does not change Firecracker wire compatibility; it makes the existing runtime usable by other Rust applications in-process.

| # | Task | Spec | Effort |
|---|------|------|--------|
| 2.6.1 | Move the current CLI package from `apps/squib` to `apps/squib-cli`; keep `[[bin]] name = "squib"` and all CLI flags unchanged. | [22-embedding-facade.md § 4](./22-embedding-facade.md#4-crate-and-package-layout), [50-cli.md](./50-cli.md) | 0.5 day |
| 2.6.2 | Add `crates/squib` package `squib` with public `SquibBuilder`, `Squib`, `SquibError`, and runtime options. | [22-embedding-facade.md § 5](./22-embedding-facade.md#5-public-api-contract) | 1 day |
| 2.6.3 | Move shared runtime/VMM loop wiring out of the CLI into the facade crate; CLI delegates to `SquibBuilder::spawn`. | [22-embedding-facade.md § 6](./22-embedding-facade.md#6-runtime-lifecycle) | 1 day |
| 2.6.4 | Add facade tests for stub spawn/dispatch/shutdown and config-file replay with `start_microvm=false`; update crate graph docs and Makefile package references. | [22-embedding-facade.md § 8](./22-embedding-facade.md#8-tests-and-exit-criteria) | 0.5 day |

**Exit criteria**: I-FACADE-1 through I-FACADE-5 hold; `cargo build --bin squib` produces the CLI; facade tests pass on non-macOS without HVF; the full Rust verification gate remains green.

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
| 5.1 | `SnapshotError` enum ([11 § 6](./11-runtime-core.md#6-error-types)); state file + memory file (Full); **D25 atomic save** (`<id>.snap.tmp` + fsync + `rename(2)`); `--describe-snapshot` | [16-snapshots.md § 2,3](./16-snapshots.md#2-state-file), [10-data-model.md § 6](./10-data-model.md#6-snapshot-file-format) | 1 wk |
| 5.2 | vCPU + GIC state save/restore; PSCI-state normalization on restore; `hv_gic_set_state` gated before any `hv_vcpu_run` | [16-snapshots.md § 2](./16-snapshots.md#2-state-file) | 1 wk |
| 5.3 | Dirty page tracking via `hv_vm_protect`-and-fault; `Box<[AtomicU64]>` shadow bitmap; per-RAM-region adaptive step-down ([16 § 4.2](./16-snapshots.md#42-bitmap-sizing-and-adaptive-heuristic)) | [16-snapshots.md § 4](./16-snapshots.md#4-dirty-page-tracking) | 1 wk |
| 5.4 | Diff snapshot path; sparse-of-dirty memory file via `pwrite` at page-aligned offsets | [16-snapshots.md § 4](./16-snapshots.md#4-dirty-page-tracking) | 1 wk |
| 5.5 | Postcopy via Mach exception ports (`squib-host::pager`); both `File` and `Uffd` backends; **`task_swap_exception_ports` for LLDB-coexistence** with re-installation poll on the 1 s `mach_msg` timeout | [16-snapshots.md § 5](./16-snapshots.md#5-postcopy--lazy-restore) | 1 wk |
| 5.6 | LLDB-attach CI test in both orderings (squib-first, lldb-first); fault-injection test for `AtomicCommitFailed` mid-rename; cross-FS temp-path rejection test | [16-snapshots.md § 5,8](./16-snapshots.md#5-postcopy--lazy-restore) | 0.5 wk |

**Exit criteria**: Full and Diff snapshot round-trip in CI; Uffd path passes a "lazy load" test where most pages are never touched; LLDB attach works in both orderings; a forced `rename(2)` failure leaves the previous snapshot pair intact. Closes M3.

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
- Integration tests in `tests/` per crate, plus top-level `apps/squib-cli/tests/` driving the binary via `assert_cmd`.
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
| 1 | Layout regression re-introduces the GICR/virtio overlap (D22) | `const`-evaluated overlap-check in `squib-arch::layout` + I-AB-6 invariant + 32-vCPU smoke test in 1.6 — all three must trip together for a regression to ship |
| 1 | INTID convention drift between FDT, IRQ allocator, and GIC wrapper | Single `IntId(u32)` newtype with `from_spi_cell` / `from_ppi_cell` / `as_raw` constructors; raw `u32` interrupt parameters disallowed by `clippy::disallowed_types` lint at the boundary |
| 2 | API schema drift between manually-typed structs and `firecracker.yaml` | Round-trip property test: parse-then-serialize swagger examples and assert byte-equality |
| 2 | SDKs treat squib's 504 (D26) as a hard failure rather than a retry signal | Document in `docs/api-deviations.md`; verify against `firectl` and `firecracker-go-sdk` in Phase 7 soak (7.4); consider PR upstream if a breaking pattern shows up |
| 3 | virtio-vsock TSI port from libkrun is gnarly | Keep TSI off by default per [99-key-decisions.md § D8](./99-key-decisions.md#d8-tsi-vsock-off-by-default); ship plain virtio-vsock first |
| 4 | `com.apple.vm.networking` denial blocks bridged users | Already mitigated by gvproxy fallback; document expectations early |
| 5 | `hv_vm_protect` TLB cost under high dirty rate | 2 MiB granularity default + adaptive heuristic per [99-key-decisions.md § D11](./99-key-decisions.md#d11-dirty-tracking-2-mib-default-with-host-page-fallback) |
| 5 | Mach exception port edge cases break LLDB (both attach orderings) | `task_swap_exception_ports` + 1 s re-installation poll; CI test in 5.6 covers both orderings |
| 5 | Snapshot save crashes mid-write and corrupts the previous good pair | D25 atomic temp-file + rename pattern + fault-injection test in 5.6 |
| 5 | User points `<id>.snap.tmp` at a different filesystem from `<id>.snap`, breaking the rename atomicity | Pre-flight `statfs`-equivalent check; reject with `SnapshotError::AtomicCommitFailed` before opening either file |
| 6 | Notarytool stalls release | Notarize post-merge async, decoupled from tag |
| 7 | Boot time misses 400 ms | Hand-tuned kernel config + minimal initramfs in `examples/` |

## 13. What "done" looks like at 1.0

*Functional / wire-compat:*

- Compat coverage ≥ 95% of upstream Firecracker `docs/api_requests/` examples passing unmodified.
- `firectl`, `firecracker-go-sdk`, `firecracker-containerd` integration tests pass.
- `docs/api-deviations.md` published, every P / A / R / squib-only row (including the **504** response code from D26) tested.

*Performance (published, not borrowed; per [71-performance-budgets.md § 2](./71-performance-budgets.md#2-targets-10)):*

- p50 boot to `/sbin/init` ≤ 400 ms on M2 Pro / M3.
- Memory overhead ≤ 15 MiB at idle.
- vCPU exit dispatch ≤ 10 µs / exit; vmnet shared throughput ≥ 1 Gbit/s; block IO ≥ 100 K IOPS; Diff snapshot save ≤ 50 ms; postcopy first-response ≤ 1 s.

*Architectural invariants (every one a CI assertion):*

- I-AB-6 (D22): GICR window non-overlap with PL011 / virtio-MMIO holds for `vcpu_count ∈ 1..=32`.
- I-RC-7 / I-RC-8: `Error::EventLoopGone` and `SnapshotError` variant strings are wire-stable.
- I-API-7 (D20): liveness GETs complete in < 5 ms p99 even during a multi-second `PUT /snapshot/load`.
- I-NET-1, I-SEC-1: `unsafe` confined to `squib-hv` and `squib-net::sys`.

*Distribution:*

- Notarized `.pkg` and Homebrew formula available.
- 1.0 git tag and release notes.

*External signal (targets 1.0+1, not gating 1.0):*

- At least one orchestrator's CI runs squib on macOS Apple Silicon.

## 14. What makes this order *correct*, not just plausible

Three principles drive the phase order. State them so a reviewer can argue with the principles instead of nitpicking task ordering:

- **Land contracts before consumers.** The `Vm::protect_memory` shape (Phase 1) determines whether dirty tracking (Phase 5) can be implemented at all. If Phase 5 were attempted first, Phase 1's HVF backend would be designed for the wrong contract and refactored later. Same for `MmdsInterceptor` shape vs virtio-net frontend.
- **Pay design costs once, in the foundation.** Multi-vCPU PSCI dispatch, VmExit shape, FaultMessage envelope, the `IntId` newtype, the `Box<[AtomicU64]>` IRQ shadow — adding any of these later is a refactor of every call site. They land in the spine even if M0 only uses the trivial case.
- **Make silent regressions impossible to ship.** Where the spec calls out a constraint that a future contributor could plausibly violate (D22 layout overlap, D24 edge-pulse shape, D25 atomic-rename, D26 timeout taxonomy), the implementation pairs it with a build-breaking check (`const_assert!`), a lint-level prohibition, or a CI fault-injection test — not just a comment. The D22 GICR overlap was found in expert review because the original spec lacked I-AB-6; the const overlap-check in 1.4 ensures the next reviewer doesn't have to find it again.

## 15. Cross-references

- ← Depends on: [90-roadmap.md](./90-roadmap.md), every component spec
- → Pairs with: [90-roadmap.md](./90-roadmap.md) (stakeholder-facing milestones)
- ↔ Decisions: [99-key-decisions.md](./99-key-decisions.md)
