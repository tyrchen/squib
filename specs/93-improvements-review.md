---
title: 93-improvements-review — deferred-findings backlog
type: review
status: draft
last_updated: 2026-05-03
depends_on: 91-impl-plan.md
---

# 93 · Deferred-findings backlog

The single home for review findings that surfaced during a phase but are out-of-scope for that phase, plus surfaced spec defects. Each entry includes severity, `path:LINE`, and a one-line fix shape so the next phase can pick it up without re-deriving the context.

Entries are append-only. When a deferred item is fixed, **strike it through** rather than removing — the historical decision matters for future contributors.

## Phase 1 (lands at end of Phase 1.6)

### Spec inconsistencies

- **P3** — `specs/13-arch-and-boot.md` § 4 declares `EsrDecoded::DataAbort { is_write, sas, srt, sf, far }` but the function signature on the same line is `decode(esr: u64) -> EsrDecoded` — the decoder cannot produce `far` from `esr` alone. The implementation in `crates/arch/src/esr.rs:84` omits `far` (FAR is read separately and threaded through to `Exit::Mmio` in `crates/hv/src/run_loop.rs:131`). Fix shape: amend 13 § 4 to drop `far` from `EsrDecoded::DataAbort` (it's already on the data-abort exit downstream).

### Trait-surface refactor (out-of-phase)

- **P3** — `crates/core/src/backend.rs` carries the Phase 0 skeleton (`HypervisorBackend`/`Vm`/`Vcpu` with `Box<dyn Vcpu>` dynamic dispatch) instead of the alioth-shaped associated-type surface in `specs/11-runtime-core.md` § 2 (`Hypervisor::Vm: Vm`, `Vm::Vcpu: Vcpu`, `create_gic`, `save_state`/`restore_state`, richer `Vcpu` API). Fix shape: a Phase-2-adjacent refactor with all consumers (squib-hv, squib-vmm, squib-api) updated together. Phase 0 follow-ups list (91 § 0) does not include this and 91 § 4 does not require the rename for Phase 1; the boot-orchestration skeleton is fine without it.

### Boundary input validation

- **P2** — `crates/loader/src/lib.rs:286-289, 320-322` (`std::fs::metadata`, `std::fs::read`) trust a path supplied by the API layer. `specs/70-security.md` § 4 wants validate-at-the-boundary (length cap, NUL byte rejection, charset allowlist for any non-canonical fragment) and the API layer's `Raw<DriveConfig>::SafePath::new` is the canonical place. Fix shape: ensure every caller of `load_from_path` runs through `SafePath` first, and document the trust boundary in the loader's module doc.

### Performance / hot-path

- **P3** — `crates/fdt/src/lib.rs:287` uses `expect("...")` on masked arithmetic. The mask (`& 0x00FF_FFFF`) makes truncation impossible, but CLAUDE.md style says no `expect()` in production code. Fix shape: rewrite as `(mpidr & 0xFFFF) as u32` or precompute the truncated MPIDR once and panic-free.

### Polished-but-not-blocking

- **P3** — `crates/core` still depends on three external crates (`serde`, `smallvec`, `thiserror`). I-CRATE-1 in 61-crates-and-features.md says "no workspace dependencies"; external deps are not banned by the literal text. Fix shape: clarify the invariant in 61 to "no squib-crate workspace deps + minimal external deps".

## Boot-to-busybox smoke test (Phase 1 exit-criteria gap)

- **P1** — Phase 1's exit criterion is "`cargo run -- --config-file examples/hello.json` boots an aarch64 demo VM in under 1 s and the serial output shows `/sbin/init` running. The 32-vCPU variant of the same config also boots and `nproc` reports 32 (smoke for D22)." This criterion is **cross-phase**: it requires several deliverables that the impl plan places after Phase 1 — Phase 2.4 (`--config-file` static-config replay), Phase 3.1 (`squib-bus` MMIO bus + `BusDevice` trait), and a PL011 UART emulation (PL011 is not on any phase task list but is implicit for serial-output-based smoke). Bundled kernel + busybox initramfs in `examples/` is D14, scheduled in Phase 7. Phase 1.6's `build_microvm_for_boot` ships the planning + memory-map orchestration but does **not** spawn vCPU threads, write the kernel into guest RAM, or drive `hv_vcpu_run`. Fix shape: a follow-up phase-1-tail task that bolts on the vCPU thread spawn + kernel/FDT writes via `HvfVm::write_to_region`. Full-boot verification additionally needs the bus + PL011 (Phase 3.1 prerequisite) and the bundled kernel/initramfs (D14) — those gate the user-facing smoke; closing them is a roadmap conversation.

  **Live HVF binding verified** by `crates/hv/tests/hvf_smoke.rs` (run via `make hvf-test`): real `init_with_gic`, real `map_memory` + `write_to_region`, real `hv_vcpu_run`, real `EXCEPTION` exit, `decode_esr` → `EsrDecoded::Hvc { imm16: 0 }`. Cuts through the binding stack end-to-end on Apple Silicon hardware. The remaining blockers are above the binding layer (run-loop body, MMIO bus, PL011, kernel image) — not the HVF integration.

  **Implicit prerequisite — PL011 UART emulation** is not on any phase task list but is required for the Phase 1 exit criterion's "serial output shows /sbin/init" wording. Either move the criterion's serial-output clause to Phase 3 (where the bus + first non-virtio device land) or add a "Phase 1.8 — PL011 emulation" task to the impl plan. Recording the inconsistency here so the next impl-plan revision can resolve it.

## Phase 2 (lands at end of Phase 2.5 review pass)

### Cross-field validation deferred to controller / Phase 3

- **P2** — `MemSizeMib::new` (`crates/api/src/schemas/common.rs:289-301`) only enforces `>= 1`. [10-data-model.md § 2.3](./10-data-model.md#23-schema-layer) and [70-security.md § 4](./70-security.md#4-input-validation) require an upper bound against host RAM minus hypervisor overhead. The `BackendCapabilities` surface needed for that check lands with Phase 1's HVF backend. Fix shape: thread `BackendCapabilities` into the controller, add `MachineConfig::validate_against_host(...)` invoked at dispatch time before the action reaches the VMM event loop.

- **P2** — `BalloonConfig.amount_mib` upper bound (`mem_size_mib − 32`, the upstream `MAX_BALLOON_SIZE_MIB` rule from [21-api-compat-matrix.md § 2 `/balloon`](./21-api-compat-matrix.md#balloon-put)) is not enforced. `crates/api/src/schemas/balloon.rs:RawBalloonConfig` accepts any `u64`. The cross-field check requires the running `mem_size_mib`, so the controller needs to consult its own `vm_config` snapshot. Fix shape: same as above — controller-level cross-field validation gate at dispatch.

- **P2** — Per-class running-count caps (`drives:8`, `network_interfaces:8`, `pmem:4` from `common.rs:21-31`) are only enforced inside `replay.rs` for the static-config path. The HTTP per-PUT path has no running-set tracker yet. Phase 3 wires a device manager; the cap check belongs there. Fix shape: device manager in `squib-vmm` returns `BadRequest` with the spec's documented `fault_message` when a 9th drive is PUT.

### Validator-crate adoption stance

- **P3** — `validator` crate is a workspace dep (`Cargo.toml:52`) but `squib-api` does not use it (the hand-rolled `Raw* → Validated TryFrom` covers the same checks). Either (a) drop the dep from the workspace if no other crate adopts it, or (b) add `#[derive(Validate)]` to `Raw*` shapes for documentation / lintability and call `.validate()` inside `try_from` ([10-data-model.md § 2.3](./10-data-model.md#23-schema-layer) explicitly endorses this layered pattern). Decision can wait until Phase 3 picks a crate-set.

### Spec inconsistencies surfaced

- **P3** — [21-api-compat-matrix.md § 1](./21-api-compat-matrix.md#1-http-api-endpoints) lists `PUT / PATCH / DELETE | /pmem/{id}` and `PUT / GET / PATCH | /hotplug/memory` but [20-firecracker-api.md § 2](./20-firecracker-api.md#2-server-shape) router skeleton omits the `PATCH`/`DELETE` for pmem and the `PUT`/`GET` for hotplug-memory. Phase 2.5 review caught this and the implementation now wires all five — but the spec sample router needs the same correction in its next revision.

- **P3** — `BalloonHintingOp` (start | status | stop) is not pinned in any spec. Phase 2 added the enum at `crates/api/src/schemas/balloon.rs:BalloonHintingOp`; the [21 § 1](./21-api-compat-matrix.md#1-http-api-endpoints) row should call out the three-value vocabulary explicitly so future contributors don't drift on it.

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

- **P2** — I-DEV-4 in [14 § 6](./14-virtio-and-devices.md#6-invariants) reads "virtio-mem hotplug `plug`/`unplug` of an N-block range performs exactly N `Vm::map_memory`/`Vm::unmap_memory` calls." Squib's implementation coalesces the N contiguous blocks into one `MemHotplugBackend::plug(base, N * BLOCK_SIZE)` call (cheaper TLB shootdown). The test at `crates/virtio/src/devices/mem.rs:test_should_plug_n_blocks_in_a_single_backend_call` documents the ambiguity inline. Fix shape: amend I-DEV-4 to "exactly N or one coalesced map of N × BLOCK_SIZE", or split the merged call back into N per-block calls. The merged shape is preferable on Apple Silicon (one stage-2 TLB invalidate vs N).

### Spec inconsistency: VendorID

- **P3** — `crates/virtio/src/transport.rs:53` uses `VENDOR_ID = 0` matching upstream Firecracker (`vendors/firecracker/src/vmm/src/devices/virtio/transport/mmio.rs:25`), which itself carries a `// TODO crosvm uses 0 here, but IIRC virtio specified some other vendor id that should be used`. Compat-suite parity is fine (both sides use `0`), but the virtio spec recommends `0x1AF4` (Red Hat / Qumranet). Fix shape: track upstream Firecracker; if they ever bump to `0x1AF4` we follow.

### `set_status` lacks a transition-rejected-into-DEVICE_NEEDS_RESET test

- **P2** — `crates/virtio/src/transport.rs::set_status` correctly drops invalid driver-init transitions and sets `DEVICE_NEEDS_RESET` on activation failure (line 286), but no unit test asserts that a post-`DRIVER_OK` feature ack returns `(ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK | DEVICE_NEEDS_RESET)` per virtio v1.2 § 2.1. Fix shape: add a unit test in `transport.rs::tests` that drives the device past `DRIVER_OK`, attempts a feature ack, then reads the `Status` register and asserts the `DEVICE_NEEDS_RESET` bit.

### Bus uses `RwLock` on the dispatch hot path

- **P2** — `crates/bus/src/lib.rs::Bus` wraps the `BTreeMap` in `RwLock`, but every `read()`/`write()` call takes the read-lock — ~5–10 ns per MMIO exit, compounding across millions per second. Spec § 2 example shows a bare `BTreeMap`. Fix shape: build the bus immutably via a `BusBuilder` that returns `Arc<Bus>` post-boot, or move the `RwLock` outside `Bus` so callers who mutate during boot pay the cost and the dispatch path uses `&BTreeMap` directly.

### MMDS — base64url helper duplicates a maintained crate

- **P3** — `crates/mmds/src/token.rs::base64url_no_pad` is hand-rolled (~30 LOC). Workspace already plans `aws-lc-rs` for snapshot crypto; pulling `base64 = "0.22"` (or `data-encoding`) is one line and removes hand-coded SIMD-unfriendly bit-shifting that has no fuzz coverage. Fix shape: add `base64` to `[workspace.dependencies]` once any other crate adopts it; replace `base64url_no_pad` with `URL_SAFE_NO_PAD.encode(...)`.

### MMDS interceptor — set_ipv4 needs API-layer wiring

- **P2** — `MmdsInterceptor::set_ipv4` (and the consume-and-return `with_ipv4`) ship in `crates/mmds/src/interceptor.rs`, but the API layer's `PUT /mmds/config { ipv4_address }` handler does not call them yet — the MMDS controller in `crates/api/src/controller.rs` will need to thread the override into the active interceptor when virtio-net comes up. Fix shape: in the device-manager wiring (Phase 4 or later), surface the interceptor handle on the `RuntimeApiController` and call `set_ipv4` from the `MmdsConfig::ipv4_address` field on apply.

## Cross-references

- ← Read by: every phase as the place to land out-of-phase findings.
- → Pairs with: [91-impl-plan.md](./91-impl-plan.md) (a deferred item is a future phase task).
