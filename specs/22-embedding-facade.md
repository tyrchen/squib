---
title: 22-embedding-facade — public Rust facade for embedded microVMs
type: design
status: draft
last_updated: 2026-05-28
depends_on: 20-firecracker-api.md, 50-cli.md, 61-crates-and-features.md
---

# 22 · Embedding Facade — public Rust API for embedded microVMs

Status: draft · Owner: runtime/API · Depends on: [20-firecracker-api.md](./20-firecracker-api.md), [50-cli.md](./50-cli.md), [61-crates-and-features.md](./61-crates-and-features.md)

## 1. Problem

Before this layer, `squib` is only a binary package under `apps/squib`. Other Rust applications can talk to the Firecracker-compatible UDS API after spawning that binary, but they cannot embed Squib as a library, configure the runtime in-process, dispatch validated API actions directly, or own shutdown deterministically.

The crate/package name `squib` should identify the embeddable facade. The CLI remains available as a binary named `squib`, but its package moves to `apps/squib-cli` so Cargo consumers can depend on `squib` without pulling a binary-only package.

## 2. Goals

| # | Goal | Measure |
|---|------|---------|
| G1 | Provide a stable first Rust facade for embedded callers. | A downstream crate can `use squib::{Squib, SquibBuilder}` and spawn a VM from a static config without depending on `apps/*`. |
| G2 | Preserve CLI compatibility. | `cargo build --bin squib` still produces a binary named `squib`; CLI flags and runtime behavior stay compatible with [50-cli.md](./50-cli.md). |
| G3 | Reuse the production runtime path. | The CLI and embedded facade both use the same controller, VMM loop, config replay, API server, network, and vsock muxer code. |
| G4 | Make lifecycle ownership explicit. | Embedded callers get an owned handle with `controller()`, `dispatch(...)`, `snapshot()`, and async `shutdown()` methods. |

## 3. Non-goals

- No stable semantic-versioned SDK promise beyond the facade types introduced here; the crate is still `0.1.0`.
- No new high-level typed configuration DSL in this phase. Callers may use Firecracker-compatible config files or dispatch `squib-api` validated `ApiAction`s.
- No multi-VM-in-one-process guarantee. HVF global VM initialization remains effectively one live VM per process for the current backend.
- No change to the Firecracker-compatible HTTP API shape.

## 4. Crate and package layout

```text
crates/squib                package "squib"
  ├─ public builder/handle facade
  ├─ runtime wiring shared by CLI + embedded callers
  ├─ macOS VMM event loop and vsock muxer
  └─ non-macOS stub VMM loop for compile/test parity

apps/squib-cli              package "squib-cli"
  └─ [[bin]] name = "squib"
       parses CLI flags and delegates to crates/squib
```

The facade crate depends on `squib-api`, `squib-vmm`, `squib-net`, and the same runtime crates the old binary used. `apps/squib-cli` should depend on the facade and only own CLI parsing plus process-only side channels such as `--snapshot-version` and `--describe-snapshot`.

## 5. Public API contract

```rust
use squib::{InstanceId, Squib, SquibBuilder};

let mut vm = Squib::builder()
    .instance_id(InstanceId::try_from("demo")?)
    .config_file("vm.json")
    .start_microvm(true)
    .spawn()
    .await?;

vm.shutdown().await?;
```

### 5.1 Builder

`Squib::builder()` returns `SquibBuilder`. The builder is `Debug` and owns all runtime options:

