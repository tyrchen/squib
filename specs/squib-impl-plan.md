---
title: squib — Implementation Plan
type: impl-plan
status: draft
last_updated: 2026-05-03
depends_on: squib-prd.md, squib-design.md, squib-api-compat-design.md
supersedes: prior multi-version impl-plan (v0.1→v0.5)
---

# squib — Implementation Plan

The PRD says **single 1.0 release with full Firecracker compatibility on day-one**. This plan rejects the earlier "v0.1 boots a kernel, v0.4 brings HVF online" phasing. There is no public v0.1 — internal phases exist for engineering sequencing only, and there is exactly one publishable artifact: 1.0.

## Approach

The 1.0 work is sequenced as **eight construction tracks**, each with explicit entry/exit criteria, that fan out and rejoin. The critical path is HVF backend → vCPU run loop → boot path → first-kernel-boot. Everything else can parallelize once the HVF and boot path are working. We aim to have a workable squib (HTTP API + boots + vsock + snapshot save) at week 12, then spend weeks 13-18 hardening, polishing, and benchmarking.

```
                        ┌──────────────────────────────────────┐
                        │  Phase 0: Skeleton (DONE)            │
                        └────────────────┬─────────────────────┘
                                         │
                ┌────────────────────────┼────────────────────────┐
                ▼                        ▼                        ▼
        ┌───────────────┐        ┌───────────────┐        ┌───────────────┐
        │ Track A:      │        │ Track B:      │        │ Track C:      │
        │ HVF backend   │        │ API server    │        │ Devices       │
        │ + vCPU run    │        │ + JSON config │        │ (block/net/   │
        │ + GIC + PSCI  │        │ + Firecracker │        │  vsock/...)   │
        │ + boot path   │        │ surface       │        │ on top of bus │
        └──────┬────────┘        └──────┬────────┘        └──────┬────────┘
               │                        │                        │
               └────────────┬───────────┴────────────┬───────────┘
                            ▼                        ▼
                   ┌────────────────┐       ┌─────────────────┐
                   │ Track D: MMDS  │       │ Track E: Network│
                   │ + dumbo port   │       │ vmnet + gvproxy │
                   └────────┬───────┘       └────────┬────────┘
                            └──────────┬─────────────┘
                                       ▼
                            ┌────────────────────┐
                            │ Track F: Snapshots │
                            │ + dirty tracking   │
                            │ + Mach-exc postcopy│
                            └──────────┬─────────┘
                                       ▼
                            ┌────────────────────┐
                            │ Track G: Jailer +  │
                            │ codesign + dist    │
                            └──────────┬─────────┘
                                       ▼
                            ┌────────────────────┐
                            │ Track H: Compat    │
                            │ suite + perf bench │
                            └──────────┬─────────┘
                                       ▼
                                 1.0 release
```

## Phase 0 — Skeleton (DONE, weeks -1 to 0)

The workspace skeleton, `squib-core` traits, CLI parser, and Makefile already shipped. Build/test/clippy/fmt all green on the skeleton. See the existing `crates/core` and `apps/squib` for the entry points.

**Already delivered:**
- `rust-toolchain.toml` pinning Rust 1.95 stable.
- Workspace `Cargo.toml` with workspace lints (`pedantic`, `unreachable_pub`, etc.) and dependency catalogue.
- `crates/core` (squib-core) with the trait surface (`HypervisorBackend`, `Vm`, `Vcpu`, `VmExit`, `GuestRange`, `Protection`, `Error`).
- `apps/squib/src/cli.rs` covering the entire Firecracker-compatible flag set + squib extensions.
- 16 unit tests passing, clippy `-D warnings` clean, nightly fmt clean.

**Pending Phase 0 follow-ups:**
- Drop `BackendKind::Vz` from `squib-core` (HVF-only now).
- Drop `--hypervisor` from CLI (only one backend).
- Update entitlements scaffolding: `squib.entitlements` with `com.apple.security.hypervisor` + `com.apple.vm.networking`.
- `MACOSX_DEPLOYMENT_TARGET=15.0` in `.cargo/config.toml`.

## Track A — HVF backend, vCPU loop, GIC, PSCI, boot path (critical path, weeks 1-6)

**Outcome:** an aarch64 Linux kernel boots inside squib, `/sbin/init` runs, console output observable on stdout.

