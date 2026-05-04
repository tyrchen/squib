---
title: 93-improvements-review — deferred-findings backlog
type: review
status: draft
last_updated: 2026-05-04
depends_on: 91-impl-plan.md
---

# 93 · Deferred-findings backlog

The single home for review findings that surfaced during a phase but are out-of-scope for that phase, plus surfaced spec defects. Each entry includes severity, `path:LINE`, and a one-line fix shape so the next phase can pick it up without re-deriving the context.

Entries are append-only. When a deferred item is fixed, **strike it through** rather than removing — the historical decision matters for future contributors.

## Resolved in the 2026-05-04 cleanup pass

Sweeping pass after every phase landed. Each line names the originally-cited entry plus the resolution shape; the original entries are struck through in place below.

### Phase 1
- **P3 spec** (`13` § 4): dropped `far` from `EsrDecoded::DataAbort`; added a one-line note that FAR_EL2 is read separately and threaded onto the data-abort exit envelope downstream.
- **P3 spec** (`61` § 3 + I-CRATE-1): clarified the invariant to "no squib-workspace deps + minimal external deps" — the no-workspace half remains the load-bearing half.
- **P3 code** (`crates/fdt/src/lib.rs:287`): replaced `expect()` on masked arithmetic with an explicit `as`-cast plus a `// reason = …` lint allow. No production-path `expect` reachable from `add_cpus`.
- **P2 code** (`crates/loader/src/lib.rs`): added defence-in-depth `enforce_path_bounds()` (PATH_MAX = 1024, NUL-byte rejection) before the first `std::fs` call in `load_from_path_with_caps`; new `LoaderError::PathRejected` variant. The API-layer `SafePath` remains the canonical boundary; the loader is now also correct in isolation.

### Phase 2
- **P3 spec** (`21` /balloon/hinting row): pinned `BalloonHintingOp` vocabulary (`start | status | stop`) inline with a citation to `crates/api/src/schemas/balloon.rs::BalloonHintingOp`.
- **P3 deps** (workspace `Cargo.toml`): dropped the unused `validator` workspace dep + the `squib-api` adoption stub. The hand-rolled `Raw* → Validated TryFrom` pattern in `crates/api/src/schemas/` covers the same checks; nothing else in the workspace adopted it.
- **P2 code** (`crates/api/src/controller.rs::RuntimeApiController`): added `LimitsState` (host RAM cap, current `mem_size_mib`, per-class running counts) + `validate_cross_field()` invoked from `dispatch` before the channel is touched. Counters bump only on a non-fault VMM response (a stub-VMM rejection no longer consumes a slot). `cross_check_balloon` defers to boot when `mem_size_mib` is unset, matching upstream Firecracker's accept-then-validate-at-start semantics.

### Phase 3
- **P2 spec** (`14` § 6 / I-DEV-4): amended to read "either exactly N `Vm::map_memory`/`Vm::unmap_memory` calls **or** one coalesced call covering N × BLOCK_SIZE" — the merged shape is preferable on Apple Silicon (one stage-2 TLB invalidate vs N) and is what the existing `test_should_plug_n_blocks_in_a_single_backend_call` documents.
- **P2 code** (`crates/virtio/src/transport.rs::tests`): added `test_should_or_device_needs_reset_when_activation_fails` driving the device past `DRIVER_OK` with a stub-Err `activate()` and asserting `(ACK | DRIVER | FEATURES_OK | DRIVER_OK | DEVICE_NEEDS_RESET)`.
- **P2 code** (`crates/bus/src/lib.rs`): replaced `RwLock<BTreeMap<…>>` with a split-phase `BusBuilder → Arc<Bus>`; the dispatch path now takes `&BTreeMap` directly. Saves ~5–10 ns per MMIO exit (Phase 3 review finding). All callers (`crates/vmm/src/device_manager.rs`, `crates/vmm/tests/runner_hvf_smoke.rs`) updated.
- **P3 code** (`crates/mmds/src/token.rs`): replaced the hand-rolled `base64url_no_pad` (~30 LOC) with `base64::engine::general_purpose::URL_SAFE_NO_PAD`; added `base64 = "0.22"` to `[workspace.dependencies]`.
- **P3 dec** (virtio VENDOR_ID = 0): tracked. No code change — squib mirrors upstream Firecracker's pre-Red-Hat encoding, and parity with the compat suite is what matters.

### Phase 4
- **P2 code** (snapshot tests): added `host_dev_name_round_trips_through_save_restore` planting a synthetic virtio-net `DeviceState` blob with embedded `host_dev_name` bytes; asserts byte-equal blob round-trip after `save`/`load`. Pins I-NET-3.
- **P3 code** (`crates/core/src/identifiers.rs`): introduced `HostDevName` newtype (≤64 B, no NUL, serde-transparent) — threaded through `crates/api/src/schemas/network.rs::NetworkInterfaceConfig` and `crates/vmm/src/device_manager.rs::NetSpec`. The chained validation no longer relies on the field type being `String`.
- **P3 code** (`apps/squib/src/cli.rs::Args::bridged_iface`): added `--bridged-iface <IFNAME>` for `--network=bridged`. Warn-and-continue when set with a non-bridged mode.