| Field | Default | Rule |
|-------|---------|------|
| `instance_id` | `anonymous` | Validated by `squib-api`'s `InstanceId` newtype. |
| `config_file` | `None` | When present, replay through `squib-api::replay_config`; no duplicate parser. |
| `start_microvm` | `true` | Passed to `replay_config`; only meaningful with `config_file`. |
| `api_socket` | `None` | When set, spawn the Firecracker-compatible UDS API server. |
| `http_api_max_payload_size` | `51_200` | Validated by `ServeOptions`; same default as CLI. |
| `timeouts` | `TimeoutTable::from_spec()` | Reuses [70-security.md § 6](./70-security.md#6-resource-limits). |
| `channel_capacity` | `1024` | Bounded API → VMM channel per [20-firecracker-api.md § 5](./20-firecracker-api.md#5-channel-to-vmm-and-the-read-only-fast-path). |
| `network_mode` | shared vmnet | Same modes as [30-networking.md](./30-networking.md). |
| `gvproxy_path` | `None` | Falls back to `/usr/local/libexec/squib/gvproxy` in userspace mode. |
| `bridged_iface` | `None` | Effective only for bridged builds. |
| `run_budget` | 5 min | Passed to the VMM runner watchdog. |
| `mmds_size_limit` | `8192` | Passed to device build args. |
| `firecracker_version` | current compatibility pin | Defaults to the Firecracker pin used by the CLI. |

Every setter returns `Self` for fluent construction. Fallible setters use `TryFrom` for validated newtypes rather than stringly errors.

### 5.2 Handle

`spawn().await` returns `Squib`, an owned runtime handle:

- `controller(&self) -> &Arc<RuntimeApiController>` exposes the low-level controller for advanced integrations.
- `snapshot(&self) -> Arc<ControllerSnapshot>` returns the lock-free read mirror.
- `dispatch(&self, ApiAction) -> impl Future<Output = Result<ApiResponse, SquibError>>` forwards a validated action through the same timeout and phase checks as HTTP handlers.
- `shutdown(&mut self) -> impl Future<Output = Result<(), SquibError>>` sends `ApiAction::Shutdown`, stops the API server task if present, awaits the VMM task, and is idempotent.

Dropping the handle is best-effort only: it aborts the API task if it is still running, but callers who need deterministic VM teardown must call `shutdown().await`.

## 6. Runtime lifecycle

```text
Embedded caller / CLI          squib facade             squib-api              VMM loop
        │                           │                       │                     │
        │ 1. Build options          │                       │                     │
        ├──────────────────────────▶│                       │                     │
        │                           │ 2. RuntimeApiController::new                │
        │                           ├──────────────────────▶│                     │
        │                           │                       │                     │
        │                           │ 3. spawn VMM loop ─────────────────────────▶│
        │                           │                       │                     │
        │                           │ 4. optional config replay via dispatch      │
        │                           ├──────────────────────▶│────────────────────▶│
        │                           │                       │                     │
        │                           │ 5. optional UDS server                      │
        │                           ├──────────────────────▶│                     │
        │ 6. Squib handle ◀─────────│                       │                     │
        │                           │                       │                     │
        │ 7. shutdown()             │                       │                     │
        ├──────────────────────────▶│ 8. ApiAction::Shutdown────────────────────▶│
        │                           │ 9. await tasks        │                     │
```

On macOS, the VMM loop is the production HVF-backed event loop. On non-macOS targets, the facade starts the existing deterministic stub loop so CI can compile and unit-test the API without HVF.

## 7. Errors, safety, and observability

- Error handling follows AGENTS.md § Error Handling: `SquibError` is a `thiserror` enum with `#[source]` for API, replay, IO, and task join failures.
- The facade crate uses `#![forbid(unsafe_code)]`; unsafe remains isolated to `squib-hv` and `squib-net::sys` per [70-security.md § 2](./70-security.md#2-unsafe-boundaries).
- External paths (`config_file`, `api_socket`, `gvproxy_path`) are accepted as `PathBuf` and consumed by existing boundary validators where applicable.
- Public items have doc comments; examples compile on non-macOS by using `start_microvm(false)` or stub dispatch.
- Tracing initialization is not performed by the facade. Embedded callers own subscriber setup; `apps/squib-cli` keeps CLI-specific tracing initialization.

## 8. Tests and exit criteria

| # | Criterion | Evidence |
|---|-----------|----------|
| I-FACADE-1 | `cargo metadata` shows a package named `squib` under `crates/squib` and a package named `squib-cli` under `apps/squib-cli`. | Unit/integration metadata or `cargo check` plus workspace manifest review. |
| I-FACADE-2 | `cargo build --bin squib` still builds the CLI binary. | Build gate. |
| I-FACADE-3 | A facade unit/integration test spawns the non-macOS stub runtime, dispatches a pre-boot action, observes `204`, then shuts down idempotently. | `squib` crate test. |
| I-FACADE-4 | A facade config-file test replays a static config with `start_microvm=false`, proving embedders can preload VM state without serving UDS. | `squib` crate test with `tempfile`. |
| I-FACADE-5 | CLI behavior is delegated, not forked: CLI startup code calls `SquibBuilder::spawn`. | Code review against `apps/squib-cli/src/main.rs`. |

## 9. Cross-references

- ← Depends on: [20-firecracker-api.md](./20-firecracker-api.md), [50-cli.md](./50-cli.md), [61-crates-and-features.md](./61-crates-and-features.md)
- → Consumed by: [91-impl-plan.md § 5.6](./91-impl-plan.md#56-phase-26--embedding-facade)
- ↔ Decision: [99-key-decisions.md § D27](./99-key-decisions.md#d27-squib-package-name-reserved-for-the-embeddable-facade)