- **Week 1 — `squib-hv` ground floor.**
  - [ ] Add `applevisor = "1.0"` with `features = ["macos-26-0"]` as the only consumer.
  - [ ] Implement `Hypervisor::create_vm`, `Vm::map_memory`, `Vm::create_vcpu` skeletons.
  - [ ] Wire `applevisor::Vm` and `applevisor::Vcpu` lifetime to squib types.
  - [ ] Codesign Makefile target with the entitlements plist; verify `cargo run -- --version` works after sign.

- **Week 2 — vCPU run loop port from libkrun.**
  - [ ] Port `containers/libkrun/src/hvf/src/lib.rs::HvfVcpu::run` to use `applevisor` calls.
  - [ ] Implement ESR_EL2 decoder per `docs/research/aarch64-hvf-guest-stack.md` §8.4.
  - [ ] `VmExit` enum complete with all variants (Mmio, Hvc, Smc, SystemRegister, Wfi/Wfe, VtimerActivated, Brk, Reset, Shutdown, Cancelled).
  - [ ] One vCPU thread per vCPU; mpsc command channel for Pause/Resume/Shutdown.

- **Week 3 — GIC and PSCI.**
  - [ ] `squib-gic`: wrap `applevisor::gic::*` for `hv_gic_create`, `hv_gic_set_spi`, `hv_gic_send_msi`, snapshot APIs.
  - [ ] `squib-arch::psci` with the function dispatch table from `docs/research/aarch64-hvf-guest-stack.md` §4.
  - [ ] PSCI `CPU_ON` correctly brings up secondary vCPUs.

- **Week 4 — Boot path.**
  - [ ] `squib-loader`: PE loader via `linux-loader::pe::PE::load`; gzip detection + `flate2` decompression; zstd via `zstd` crate.
  - [ ] `squib-arch::layout` constants per `docs/research/aarch64-hvf-guest-stack.md` §11.
  - [ ] `squib-arch::regs::set_boot_regs(vcpu, kernel_addr, fdt_addr)` setting PC, X0-X3, PSTATE=0x3C5.

- **Week 5 — FDT.**
  - [ ] `squib-fdt` builder per `docs/research/aarch64-hvf-guest-stack.md` §2.2 using `vm-fdt`.
  - [ ] Generate `/`, `/chosen`, `/memory`, `/cpus[N]`, `/psci`, `/timer`, `/intc`, `/pl011`, `/virtio_mmio[K]`, `/clocks`.

- **Week 6 — First kernel boot.**
  - [ ] Wire all of the above into `squib-vmm::builder::build_microvm_for_boot`.
  - [ ] Boot a known-good aarch64 vmlinux + busybox initrd to a busybox shell.
  - [ ] Capture serial output via PL011 to a file.

**Exit criteria:** `cargo run -- --config-file examples/hello.json` boots the demo VM in under 1 s and the serial output shows `/sbin/init` running.

## Track B — API server + static config (weeks 1-4, parallel with Track A)

**Outcome:** every Firecracker REST endpoint accepts the right shapes and replies with the right errors.

