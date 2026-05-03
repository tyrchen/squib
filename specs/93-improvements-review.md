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

## Cross-references

- ← Read by: every phase as the place to land out-of-phase findings.
- → Pairs with: [91-impl-plan.md](./91-impl-plan.md) (a deferred item is a future phase task).