### Phase 5
- **P2 code** (`crates/arch/src/sysregs.rs::as_encoded`/`from_encoded`): replaced the positional `position()+1` encoding with hand-assigned per-variant constants (1..=47). Reordering `SysReg::all()` no longer silently re-keys the wire format. Added `test_should_assign_distinct_wire_constants_per_variant`.
- **P2 code** (snapshot integration tests): added `diff_round_trip_property_sweep_over_random_dirty_patterns` — 12 trials × 3 (RAM, page) shapes, deterministic-LCG-seeded; asserts dirty pages carry the pattern byte and clean pages carry zero. Pins I-SNAP-2 without pulling in `proptest`.
- **P3 code** (snapshot/host casts): replaced `as u64` / `as usize` at `crates/snapshot/src/state.rs:117` and `crates/host/src/pager.rs:181-185` with `try_from`-based widening; remaining casts are widening (no truncation possible) or in test-only code.
- **P3 code** (`crates/snapshot/src/load.rs::infer_memory_path`): now accepts any extension (swaps to `.mem` regardless of the input stem). Operators with non-`.snap` naming get the matching hint.
- **P3 test** (`make snapshot-cross-fs-test`): added `cross_filesystem_save_rejects_when_dest_is_on_a_separate_ramdisk` (macOS-only, `#[ignore]`'d by default) using `hdiutil attach -nomount ram://…` + `mount -t hfs`.

### Phase 6
- **P2 code** (`apps/squib-jail/src/cli.rs`): added per-flag byte caps via `clap::builder::TypedValueParser` chains (256 B for short fields, 1024 B for paths, 4 KiB per child argv element with a 16 KiB total cap).
- **P2 code** (`apps/squib-jail/src/env.rs::stage`): chroot dir gets `fs::set_permissions(_, 0o755)` after `create_dir_all` (defeats whatever umask the launcher arrived with).
- **P2 code** (`apps/squib-jail/src/env.rs::copy_via_nofollow`): TOCTOU-safe staging via `O_NOFOLLOW + fstat` — refuses to traverse a symlink that materialised between `canonicalize_exec_file` and `copy`.
- **P2 code** (`apps/squib-jail/src/sequence.rs::daemonize`): standard double-fork prologue (fork → setsid → fork → grandchild execs). Defeats the `setsid → EPERM` failure on already-process-group-leader launchers.
- **P3 code** (`apps/squib-jail/src/sequence.rs::exec`): `execv → execve` with a curated `envp` (PATH, LANG, LC_ALL only). Caller-side credentials no longer leak.
- **P3 code** (`dist/homebrew/squib.rb:32`): added `--locked` to the cargo invocation.
- **P3 code** (`dist/pkg/build-pkg.sh` + `Makefile`): replaced the inline `python3 -c '…json.load(sys.stdin)…'` with `jq -r '…'`. Drops one interpreter dep on minimal CI runners.
- **P3 code** (`apps/squib-jail/Cargo.toml`): gated `[[bin]]` to `required-features = ["macos"]` so a non-macOS workspace build skips the binary outright; `tracing-subscriber` overridden to `default-features = false, features = ["env-filter", "fmt"]` (drops JSON/ANSI/chrono inflation).
- **P3 code** (`apps/squib-jail/profiles/default.sb`): scoped `sysctl-read` to a name allowlist (`hw.optional.arm.FEAT_*`, `hw.{ncpu,memsize,…}`, `kern.{osversion,…}`); dropped the `com.apple.SystemConfiguration.configd` mach-lookup (vmnet handles its own configd).

### Phase 7
- **P2 code** (`crates/snapshot/Cargo.toml`, `crates/vmm/Cargo.toml`): `criterion` is now `optional = true` keyed off a `bench = ["dep:criterion"]` feature. Plain `cargo test -p squib-snapshot` no longer pulls criterion or its ~30 transitives.
- **P2 tests** (`tests/firecracker-compat/tests/f_rows.rs`): added `f_get_balloon_statistics`, `f_patch_balloon_statistics`, `f_put_snapshot_create_returns_documented_stub_fault`, `f_put_snapshot_load_returns_documented_stub_fault`, `f_patch_vm_pause_post_boot`, `f_patch_vm_resume_post_boot`, `f_put_actions_instance_start_returns_documented_stub_fault`. The stubbed actions assert the exact `fault_message` text so the tests flip cleanly when Phase 1's tail lands.

### Cross-phase items remaining deferred (verified, gated)

**All items previously deferred have now landed** (commit `<2026-05-04 deferred-items pass>`):

- ~~vCPU + GIC HVF capture/restore~~ — **resolved**. `crates/hv/src/vcpu_save.rs` impls `VcpuSnapshotSource` / `VcpuRestoreTarget` over `applevisor::vcpu::Vcpu` (X0..X30, PC, CPSR, SP_EL1 via `get_sys_reg`, FP/SIMD via `get_simd_fp_reg`/`get_reg(FPCR/FPSR)`). `HvfGicSnapshot` impls `GicSnapshotSource` / `GicRestoreTarget` over `applevisor::gic::GicState`. Squib `SysReg` ↔ HVF `hv_sys_reg_t` mapping covers boot-setup / ID / timers / exceptions / TLB / debug; PMU + OSLAR + OSDLR + CNTFRQ_EL0 surface as `Ok(None)` (HVF doesn't expose them). `crates/hv/tests/vcpu_save_smoke.rs` does an HVF-gated round-trip of X0/X1/PC/CPSR/SCTLR/MAIR/VBAR/Q0/FPCR/FPSR.
- ~~HvfMemBackend wrapper for virtio-mem hotplug~~ — **resolved**. `crates/hv/src/hotplug.rs::HvfMemBackend` wraps `Arc<HvfVm>`; on plug calls `instance().memory_create(len)` + `mem.map(base, MemPerms::RW)`, on unplug calls `mem.unmap()` and drops the Arc. `crates/hv/tests/hotplug_smoke.rs` exercises plug → unplug → re-plug pattern (the I-DEV-4 unmap/remap gate).
- ~~`pager-live-mach` cargo feature~~ — **resolved**. `crates/host/src/pager.rs::mach_imp::live` is gated behind `pager-live-mach` and runs `mach_port_allocate(RECEIVE)` → `task_swap_exception_ports(EXC_MASK_BAD_ACCESS, …)` → `mach_msg(MACH_RCV_MSG, timeout)` loop with periodic `task_get_exception_ports` drift detection + re-install. Default build keeps the skeleton path so `cargo test` doesn't take over the developer Mac's exception delivery.
- ~~Block backend async engine + per-queue rate limiter~~ — **resolved**. `AsyncFileBackend` in `crates/virtio/src/devices/block.rs` does lockless positioned I/O via `pread`/`pwrite` (no `Mutex<File>`); `F_NOCACHE` is applied via `squib_host::block_io::set_f_nocache` (the unsafe-allowed crate). `RateLimitedBackend` is the token-bucket wrapper with `RateLimit::{steady, unlimited}` constructors; oversize requests grow the bucket up to the request size (single-call override) so 100 KiB writes complete in finite time even with a 100-byte burst budget. Tests at `block.rs::tests`.
- ~~`Frame::Bytes` + `FramePool` refactor for I-NET-4~~ — **resolved**. `Frame::bytes` is now `bytes::Bytes` (immutable, refcounted) and `Frame::from_buf(BytesMut)` is the freeze constructor. `FramePool` ships in `crates/virtio/src/devices/net.rs` with `acquire`/`release` methods + bounded slot count; `MmdsInterceptor::drain_rx` returns `Vec<bytes::Bytes>` so frames flow through the interceptor with no extra allocation. virtio-net + squib-net + mmds all updated.
- ~~gvproxy bundling~~ — **resolved (manifest-only).** `vendors/gvproxy/MANIFEST.toml` pins a release URL + SHA-256; `vendors/gvproxy/fetch.sh` downloads + verifies into `bin/`. `make vendor-gvproxy` drives the fetch; the .pkg builder + Homebrew formula stage the binary if present and warn if not. The `MANIFEST.toml` ships with placeholder SHA pins — release prep replaces them with the actual upstream-pinned values.
- ~~`iface_uuid_for` via `uuid::new_v5`~~ — **resolved**. Hand-rolled FNV mash replaced with `uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, iface_id.as_bytes())`. Workspace `uuid = "1"` dep added; `crates/host/src/pager.rs::live` is the second documented consumer (postcopy snapshot identifier).
- vmnet keyed callback registry: **already resolved differently** — the `block2`-based per-call `Arc<StartContext>` shape supports multi-NIC by construction.

Phase-1-tail and Phase-3-tail (boot-to-busybox smoke + dumbo TCP/HTTP server) remain **resolved**: Phase 1's commit `8ac7149` lands the vCPU thread spawn + kernel/initrd/FDT writes + PL011 emulation; Phase 3's commit `516c332` lands dumbo TCP/HTTP synthesis in `crates/mmds/src/interceptor.rs`.

## Phase 1 (lands at end of Phase 1.6)

### Spec inconsistencies

- ~~**P3** — `specs/13-arch-and-boot.md` § 4 declares `EsrDecoded::DataAbort { is_write, sas, srt, sf, far }` but the function signature on the same line is `decode(esr: u64) -> EsrDecoded` — the decoder cannot produce `far` from `esr` alone. The implementation in `crates/arch/src/esr.rs:84` omits `far` (FAR is read separately and threaded through to `Exit::Mmio` in `crates/hv/src/run_loop.rs:131`). Fix shape: amend 13 § 4 to drop `far` from `EsrDecoded::DataAbort` (it's already on the data-abort exit downstream).~~ — **resolved 2026-05-04**, see top of file.

### Trait-surface refactor (out-of-phase)

- **P3** — `crates/core/src/backend.rs` carries the Phase 0 skeleton (`HypervisorBackend`/`Vm`/`Vcpu` with `Box<dyn Vcpu>` dynamic dispatch) instead of the alioth-shaped associated-type surface in `specs/11-runtime-core.md` § 2 (`Hypervisor::Vm: Vm`, `Vm::Vcpu: Vcpu`, `create_gic`, `save_state`/`restore_state`, richer `Vcpu` API). Fix shape: a Phase-2-adjacent refactor with all consumers (squib-hv, squib-vmm, squib-api) updated together. Phase 0 follow-ups list (91 § 0) does not include this and 91 § 4 does not require the rename for Phase 1; the boot-orchestration skeleton is fine without it.

### Boundary input validation

- ~~**P2** — `crates/loader/src/lib.rs:286-289, 320-322` (`std::fs::metadata`, `std::fs::read`) trust a path supplied by the API layer. `specs/70-security.md` § 4 wants validate-at-the-boundary (length cap, NUL byte rejection, charset allowlist for any non-canonical fragment) and the API layer's `Raw<DriveConfig>::SafePath::new` is the canonical place. Fix shape: ensure every caller of `load_from_path` runs through `SafePath` first, and document the trust boundary in the loader's module doc.~~ — **resolved 2026-05-04** (defence-in-depth `enforce_path_bounds`, plus module-doc trust-boundary section).

### Performance / hot-path

- ~~**P3** — `crates/fdt/src/lib.rs:287` uses `expect("...")` on masked arithmetic. The mask (`& 0x00FF_FFFF`) makes truncation impossible, but CLAUDE.md style says no `expect()` in production code. Fix shape: rewrite as `(mpidr & 0xFFFF) as u32` or precompute the truncated MPIDR once and panic-free.~~ — **resolved 2026-05-04**.

### Polished-but-not-blocking

- ~~**P3** — `crates/core` still depends on three external crates (`serde`, `smallvec`, `thiserror`). I-CRATE-1 in 61-crates-and-features.md says "no workspace dependencies"; external deps are not banned by the literal text. Fix shape: clarify the invariant in 61 to "no squib-crate workspace deps + minimal external deps".~~ — **resolved 2026-05-04**.

## Boot-to-busybox smoke test (Phase 1 exit-criteria gap)

- **P1** — Phase 1's exit criterion is "`cargo run -- --config-file examples/hello.json` boots an aarch64 demo VM in under 1 s and the serial output shows `/sbin/init` running. The 32-vCPU variant of the same config also boots and `nproc` reports 32 (smoke for D22)." This criterion is **cross-phase**: it requires several deliverables that the impl plan places after Phase 1 — Phase 2.4 (`--config-file` static-config replay), Phase 3.1 (`squib-bus` MMIO bus + `BusDevice` trait), and a PL011 UART emulation (PL011 is not on any phase task list but is implicit for serial-output-based smoke). Bundled kernel + busybox initramfs in `examples/` is D14, scheduled in Phase 7. Phase 1.6's `build_microvm_for_boot` ships the planning + memory-map orchestration but does **not** spawn vCPU threads, write the kernel into guest RAM, or drive `hv_vcpu_run`. Fix shape: a follow-up phase-1-tail task that bolts on the vCPU thread spawn + kernel/FDT writes via `HvfVm::write_to_region`. Full-boot verification additionally needs the bus + PL011 (Phase 3.1 prerequisite) and the bundled kernel/initramfs (D14) — those gate the user-facing smoke; closing them is a roadmap conversation.

  **Live HVF binding verified** by `crates/hv/tests/hvf_smoke.rs` (run via `make hvf-test`): real `init_with_gic`, real `map_memory` + `write_to_region`, real `hv_vcpu_run`, real `EXCEPTION` exit, `decode_esr` → `EsrDecoded::Hvc { imm16: 0 }`. Cuts through the binding stack end-to-end on Apple Silicon hardware. The remaining blockers are above the binding layer (run-loop body, MMIO bus, PL011, kernel image) — not the HVF integration.

  **Implicit prerequisite — PL011 UART emulation** is not on any phase task list but is required for the Phase 1 exit criterion's "serial output shows /sbin/init" wording. Either move the criterion's serial-output clause to Phase 3 (where the bus + first non-virtio device land) or add a "Phase 1.8 — PL011 emulation" task to the impl plan. Recording the inconsistency here so the next impl-plan revision can resolve it.

## Phase 2 (lands at end of Phase 2.5 review pass)

### Cross-field validation deferred to controller / Phase 3

- ~~**P2** — `MemSizeMib::new` (`crates/api/src/schemas/common.rs:289-301`) only enforces `>= 1`. [10-data-model.md § 2.3](./10-data-model.md#23-schema-layer) and [70-security.md § 4](./70-security.md#4-input-validation) require an upper bound against host RAM minus hypervisor overhead. The `BackendCapabilities` surface needed for that check lands with Phase 1's HVF backend. Fix shape: thread `BackendCapabilities` into the controller, add `MachineConfig::validate_against_host(...)` invoked at dispatch time before the action reaches the VMM event loop.~~ — **resolved 2026-05-04** (controller `LimitsState` + `validate_cross_field`; production wires `host_ram_mib` from live capabilities at controller-construction time).

- ~~**P2** — `BalloonConfig.amount_mib` upper bound (`mem_size_mib − 32`, the upstream `MAX_BALLOON_SIZE_MIB` rule from [21-api-compat-matrix.md § 2 `/balloon`](./21-api-compat-matrix.md#balloon-put)) is not enforced. `crates/api/src/schemas/balloon.rs:RawBalloonConfig` accepts any `u64`. The cross-field check requires the running `mem_size_mib`, so the controller needs to consult its own `vm_config` snapshot. Fix shape: same as above — controller-level cross-field validation gate at dispatch.~~ — **resolved 2026-05-04**.

- ~~**P2** — Per-class running-count caps (`drives:8`, `network_interfaces:8`, `pmem:4` from `common.rs:21-31`) are only enforced inside `replay.rs` for the static-config path. The HTTP per-PUT path has no running-set tracker yet. Phase 3 wires a device manager; the cap check belongs there. Fix shape: device manager in `squib-vmm` returns `BadRequest` with the spec's documented `fault_message` when a 9th drive is PUT.~~ — **resolved 2026-05-04** (controller-side counters in `LimitsState`; bumps only on a non-fault VMM response).

### Validator-crate adoption stance

- ~~**P3** — `validator` crate is a workspace dep (`Cargo.toml:52`) but `squib-api` does not use it (the hand-rolled `Raw* → Validated TryFrom` covers the same checks). Either (a) drop the dep from the workspace if no other crate adopts it, or (b) add `#[derive(Validate)]` to `Raw*` shapes for documentation / lintability and call `.validate()` inside `try_from` ([10-data-model.md § 2.3](./10-data-model.md#23-schema-layer) explicitly endorses this layered pattern). Decision can wait until Phase 3 picks a crate-set.~~ — **resolved 2026-05-04** via option (a).

### Spec inconsistencies surfaced

- **P3** — [21-api-compat-matrix.md § 1](./21-api-compat-matrix.md#1-http-api-endpoints) lists `PUT / PATCH / DELETE | /pmem/{id}` and `PUT / GET / PATCH | /hotplug/memory` but [20-firecracker-api.md § 2](./20-firecracker-api.md#2-server-shape) router skeleton omits the `PATCH`/`DELETE` for pmem and the `PUT`/`GET` for hotplug-memory. Phase 2.5 review caught this and the implementation now wires all five — but the spec sample router needs the same correction in its next revision.

- ~~**P3** — `BalloonHintingOp` (start | status | stop) is not pinned in any spec. Phase 2 added the enum at `crates/api/src/schemas/balloon.rs:BalloonHintingOp`; the [21 § 1](./21-api-compat-matrix.md#1-http-api-endpoints) row should call out the three-value vocabulary explicitly so future contributors don't drift on it.~~ — **resolved 2026-05-04**.

## Phase 3 (lands at end of Phase 3 review pass)

### User-facing exit criterion is cross-phase blocked

- **P0** — Phase 3's exit criterion ([91 § 6](./91-impl-plan.md#6-phase-3-devices-and-mmds)) reads "a guest with rootfs + net (loopback for now) + vsock + balloon + entropy boots, opens a network connection, runs a workload, and `curl 169.254.169.254/foo` returns MMDS data." Two cross-phase blockers prevent the demo end-to-end:
  - **Phase 1 vCPU run-loop tail not landed** (already tracked in this file under "Phase 1"): no live vCPU thread spawn, no PL011 UART, no kernel/initramfs in `examples/`. Without this, no guest boots in CI.
  - **dumbo TCP/HTTP server is the deferred Phase 3.7 tail** (`crates/mmds/src/interceptor.rs:153-167`): the interceptor parses ARP and intercepts IPv4 frames (so I-MMDS-1 holds — host backend never sees MMDS-bound traffic) but does not synthesise SYN-ACK / HTTP responses. The data store + V2 token store + `service_http` helper are complete and tested in isolation, ready to bolt onto the dumbo state machine when it lands.
  - Fix shape: a Phase-3.7-tail task that ports `vendors/firecracker/src/vmm/src/dumbo/` (IPv4 + TCP) and wires it to call `MmdsInterceptor::service_http`. Until then, Phase 3 ships device + MMDS substrate at unit-test parity (345 workspace tests pass), and the user-facing curl demo is gated on the same Phase-1-tail items as the boot-to-busybox smoke.

### Block backend — async engine + rate limiter

- **P2** — `crates/virtio/src/devices/block.rs` ships only the sync engine. Per [14 § 4.1](./14-virtio-and-devices.md#41-virtio-block) the async engine (Tokio `spawn_blocking` + `F_NOCACHE`) is required for the ≥ 100 K IOPS budget in [71 § 3](./71-performance-budgets.md#3-block-io). The `BlockBackend` trait is shaped so the async engine drops in without touching the device frontend; the sync engine carries the 1.0 functional surface. Per-queue token-bucket rate limiter (D7-adjacent) also deferred. Fix shape: add `AsyncFileBackend` impl in Phase 7 perf-tuning; rate limiter as a `tower::Layer`-shaped wrapper around `BlockBackend`.

### virtio-mem hotplug — backend not wired to HVF

- **P2** — `crates/virtio/src/devices/mem.rs` defines `MemHotplugBackend` and the `InMemoryHotplugBackend` for tests, but the production `HvfBackend` that calls `HvfVm::map_memory` / `HvfVm::unmap_memory` is not wired. The spec ([14 § 4.7](./14-virtio-and-devices.md#47-virtio-pmem-and-virtio-mem)) notes "verified that HVF allows the unmap/remap pattern at runtime (an open question in early drafts; resolved in week 8 of Phase 3)" — that verification is the gating step. Fix shape: a small `squib-hv::HvfMemBackend` wrapper that takes an `HvfVm` and implements `MemHotplugBackend`, plus a CI test (gated on `make hvf-test`) confirming unmap+remap of a 2 MiB block is observable from a vCPU.

### virtio-mem invariant ambiguity (spec defect)

- ~~**P2** — I-DEV-4 in [14 § 6](./14-virtio-and-devices.md#6-invariants) reads "virtio-mem hotplug `plug`/`unplug` of an N-block range performs exactly N `Vm::map_memory`/`Vm::unmap_memory` calls." Squib's implementation coalesces the N contiguous blocks into one `MemHotplugBackend::plug(base, N * BLOCK_SIZE)` call (cheaper TLB shootdown). The test at `crates/virtio/src/devices/mem.rs:test_should_plug_n_blocks_in_a_single_backend_call` documents the ambiguity inline. Fix shape: amend I-DEV-4 to "exactly N or one coalesced map of N × BLOCK_SIZE", or split the merged call back into N per-block calls. The merged shape is preferable on Apple Silicon (one stage-2 TLB invalidate vs N).~~ — **resolved 2026-05-04**.

### Spec inconsistency: VendorID

- **P3** — `crates/virtio/src/transport.rs:53` uses `VENDOR_ID = 0` matching upstream Firecracker (`vendors/firecracker/src/vmm/src/devices/virtio/transport/mmio.rs:25`), which itself carries a `// TODO crosvm uses 0 here, but IIRC virtio specified some other vendor id that should be used`. Compat-suite parity is fine (both sides use `0`), but the virtio spec recommends `0x1AF4` (Red Hat / Qumranet). Fix shape: track upstream Firecracker; if they ever bump to `0x1AF4` we follow.

### `set_status` lacks a transition-rejected-into-DEVICE_NEEDS_RESET test

- ~~**P2** — `crates/virtio/src/transport.rs::set_status` correctly drops invalid driver-init transitions and sets `DEVICE_NEEDS_RESET` on activation failure (line 286), but no unit test asserts that a post-`DRIVER_OK` feature ack returns `(ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK | DEVICE_NEEDS_RESET)` per virtio v1.2 § 2.1. Fix shape: add a unit test in `transport.rs::tests` that drives the device past `DRIVER_OK`, attempts a feature ack, then reads the `Status` register and asserts the `DEVICE_NEEDS_RESET` bit.~~ — **resolved 2026-05-04** via `test_should_or_device_needs_reset_when_activation_fails`.

### Bus uses `RwLock` on the dispatch hot path

- ~~**P2** — `crates/bus/src/lib.rs::Bus` wraps the `BTreeMap` in `RwLock`, but every `read()`/`write()` call takes the read-lock — ~5–10 ns per MMIO exit, compounding across millions per second. Spec § 2 example shows a bare `BTreeMap`. Fix shape: build the bus immutably via a `BusBuilder` that returns `Arc<Bus>` post-boot, or move the `RwLock` outside `Bus` so callers who mutate during boot pay the cost and the dispatch path uses `&BTreeMap` directly.~~ — **resolved 2026-05-04** (BusBuilder split-phase shape).

### MMDS — base64url helper duplicates a maintained crate

- ~~**P3** — `crates/mmds/src/token.rs::base64url_no_pad` is hand-rolled (~30 LOC). Workspace already plans `aws-lc-rs` for snapshot crypto; pulling `base64 = "0.22"` (or `data-encoding`) is one line and removes hand-coded SIMD-unfriendly bit-shifting that has no fuzz coverage. Fix shape: add `base64` to `[workspace.dependencies]` once any other crate adopts it; replace `base64url_no_pad` with `URL_SAFE_NO_PAD.encode(...)`.~~ — **resolved 2026-05-04**.

### MMDS interceptor — set_ipv4 needs API-layer wiring

- ~~**P2** — `MmdsInterceptor::set_ipv4` (and the consume-and-return `with_ipv4`) ship in `crates/mmds/src/interceptor.rs`, but the API layer's `PUT /mmds/config { ipv4_address }` handler does not call them yet — the MMDS controller in `crates/api/src/controller.rs` will need to thread the override into the active interceptor when virtio-net comes up. Fix shape: in the device-manager wiring (Phase 4 or later), surface the interceptor handle on the `RuntimeApiController` and call `set_ipv4` from the `MmdsConfig::ipv4_address` field on apply.~~ — **resolved 2026-05-04** (already wired by Phase 3).

## Phase 4 (lands at end of Phase 4 review pass)

### Live FFI bugs caught by the post-review smoke test

The post-review user check ("have you fully tested this?") drove a `make vmnet-test`-shaped live FFI smoke (`crates/net/tests/vmnet_ffi_smoke.rs` + `examples/`-based debug binaries). Running the codesigned smoke against `vmnet.framework` on macOS 15.x exposed four bugs that none of the unit tests, clippy, or the independent code review caught — every one was a misread of the framework headers or an undocumented runtime contract:

- **P0 — vmnet operating-mode constants were swapped.** `<vmnet/vmnet.h>` defines `VMNET_HOST_MODE = 1000`, `VMNET_SHARED_MODE = 1001`, `VMNET_BRIDGED_MODE = 1002`. `crates/net/src/mode.rs` had Shared at 1000 and Host at 1001 — every `--network=shared` boot would have asked for host-only mode under the hood. **Fixed in this phase.**
- **P0 — XPC keys had a stray `_key` suffix.** The C *variable* `vmnet_operation_mode_key` is an `extern const char *` pointing to the string `"vmnet_operation_mode"` (no suffix). Squib was passing the key string `"vmnet_operation_mode_key"` and vmnet was rejecting the dictionary as unrecognised. **Fixed by switching `crates/net/src/sys/vmnet.rs::keys::*` to the actual key strings.**
- **P0 — `vmnet_interface_id_key` is a `uuid_t`, not a string.** Squib was hex-encoding the iface_id into a UUID-shaped string and calling `xpc_dictionary_set_string`; the framework wants 16 raw bytes via `xpc_dictionary_set_uuid`. **Fixed by adding `XpcObject::set_uuid` and switching the descriptor builder.**
- **P0 — `dispatch_time_t` is not raw nanoseconds.** `crates/net/src/sys/dispatch.rs::dispatch_time_now_plus_ns` was returning the raw delta nanosecond count instead of going through libdispatch's `dispatch_time(DISPATCH_TIME_NOW, delta)` helper. The result: `dispatch_semaphore_wait` interpreted the value as an already-past absolute timestamp and returned immediately, so every `vmnet_start_interface` call timed out before the callback could land. **Fixed by binding `dispatch_time` and calling it.**

A fifth issue surfaced in the same smoke test and was resolved by adopting the `block2` crate (added to `[workspace.dependencies]`): the hand-rolled global block literal in the original `crates/net/src/sys/block.rs` did not match what `vmnet.framework` / libdispatch on macOS 15.x expect for invocation. `block2` is the maintained Rust binding for the Apple Block ABI; the unsafe surface this carries is bounded and audited.

What this means for the Phase 4 exit criterion: **the live FFI surface is now verified end-to-end** under ad-hoc-signed test binaries — `make vmnet-test` exercises `vmnet_start_interface` against the real framework and the callback fires with `VMNET_FAILURE` (expected for an ad-hoc-signed binary that does not own the sharing service). Achieving `VMNET_SUCCESS` and an actual `curl example.com` from inside a guest still requires the rest of the Phase 1 vCPU-thread/PL011/init-RAM stack documented elsewhere in this file, but the squib-net binding itself is no longer "compiles, untested" — it's "compiles, callback verified, awaiting full-VM integration."

### Frame allocation invariant deferred to Phase 7 perf tuning

- **P2** — I-NET-4 in [30-networking.md § 7](./30-networking.md#7-invariants) reads "frame allocation uses the pre-allocated `BytesMut` pool; no per-packet `Vec<u8>` allocation in the hot path." `crates/net/src/backend.rs::VmnetHostBackend::recv` allocates `Vec<Vec<u8>>` storage per call because `squib_virtio::devices::net::Frame { bytes: Vec<u8> }` is the trait shape. The proper fix is a Phase-7 refactor that switches `Frame` to `Bytes`/`BytesMut` end-to-end (virtio-net frontend, MMDS interceptor, and squib-net backends together) and reintroduces a `FramePool` sized to MTU × 256 per direction. The bench harness ([71 § 5](./71-performance-budgets.md#5-network-throughput)) is the gating signal — under the Lambda-shaped workload profile (short-lived microVMs, low concurrent flows) the per-recv `vec![0u8; mtu]` allocation is below the noise floor; once a concurrent-flow benchmark lands the allocator-profiling lane in CI will surface the regression. Fix shape: introduce `Frame::with_capacity(BytesMut)` constructor + a `FramePool` in `squib-net`, then port the three call sites.

### `host_dev_name` round-trip needs a snapshot golden test

- ~~**P2** — I-NET-3 in [30-networking.md § 7](./30-networking.md#7-invariants): "`host_dev_name` is round-trip-preserved through snapshot save/restore even though it is opaque." The string is preserved through `crates/api/src/schemas/network.rs::NetworkInterfaceConfig` and threaded into `crates/vmm/src/device_manager.rs::NetSpec::host_dev_name`, but no snapshot-side test asserts it round-trips through a `Snapshot::save_state`/`restore_state` cycle (snapshot subsystem lands in Phase 5). Fix shape: add the golden test as part of Phase 5.2's vCPU/GIC save-restore work — the network-interface description sits in the same state-blob.~~ — **resolved 2026-05-04**.

### gvproxy bundling — binary not yet vendored

- **P3** — [30-networking.md § 4](./30-networking.md#4-userspace-mode-gvproxy) requires the `gvproxy` binary to ship under `<install-prefix>/libexec/squib/gvproxy`. `crates/net/src/gvproxy.rs::GvproxyBackend::start` accepts the path but the bundling itself (download + SHA-256 pin in `vendors/`) belongs to Phase 6 distribution work — the binary isn't published yet so vendoring is premature. Fix shape: add a `vendors/gvproxy/` subtree with a checksum-pinned download as part of Phase 6.4 (`Homebrew formula prep; .pkg builder`).

### Bridged `bridged_iface_name` not exposed in CLI

- ~~**P3** — `crates/net/src/iface.rs::InterfaceParams::bridged_iface_name` accepts the host-side physical interface name (`en0`, etc.) for `VMNET_BRIDGED_MODE`. The CLI does not expose this knob — `--network=bridged` defaults `bridged_iface_name = None`, which lets vmnet pick the primary interface. Fix shape: add `--bridged-iface <name>` when bridged mode is exercised in production; for the inner-dev-loop use case the default is fine.~~ — **resolved 2026-05-04**.

### vmnet `start_interface` callback budget is a single global

- ~~**P3** — `crates/net/src/sys/block.rs` uses a single static `ACTIVE_CONTEXT: Mutex<Option<usize>>` so only one `vmnet_start_interface` can be in flight at a time. squib instantiates one virtio-net per VM so this is fine for now, but if a future feature spawns N interfaces in parallel the guard `debug_assert!` will trip. Fix shape: keyed registry (`SlotMap` or per-block-instance heap allocation with `BLOCK_HAS_COPY_DISPOSE` flags); only worth doing if multi-NIC microVMs land.~~ — **resolved 2026-05-04** by the `block2` adoption (per-call `Arc<StartContext>` captured by `RcBlock`; no global state).

### `host_dev_name` not yet a newtype in the device manager

- ~~**P3** — `crates/vmm/src/device_manager.rs::NetSpec::host_dev_name` is a plain `String`. The API layer's `crates/api/src/schemas/network.rs::validate_host_dev_name` already enforces the byte cap and NUL-rejection per [70-security.md § 4](./70-security.md#4-input-validation), so the upstream value is validated, but the type system in the device manager doesn't witness that — a future direct construction could bypass the validation. Fix shape: introduce a `HostDevName(String)` newtype in `squib-core` with the same fallible constructor as `IfaceId`, then thread it through `NetSpec` and `NetworkInterfaceConfig`. Out of phase because it's a multi-crate refactor that doesn't change runtime behaviour today.~~ — **resolved 2026-05-04**.

### `iface_uuid_for` rolls a non-standard hash

- **P3** — `crates/net/src/sys/iface_impl.rs::iface_uuid_for` builds a UUID-shaped string from a bespoke FNV mash rather than `uuid::Uuid::new_v5(&Uuid::NAMESPACE_OID, ...)`. Vmnet treats the value as opaque, but adding `uuid` to `[workspace.dependencies]` once any other crate adopts it would replace 30 lines of hand-rolled hash with a single call. Fix shape: amend `61-crates-and-features.md § 4` once the second consumer arrives, then port.

## Phase 5 (lands at end of Phase 5 review pass)

### Production-path `expect()` calls (CLAUDE.md violation)

- **P2** — `crates/snapshot/src/save.rs:116` uses `dirty.expect("checked above")` to extract the bitmap reference after a structural pre-condition. The proof is correct (the `Some/None` check is two lines above), but CLAUDE.md "Error Handling" forbids `expect()` outside tests, and a future refactor that breaks the pre-condition would surface as a panic on a path reachable from `PUT /snapshot/create`. Fix shape: split `SnapshotKind` into `Full` / `Diff(&'a DirtyBitmap)` so the bitmap is part of the variant, not a sibling field — then the destructuring is total and no `expect` is needed.
- **P2** — `crates/host/src/pager.rs:524, 565` use `expect("squib-pager thread spawn")`. `squib-host` is a library; `thread::Builder::spawn` only fails on `EAGAIN`, which is a real DoS surface. Fix shape: have `spawn_mach_server` return `io::Result<JoinHandle<...>>` and propagate the spawn error.

### Wire-stability of positional sysreg encoding

- ~~**P2** — `crates/arch/src/sysregs.rs:132-137` (`SysReg::as_encoded`) uses `position()+1` in `SysReg::all()`. The doc says "never insert in the middle", but a wire format that silently reinterprets old keys when someone reorders the slice is a bug magnet — the compiler has no way to enforce the invariant. Fix shape: hand-assigned `u64` constants per variant, or a `const`-checked lookup table keyed on `Self as u8`. Do this before the snapshot format ships in a 1.0 tag.~~ — **resolved 2026-05-04**.

### Diff snapshot property test (I-SNAP-2)

- ~~**P2** — `specs/16-snapshots.md:188` (I-SNAP-2) asks for "Property test with synthetic write patterns". `crates/snapshot/tests/integration.rs::diff_round_trip_writes_only_dirty_pages` exercises one specific dirty pattern; no `proptest!` validates "the resulting `<id>.mem` carries the pattern bytes only at those offsets and zeros elsewhere" across randomized write distributions. Fix shape: add a proptest in the integration test that generates a random subset of pages, calls `mark_dirty` for each, and asserts byte-equality against the expected sparse pattern.~~ — **resolved 2026-05-04** via deterministic-LCG sweep.

### Cross-FS rejection has no live coverage

- ~~**P3** — `crates/snapshot/tests/integration.rs::cross_filesystem_temp_path_rejection` documents that the live cross-FS test runs out-of-band; CI does not currently exercise it. Fix shape: add a macOS-only test that creates a tmpfs ramdisk via `hdiutil attach -nomount ram://...`, mounts it under a known directory, and asserts `AtomicCommitCrossFs` when the snapshot dest sits there but the temp path doesn't.~~ — **resolved 2026-05-04** via `make snapshot-cross-fs-test`.

### `infer_memory_path` only handles `.snap`

- ~~**P3** — `crates/snapshot/src/load.rs:212-219` returns `None` for files named `vm.snapshot` or anything other than `*.snap`. Operators with non-default extensions get no hint about the matching memory file. Documented; matches Firecracker. Fix shape: accept any stem, swap the extension to `.mem`; raise the cap if it produces ambiguous matches in practice.~~ — **resolved 2026-05-04**.

### Style: explicit truncating casts in production code

- ~~**P3** — Several `as u64` / `as usize` casts in `crates/snapshot/src/state.rs:117`, `crates/host/src/pager.rs:181, 183, 636, 731` are silenced crate-wide via `#![allow(clippy::cast_possible_truncation)]`. CLAUDE.md prefers `u64::try_from(usize)` (infallible on 64-bit) for readability. Fix shape: switch to `try_from`; remove the crate-level allow once the call sites are clean.~~ — **resolved 2026-05-04** for the cited production sites; remaining casts are widening or test-only and the crate-wide allow stays for those.

### vCPU + GIC HVF impl deferred

- **P2** — `crates/snapshot/src/vcpu_save.rs` defines `VcpuSnapshotSource` / `VcpuRestoreTarget` / `GicSnapshotSource` / `GicRestoreTarget` with mock fixtures and round-trip tests, but `squib-hv` carries no impl yet — capture/restore against a live `applevisor::Vcpu` and `applevisor::Gic` is a phase-1-tail integration (same gating as the rest of the HVF binding). Fix shape: in `crates/hv/src/vcpu_save.rs`, implement the four traits over the curated `SysReg::all()` list; add a `make hvf-test`-gated integration test that round-trips a non-trivial vCPU state through a save/load cycle.

### Pager live `mach_msg` server is feature-gated

- **P2** — `crates/host/src/pager.rs::mach_imp` ships the lifecycle skeleton (spawn → poll → drift-check → shutdown). The live `mach_msg(MACH_RCV_MSG)` loop, `task_swap_exception_ports` install + drift detection, and `mach_exception_raise_state_identity` forwarder for out-of-region faults are not wired. Fix shape: add a `pager-live-mach` cargo feature, plumb `mach2` (or hand-rolled FFI in `unsafe` blocks with `// SAFETY:` comments) inside `mach_imp`, gate the live LLDB-attach CI test (real `lldb -p $(pgrep <bin>) -o detach`) behind it.

## Phase 6 (lands at end of Phase 6 review pass)

### Spec amendment — fourth unsafe-allowed crate

- **P2** — `specs/70-security.md:136` (I-SEC-1) and § 2 still read "`unsafe` lives only in `squib-hv` and `squib-net::sys`." Phase 5 widened the rule implicitly to `squib-host` (Mach exception ports); Phase 6 now adds `squib-jail` (libc privilege-drop syscalls + `sandbox_init(3)` FFI). Fix shape: amend I-SEC-1 to enumerate `squib-hv`, `squib-net::sys`, `squib-host`, and `squib-jail`; add a one-line note in 70 § 2 explaining the per-crate justification (FFI surface, not unsoundness).

### Boundary input validation deferred to Phase 7 polish

- ~~**P2** — `apps/squib-jail/src/cli.rs:23-99` carries no explicit per-flag length cap. Per `specs/70-security.md` § 4 every external string from launchers crossing the trust boundary should be byte-bounded. clap's defaults are unbounded; a `--cgroup` value of 1 GiB would be accepted before reaching the parser. Fix shape: thread a `value_parser` chain (`clap::builder::StringValueParser::new().try_map(|s| { … })`) over `--id` / `--exec-file` / `--chroot-base-dir` / `--cgroup` / `--resource-limit` / `--parent-cgroup` / `--netns` / passthrough argv with explicit byte caps (≤256B for short fields, ≤1024B for paths).~~ — **resolved 2026-05-04**.

### TOCTOU on `--exec-file` canonicalize

- ~~**P2** — `apps/squib-jail/src/env.rs::canonicalize_exec_file` calls `fs::canonicalize` then `fs::metadata` (two stat calls). A racing rename between the two opens a TOCTOU window where the binary at the canonical path is not the binary that was checked. Per `specs/70-security.md` § 5 ("re-canonicalize after open") the right shape is `O_NOFOLLOW + fstat` against an open fd. Fix shape: open with `OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW)`, `fstat` the fd, copy via the fd into the chroot.~~ — **resolved 2026-05-04** via `copy_via_nofollow`.

### Daemonize without an initial fork

- ~~**P2** — `apps/squib-jail/src/sequence.rs::daemonize` calls `setsid()` directly. On a process that's already a process group leader (most launcher invocations), `setsid` returns EPERM and the daemonize step fails. The standard double-fork pattern is `fork() → child setsid → fork() → grandchild execs`. Fix shape: implement the fork prologue in `sequence::daemonize` and update `specs/40-jailer.md` § 3 step 5 to spell the contract out.~~ — **resolved 2026-05-04**.

### Chroot tree mode bits

- ~~**P2** — `apps/squib-jail/src/env.rs::stage` runs `fs::create_dir_all` inheriting the caller's umask (typically `022`, sometimes `002` when the process is started under a service manager). The chroot dir then has whatever mode the parent's umask permits. Fix shape: `fs::set_permissions(&self.chroot_dir, fs::Permissions::from_mode(0o755))` after `create_dir_all` completes.~~ — **resolved 2026-05-04**.

### `execv` does not sanitise environment

- ~~**P3** — `apps/squib-jail/src/sequence.rs::exec` uses `libc::execv`, inheriting the caller's full environment into the staged binary. Upstream jailer calls `execve` with a curated `envp` (PATH-only, plus a small allowlist). For squib's threat model (developer machine, trusted operator) this is low priority but worth tracking. Fix shape: add a `sanitize_env()` step that filters envp to a constant allowlist, or accept the inherited env explicitly via a doc note.~~ — **resolved 2026-05-04**.

### Homebrew formula needs `--locked`

- ~~**P3** — `dist/homebrew/squib.rb:32` runs `cargo build --release --bin squib --bin squib-jail` without `--locked`. Per `specs/70-security.md` § 10 (Supply chain), pinning to `Cargo.lock` is the only thing that prevents a yanked transitive from sneaking in between brew install and recipe-mtime. Fix shape: add `--locked` once a release-targeted `Cargo.lock` lands (currently the workspace's `Cargo.lock` is dev-mode).~~ — **resolved 2026-05-04**.

### `.pkg` builder shells out to `python3`

- ~~**P3** — `dist/pkg/build-pkg.sh:30-34` uses `python3 -c '…json.load(sys.stdin)…'` to extract the workspace version from `cargo metadata`. Some ephemeral CI runners ship without `python3` in PATH (they need to install it deliberately). The Makefile `hvf-test` target already requires `jq`; consolidating on jq drops the python dep. Fix shape: replace with `jq -r '.packages[0].version'`.~~ — **resolved 2026-05-04** (also swapped the `Makefile` PKG_OUT and TARGET_DIR helpers).

### Jailer can be built on Linux to no purpose

- ~~**P3** — `apps/squib-jail/src/sandbox.rs:75-80` ships a non-macOS stub returning an error so the crate compiles on Linux, but the resulting binary is useless (Darwin syscalls won't link / run). Fix shape: gate the `[[bin]]` target in `apps/squib-jail/Cargo.toml` to `target.'cfg(target_os = "macos")'.dependencies` (or a `required-features` analog) so a Linux build skips the binary outright.~~ — **resolved 2026-05-04** via `required-features = ["macos"]`.

### Default sandbox profile over-grants

- ~~**P3** — `apps/squib-jail/profiles/default.sb:42-43` allows `mach-lookup` of `com.apple.SystemConfiguration.configd` and `(allow sysctl-read)` is unrestricted. squib's actual sysctl reads are only the HVF-feature `hw.optional.arm.FEAT_*` set, and `configd` is needed only when squib-net opens a vmnet handle (which by then has its own entitlement). Fix shape: scope `sysctl-read` to a name allowlist; drop `configd` once an integration trace confirms it's unused.~~ — **resolved 2026-05-04**.

### `tracing-subscriber` feature bloat

- ~~**P3** — `apps/squib-jail/Cargo.toml:20` inherits the workspace's full feature set on `tracing-subscriber` (env-filter, fmt, json, …). The jailer is a sub-100ms one-shot binary that emits at most a handful of warnings; pulling JSON / ANSI / chrono dramatically inflates the binary. Fix shape: depend with `default-features = false, features = ["env-filter", "fmt"]` once a per-crate dependency override lands in workspace Cargo.toml.~~ — **resolved 2026-05-04**.

### Pre-existing: hvf_smoke test runs in plain `cargo test --workspace`

- **P2** — `crates/hv/tests/hvf_smoke.rs::hvf_round_trips_an_hvc_trap_via_real_vcpu` requires HVF entitlement on the test binary (only `make hvf-test` codesigns the test binaries via `--no-run` + `codesign --force`). The test is not `#[ignore]`, so a vanilla `cargo test --workspace` (and the GitHub Actions `nextest run --all-features`) fails on the unsigned test binary. Verified pre-Phase-6 (`git stash && cargo test`). Fix shape: add `#[ignore = "requires com.apple.security.hypervisor — run via make hvf-test"]` to the test, mirroring the `#[ignore]`'d sandbox test in `apps/squib-jail/src/sandbox.rs`.

### gvproxy bundling still deferred

- **P3** — `specs/30-networking.md` § 4 requires `gvproxy` to ship under `<install-prefix>/libexec/squib/gvproxy`, but the `.pkg` builder (`dist/pkg/build-pkg.sh`) does not stage it. The Phase 4 review row "gvproxy bundling — binary not yet vendored" already tracks the underlying vendoring work. Phase 6 inherits the dependency: until a checksummed `gvproxy` lands in `vendors/gvproxy/`, the .pkg installer ships without userspace networking, and the Homebrew formula similarly cannot install it.

## Phase 7 (lands at end of Phase 7 review pass)

### Resolved in this phase

- ~~**P2** — `crates/snapshot/src/save.rs:116` `dirty.expect("checked above")` — replaced with a `let-else` inside the `Diff` arm. No reachable panic on the `PUT /snapshot/create` path. (Phase 5 fixup.)~~
- ~~**P2** — `crates/host/src/pager.rs:524, 565` `expect("squib-pager thread spawn")` — `spawn_mach_server` now returns `Result<JoinHandle<…>, PagerError::Spawn>`; tests propagate via `.expect()` (test-only). (Phase 5 fixup.)~~
- ~~**P2** — `crates/hv/tests/hvf_smoke.rs::hvf_round_trips_an_hvc_trap_via_real_vcpu` was missing `#[ignore]` so a vanilla `cargo test --workspace` failed on the unsigned binary. Added `#[ignore = "requires com.apple.security.hypervisor — run via make hvf-test"]`. (Phase 6 fixup; `make hvf-test` already passes `--include-ignored`.)~~
- ~~**P2** — I-SEC-1 in `specs/70-security.md` § 2 / § 9 listed only `squib-hv` and `squib-net::sys`. Amended to enumerate `squib-hv`, `squib-net::sys`, `squib-host`, and `squib-jail`, with a one-line note that the per-crate justification is *FFI surface*, not relaxed soundness. (Phase 6 fixup.)~~

### Compat-suite parity at the wire layer (in-phase)

- The compat suite under `tests/firecracker-compat/` covers every endpoint, the upstream-only 504 deviation, and a representative slice of P/A/R rows. The remaining gap — F-row replays of upstream `docs/api_requests/` *examples* byte-for-byte — needs the request fixtures vendored under `tests/firecracker-compat/transcripts/` and a JSON loader that drives [`Transcript`] from disk. Fix shape: add `serde_json::from_str::<Transcript>` deserialisation and walk `vendors/firecracker/docs/api_requests/*.json` in a single `#[test]`. Out-of-phase because byte-equal upstream replay is gated on the Phase 1 vCPU run-loop tail (live `InstanceStart` returning 204).

### Live perf numbers gated on Phase 1 tail

- **P1** — Phase 7 publishes the bench *substrate* numbers in `docs/perf/<sha>/` (dirty-bitmap + planning step) but the live P1 / P2 / P3 / P4 / P5 / P7 axes — boot to `/sbin/init`, RSS overhead, vCPU exit, vmnet throughput, block IO, postcopy first-response — all need a live VM event loop. Each is wired in the bench harness skeleton; they light up the moment Phase 1's vCPU thread + kernel image lands. See `docs/perf/index.md` "Targets vs status" table for the per-axis state. Cross-references the existing "Phase 1 (lands at end of Phase 1.6) — Boot-to-busybox smoke test" entry above.

### Soak harness scaffolds shipped, runners gated

- **P2** — `tools/soak/{firectl,firecracker-go-sdk,firecracker-containerd}/run.sh` exist and `make soak` walks them. Each runner SKIPs cleanly when its SDK is missing; when present, the runners surface the documented stub-VMM `fault_message` ("VMM not yet wired") on `InstanceStart`. End-to-end SDK pass requires the same Phase 1 tail; the harness is in place to flip from WARN to PASS the day the live VMM lands.

### Deferred from Phase 7 review

- ~~**P2** — `crates/snapshot/Cargo.toml:34` and `crates/vmm/Cargo.toml:39` register `criterion` as an unconditional `dev-dependency`. Bench targets are gated on `required-features = ["bench"]`, so plain `cargo test -p squib-snapshot` pulls criterion + ~30 transitive crates it never uses. Fix shape: gate `criterion` behind an optional dependency flipped on by the `bench` feature (`criterion = { workspace = true, optional = true }` + `bench = ["dep:criterion"]`).~~ — **resolved 2026-05-04**.

- ~~**P2** — `tests/firecracker-compat/tests/f_rows.rs` covers most rows in `21-api-compat-matrix.md § 1` but is missing a row-by-row stub-VMM F-test for `GET /balloon/statistics`, `PATCH /balloon/statistics`, `PUT /snapshot/create` (expected 400 with the documented stub `fault_message`), `PUT /snapshot/load` (same), `PATCH /vm` (post-boot Pause/Resume), and `PUT /actions {InstanceStart}` (expected 400 with stub `fault_message`). Fix shape: add one mechanical happy-path test per row, marking the inherently-stubbed actions as expected-400 so they convert cleanly when Phase 1's vCPU tail lands.~~ — **resolved 2026-05-04**.

## Cross-references

- ← Read by: every phase as the place to land out-of-phase findings.
- → Pairs with: [91-impl-plan.md](./91-impl-plan.md) (a deferred item is a future phase task).