- [ ] `squib-api` with axum on a Unix socket. Middleware injects `Server: Firecracker API`.
- [ ] Define every request/response struct from the API compat matrix with full serde annotations and `validator` boundary checks.
- [ ] Error type maps to `(StatusCode, Json<FaultMessage>)`.
- [ ] `RuntimeApiController` validating pre-boot vs post-boot admissibility; outputs `ApiAction` enum.
- [ ] `--config-file` path: parse `VmmConfig` (kebab-case top-level keys), replay as a deterministic sequence of `ApiAction` calls.
- [ ] Unit tests per endpoint: success + each error class. Integration test that records a Firecracker API transcript and replays against squib (the "stub-VMM" version, before Track A's first-boot lands).

**Exit criteria:** the public Firecracker `getting-started.md` curl sequence runs against squib and gets back the same `204`s and `200`s for every request through `InstanceStart` (which still returns "VMM not yet wired" until Track A meets Track B at week 6).

## Track C — Devices (weeks 4-9, after Track A bus is up)

**Outcome:** all virtio-MMIO devices Firecracker exposes work in squib.

- **Week 4 (parallel with Track A's boot path) — bus and transport.**
  - [ ] `squib-bus`: MMIO bus ported from libkrun (`BTreeMap<BusRange, Arc<Mutex<dyn BusDevice>>>`).
  - [ ] `squib-virtio` MMIO transport ported from cloud-hypervisor (uses upstream `virtio-queue`).

- **Week 5 — virtio-block.**
  - [ ] Port from cloud-hypervisor `virtio-devices/src/block.rs`.
  - [ ] Sync engine via blocking IO; Async engine via tokio `spawn_blocking` pool against `F_NOCACHE`-opened files.
  - [ ] Token-bucket rate limiter.

- **Week 6 — virtio-net.**
  - [ ] Port frontend from cloud-hypervisor `virtio-devices/src/net.rs`.
  - [ ] Host backend stub; full `vmnet` integration arrives in Track E.

- **Week 7 — virtio-vsock.**
  - [ ] Port from libkrun (`src/devices/src/virtio/vsock/`), including UDS multiplex and the optional TSI mode.

- **Week 8 — balloon, rng, console, boot-timer, pmem, virtio-mem.**
  - [ ] Each ~1-2 days; all from cloud-hypervisor or fresh.
  - [ ] virtio-mem (memory hotplug) requires Vm slot management — verify HVF allows the unmap/remap pattern.

- **Week 9 — Buffer + polish.**
  - [ ] All devices pass their per-spec functional tests.
  - [ ] Rate limiters work end-to-end.

**Exit criteria:** a guest with rootfs + net (loopback for now) + vsock + balloon + entropy boots, opens a network connection, and runs a workload.

## Track D — MMDS / dumbo (week 7, parallel with Track C)

**Outcome:** `PUT /mmds` data is reachable from inside the guest at `169.254.169.254`.

- [ ] `squib-mmds`: port `dumbo` and `mmds` crates from upstream Firecracker (Apache-2.0). They're OS-agnostic.
- [ ] Wire to virtio-net device's frame interception path.
- [ ] V1, V2 (IMDSv2 token), and `imds_compat` content negotiation.

**Exit criteria:** a guest with `network-interfaces` + `mmds-config` does `curl 169.254.169.254/foo` and gets the right response shape, V1 and V2 both.

## Track E — Networking (vmnet + gvproxy, weeks 6-10)

**Outcome:** `--network=shared` (NAT) works out of the box; `--network=userspace` (gvproxy) works with no entitlement.

- **Week 6 — `squib-net::sys`** — hand-rolled FFI to `vmnet.framework` (no maintained crate).
  - [ ] `vmnet_start_interface`, `vmnet_read`, `vmnet_write`, `vmnet_stop_interface` against `dispatch_queue`.
  - [ ] ~300 lines of `unsafe`, isolated in `squib-net::sys` (the second of two `unsafe` boundaries; `squib-hv` is the first).
- **Week 7-8 — Shared mode.** NAT, default. End-to-end ping and TCP from guest to host.
- **Week 9 — Bridged mode.** Gated on `com.apple.vm.networking` (restricted form). Ships disabled by default; enabled in build by users with the entitlement.
- **Week 10 — Userspace mode.** Bundle `gvproxy` as a child process. UDS for control plane.

**Exit criteria:** `curl example.com` from inside a squib guest works in `shared` and `userspace` modes; bridged is exercised on a separately-signed build.

## Track F — Snapshots + dirty tracking + postcopy (weeks 10-14)

**Outcome:** Full and Diff snapshots round-trip; postcopy via Mach exception ports works.

- **Week 10 — State file + memory file (Full).**
  - [ ] `squib-snapshot::state`: `MicrovmState` with serde + bitcode encoding; magic-id matches upstream Firecracker; CRC64 trailer.
  - [ ] `squib-snapshot::memory`: full memory dump via `pwrite`.
  - [ ] `PUT /snapshot/create` Full path.
  - [ ] `PUT /snapshot/load` File path.
  - [ ] `--describe-snapshot` reads upstream files structurally.

- **Week 11 — vCPU and GIC state.**
  - [ ] Save: `applevisor::Vcpu::sys_reg_get` over the curated sysreg list; `hv_gic_state_get_data`.
  - [ ] Restore: same in reverse, with PSCI state machine reset to BSP-running / secondaries-Off.
  - [ ] CI test: snapshot, kill, restore — guest counter continues.

- **Week 12 — Dirty page tracking.**
  - [ ] `squib-snapshot::dirty`: write-protect-and-fault scheme per `docs/research/hvf-performance-and-snapshots.md` §2.2.
  - [ ] Default 2 MiB granularity; 4 KiB only for hot regions per heuristic.
  - [ ] Wire to `track_dirty_pages: true` in machine-config.

- **Week 13 — Diff snapshots.**
  - [ ] `PUT /snapshot/create` Diff path: walk dirty bitmap, write only dirty pages to memory file.
  - [ ] CI test: take Full snapshot, run workload, take Diff, restore → workload state preserved.

- **Week 14 — Postcopy via Mach exception ports.**
  - [ ] `squib-host::pager`: task-level `EXC_MASK_BAD_ACCESS` exception port, dedicated server thread, MIG message decoding.
  - [ ] Save and forward to prior exception ports (LLDB attach must keep working).
  - [ ] `mem_backend.backend_type=Uffd` accepts a UDS path; page-server connects, receives page-fault notifications, serves pages.
  - [ ] Test: snapshot a 2 GiB-RAM VM, restore with Uffd, verify only touched pages are paged in.

**Exit criteria:** Full and Diff snapshot round-trip in CI; Uffd path passes a "lazy load" test where most pages are never touched.

## Track G — Jailer + codesigning + distribution (weeks 11-15)

**Outcome:** `squib-jail` ships; `make sign` produces a notarized, codesigned binary.

- **Week 11 — `squib-jail`.**
  - [ ] CLI matches upstream jailer flag-for-flag.
  - [ ] Implements: `chroot()` + binary copy, `setrlimit`, `setuid`/`setgid`, `--daemonize` (setsid + redirect).
  - [ ] Accepts and warns: `--cgroup`, `--parent-cgroup`, `--cgroup-version`, `--netns`, `--new-pid-ns`.
  - [ ] Optional `--macos-sandbox-profile <name>` applies bundled `sandbox_init` profile.

- **Week 12 — Code-signing.**
  - [ ] `squib.entitlements` plist with `com.apple.security.hypervisor` and `com.apple.vm.networking`.
  - [ ] Makefile targets: `make sign`, `make verify`, `make notarize`.
  - [ ] Hardened runtime flag (`--options runtime`).

- **Week 13-14 — Distribution channels.**
  - [ ] Notarytool integration in CI (post-merge async).
  - [ ] Homebrew formula prep (tap or core).
  - [ ] `.pkg` builder.

- **Week 15 — Buffer.**

**Exit criteria:** `make notarize` produces a stapleable `.pkg` that installs and runs on a fresh Apple Silicon Mac.

## Track H — Compat suite + benchmarks + polish (weeks 15-18)

**Outcome:** every row in `squib-api-compat-design.md` has a passing test or a documented skip; published boot-time and memory benchmarks.

- **Week 15 — Compat suite.**
  - [ ] `tests/firecracker-compat/`: ingest recorded HTTP transcripts from upstream Firecracker's `tests/integration_tests/`, replay against squib, assert status + body match. Each transcript yields a `#[test]`.
  - [ ] Per-deviation tests for P/A/R rows.

- **Week 16 — Benchmarks.**
  - [ ] `crates/squib-vmm/benches/` with `criterion` benches for: cold boot to `/sbin/init`, vCPU exit dispatch, vmnet throughput, block IOPS.
  - [ ] Publish numbers in `docs/perf/` with the methodology.

- **Week 17 — Boot-time tuning.**
  - [ ] Hand-tune a reference Linux kernel config for fast cold boot (`pci=off`, `console=hvc0`, lz4 compression, minimal initramfs).
  - [ ] Ship the reference kernel + initramfs as part of `examples/`.
  - [ ] Aim for p50 ≤ 400 ms.

- **Week 18 — Soak.**
  - [ ] Run `firectl`, `firecracker-go-sdk`, `firecracker-containerd` examples end-to-end.
  - [ ] Bug-fix swarm.
  - [ ] Doc pass.

**Exit criteria:** 1.0 tag.

## Cross-cutting workstreams

These run continuously alongside the tracks above.

### Testing strategy

- **Unit tests** in `#[cfg(test)] mod tests` per file. `rstest` for parameterized; `proptest` for invariants on JSON schemas.
- **Integration tests** in `tests/` per crate, plus a top-level `apps/squib/tests/` driving the binary via `assert_cmd`.
- **Compat suite**: see Track H.
- **Snapshot golden tests**: bitcode payloads in `tests/fixtures/snapshots/`.
- **Performance tests**: `criterion`, see Track H.
- **CI matrix**: macOS 15 Sequoia + macOS 26 Tahoe. Both run the full test suite.

### Security workstream

- `cargo audit` on every CI run.
- `cargo deny check` enforcing license allowlist (Apache-2.0, MIT, BSD; LGPL banned).
- Boundary input lints (`unwrap_used`, `expect_used`, `indexing_slicing`, `panic`) denied in `squib-api` and `squib-vmm` boundary modules.
- Code-signing in CI; ad-hoc-signed for tests, Developer ID for releases.
- Security review at week 15 freeze.
- `#![forbid(unsafe_code)]` everywhere except `squib-hv` and `squib-net::sys`. Each `unsafe` block annotated `// SAFETY:`.

### Docs workstream

- `docs/` mirrors upstream Firecracker's structure (`getting-started.md`, `device-api.md`, `mmds/`, `vsock.md`, `snapshotting/`, `logger.md`, `metrics.md`) with squib-specific sections marked.
- `docs/api-deviations.md` enumerates every P/A/R row from the API matrix with reproductions.
- `docs/macos-setup.md` covers entitlements, code-signing, gvproxy install, network-mode tradeoffs.

### Upstream tracking

- Pin to Firecracker minor version (currently 1.16). Each release cycle:
  1. Diff `firecracker.yaml` against last pin.
  2. File issues for new endpoints/fields.
  3. Run the compat suite against the new pin's recorded transcripts.

## Decision log

- **D1.** Vendor or git-dep `dumbo` and `mmds` from upstream Firecracker. Decision: **vendor** (copy with attribution); revisit if maintenance bandwidth allows tracking upstream.
- **D2.** Embed OpenAPI document at `GET /openapi.json`. Decision: **yes**, behind a flag.
- **D3.** Default snapshot format version: pin to upstream 1.16; bump in lockstep.
- **D4.** Default min macOS: **15 Sequoia**. macOS 14 not supported.
- **D5.** TSI vsock default: **off**, opt-in via config.
- **D6.** Bundled reference kernel + initramfs: **yes**, in `examples/`.
- **D7.** Cross-host snapshot replay: **not supported**. State this clearly in docs.
- **D8.** virtio-PCI: **out of scope** for 1.0.
- **D9.** Userspace GIC fallback (for macOS 14): **out of scope**, ever.

## Risks called out by track

| Track | Risk | Mitigation |
|-------|------|------------|
| A | `applevisor` API surface gap (something missing for our run-loop) | Drop to `applevisor-sys` raw FFI for that call |
| A | HVF version skew between macOS 15 and 26 (subtle behavioral diffs) | CI matrix covers both; behavioral tests for known-tricky paths (vtimer, sysreg trap) |
| B | API schema drift between manually-typed structs and `firecracker.yaml` | Round-trip property test: parse-then-serialize swagger examples and assert byte-equality |
| C | virtio-vsock TSI port from libkrun is gnarly | Keep TSI off by default; ship plain virtio-vsock first |
| E | `com.apple.vm.networking` denial blocks bridged users | Already mitigated by gvproxy fallback; document expectations early |
| F | `hv_vm_protect` TLB cost under high dirty rate | 2 MiB granularity default + adaptive heuristic |
| F | Mach exception port edge cases break LLDB | Save and forward to prior handlers; LLDB-attach test in CI |
| G | Notarytool stalls release | Notarize post-merge async, decoupled from tag |
| H | Boot time misses 400 ms | Hand-tuned kernel config + minimal initramfs in examples/ |

## What "done" looks like at 1.0

- Compat coverage ≥ 95% of upstream Firecracker `docs/api_requests/` examples passing unmodified.
- p50 boot to `/sbin/init` ≤ 400 ms on M2 Pro / M3, measured and published.
- Memory overhead ≤ 15 MiB at idle.
- Notarized `.pkg` and Homebrew formula available.
- `firectl`, `firecracker-go-sdk`, `firecracker-containerd` integration tests pass.
- At least one orchestrator's CI runs squib on macOS Apple Silicon.
- `docs/api-deviations.md` published, every deviation tested.
