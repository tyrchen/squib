---
title: 20-firecracker-api — axum on UDS, error model, static-config replay
type: design
status: draft
last_updated: 2026-05-03
depends_on: 10-data-model.md, 11-runtime-core.md
---

# 20 · Firecracker API — axum on UDS, error model, static-config replay

Status: draft · Owner: squib-api · Depends on: [10-data-model.md](./10-data-model.md), [11-runtime-core.md](./11-runtime-core.md)

## 1. Purpose

Expose the upstream Firecracker REST API verbatim over a Unix domain socket. Every endpoint, every JSON field, every status code, every header preserved. The line-by-line per-field bookkeeping is in [21-api-compat-matrix.md](./21-api-compat-matrix.md); this file pins the *machinery*: routing, state, error shape, static-config replay.

## 2. Server shape

`axum` on `tokio::net::UnixListener`. Per CLAUDE.md § Async & Concurrency, Tokio multi-thread runtime explicitly enabled.

```rust
use axum::{Router, routing::{get, put, patch, delete}};

pub fn router(controller: Arc<RuntimeApiController>) -> Router {
    Router::new()
        .route("/", get(handlers::instance_info))
        .route("/version", get(handlers::version))
        .route("/vm/config", get(handlers::vm_config))
        .route("/vm", patch(handlers::patch_vm))
        .route("/machine-config", get(...).put(...).patch(...))
        .route("/boot-source", put(handlers::put_boot_source))
        .route("/drives/{id}", put(...).patch(...).delete(...))
        .route("/network-interfaces/{id}", put(...).patch(...).delete(...))
        .route("/vsock", put(...))
        .route("/mmds", get(...).put(...).patch(...))
        .route("/mmds/config", put(...))
        .route("/balloon", get(...).put(...).patch(...))
        .route("/balloon/statistics", get(...).patch(...))
        .route("/balloon/hinting/{op}", patch(...))
        .route("/entropy", put(...))
        .route("/serial", put(...))
        .route("/pmem/{id}", put(...).patch(...).delete(...))
        .route("/hotplug/memory", put(...).get(...).patch(...))
        .route("/cpu-config", put(...))
        .route("/actions", put(...))
        .route("/snapshot/create", put(...))
        .route("/snapshot/load", put(...))
        .route("/logger", put(...))
        .route("/metrics", put(...))
        .with_state(controller)
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("server"),
            HeaderValue::from_static("Firecracker API"),
        ))
        .layer(RequestBodyLimitLayer::new(http_api_max_payload_size))
}
```

Middleware:

- `Server: Firecracker API` header on every response (per [00-prd.md § 12](./00-prd.md#12-naming-conventions-binding) — sniffed by SDKs).
- Body-size limit per `--http-api-max-payload-size` (default 51200; range 1024..=1_048_576).
- `tracing` request span at `info` level with `instance_id`, `method`, `path`. No request body in the span (PII, MMDS data).

## 3. Error envelope

Every 4xx response uses `FaultMessage` from [10-data-model.md § 2.1](./10-data-model.md#21-error-body--every-4xx-response):

```json
{"fault_message": "<reason>"}
```

A custom Axum `IntoResponse` impl on `ApiError` produces the (StatusCode, Json<FaultMessage>) tuple. `ApiError` variants:

```rust
#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("invalid configuration: {0}")] BadRequest(String),       // 400
    #[error("payload too large")]           PayloadTooLarge,          // 413
    #[error("conflict: {0}")]               Conflict(String),         // 400 (Firecracker uses 400 not 409)
    #[error("internal error")]              Internal(#[source] Error),// 500 (rare; logs at error)
}
```

Status codes match upstream: 200 / 204 success; 400 / 413 client errors; 500 only on truly internal faults. No 404 — `axum`'s default 404 is overridden to a 400 with `fault_message="No such resource"`.

## 4. State machine

`RuntimeApiController` holds the pre-boot vs post-boot state and validates each `ApiAction` against the admissibility table from [21-api-compat-matrix.md § 1](./21-api-compat-matrix.md#1-http-api-endpoints) before forwarding to the VMM event loop.

```text
Uninitialized ── PUT /machine-config, /boot-source, /drives, /network-interfaces, ...
              ── PUT /actions {InstanceStart}  ──► VMM start path
                            │
                            ▼
NotStarted    ── PUT /snapshot/load                          (resume from snapshot)
              ── PATCH /vm {Resume}                          (post-resume)
              ── PATCH /balloon, /machine-config (limited)
                            │
                            ▼
Running       ── PATCH /vm {Pause}, PUT /snapshot/create, etc.
              ── DELETE /drives/{id}, DELETE /network-interfaces/{id}
              ── ... (post-boot subset)
```

Pre-boot endpoints called post-boot return `400` with `fault_message="The requested operation is not supported after the microVM has booted"`. Post-boot endpoints called pre-boot return the symmetric upstream message.

Per CLAUDE.md § Type Design, the state is encoded as an enum (`InstanceState`) with a state-transition table; it is not "a bunch of bools".

## 5. Channel to VMM

```rust
pub struct RuntimeApiController {
    state: ArcSwap<InstanceState>,
    vmm_tx: mpsc::Sender<(ApiAction, oneshot::Sender<ApiResponse>)>,
}

impl RuntimeApiController {
    pub async fn dispatch(&self, action: ApiAction) -> Result<ApiResponse, ApiError> {
        self.validate_state(&action)?;
        let (tx, rx) = oneshot::channel();
        self.vmm_tx.send((action, tx)).await.map_err(|_| ApiError::Internal(...))?;
        rx.await.map_err(|_| ApiError::Internal(...))
    }
}
```

`ArcSwap` over `InstanceState` per CLAUDE.md § Async & Concurrency: read frequently from request handlers, written rarely on transitions; lock-free reads.

## 6. Static config file (`--config-file`)

`apps/squib/src/cli.rs` parses `--config-file <path>` and, if present, opens the file and loads it through the same code path as the API server, but as an in-process replay of `ApiAction`s.

The schema:

```json
{
  "boot-source":         { "kernel_image_path": "...", "boot_args": "...", "initrd_path": "..." },
  "drives":              [ ... ],
  "machine-config":      { "vcpu_count": 1, "mem_size_mib": 256, ... },
  "cpu-config":          { ... },
  "network-interfaces":  [ ... ],
  "vsock":               { ... },
  "mmds-config":         { ... },
  "balloon":             { ... },
  "entropy":             { ... },
  "logger":              { ... },
  "metrics":             { ... },
  "squib":               { "network": "shared", "vsock_tsi": false, ... }
}
```

- Kebab-case top-level keys per upstream.
- `boot-source` is mandatory; everything else optional.
- Replayed in a deterministic order: `machine-config` → `cpu-config` → `boot-source` → drives → NICs → vsock → mmds-config → mmds → balloon → entropy → serial → pmem → hotplug-memory → logger → metrics → `Action(InstanceStart)` (only if `--no-api` is also set; otherwise the controller waits for an explicit `PUT /actions`).
- The `"squib"` extension key is `#[serde(default)]` and never required; upstream Firecracker silently ignores it, preserving file portability.

There is **no parallel codepath**: the static-config loader builds an `ApiAction` sequence and feeds it to `RuntimeApiController` exactly as the HTTP handler would. Same validation, same errors, same logging.

## 7. OpenAPI document

Available at `GET /openapi.json` (gated on the `--openapi` flag, off by default to match upstream behaviour). Generated at build time from the squib-api request / response struct annotations via `utoipa`. The `firecracker_version` reported in `GET /version` is the upstream version we are pinned against (currently `1.16.0`); the `vmm_version` in `GET /` includes the squib build identifier.

Recorded as [99-key-decisions.md § D12](./99-key-decisions.md#d12-openapi-served-behind-a-flag).

## 8. Behaviour edges

- **Concurrent requests**: axum dispatches request handlers concurrently on the tokio runtime. The `RuntimeApiController.vmm_tx` channel serializes `ApiAction`s into the VMM event loop, which handles them one at a time. There is no parallel mutation path.
- **Long-running actions**: `PUT /snapshot/load` with a large memory file can take seconds. The handler holds the request; the client gets a single 204 on success or 400 on failure. No streaming progress.
- **Pre-flight rejection**: every action validated by `RuntimeApiController.validate_state` before reaching the VMM event loop; the VMM never receives malformed actions.
- **`--no-api` mode**: the API server is not bound; only the static config file is consumed. `--no-api` requires `--config-file`.
- **Unknown fields**: `#[serde(deny_unknown_fields)]` on every endpoint struct except the static-config envelope. A typo in `iface_id` returns 400 with the offending field named.

## 9. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-API-1 | Every response carries `Server: Firecracker API`. | Axum middleware test |
| I-API-2 | Every 4xx response body matches the `{"fault_message": "..."}` shape exactly. | `IntoResponse` impl test + compat suite |
| I-API-3 | Pre-boot vs post-boot admissibility matches upstream Firecracker for every endpoint listed in [21-api-compat-matrix.md § 1](./21-api-compat-matrix.md#1-http-api-endpoints). | Per-endpoint state-machine test |
| I-API-4 | The static-config-file path produces the *same* `ApiAction` sequence (and same errors) as the equivalent HTTP transcript. | Round-trip property test |
| I-API-5 | `--no-api` requires `--config-file`; missing `--config-file` exits with a clap error before the runtime starts. | CLI integration test |
| I-API-6 | The OpenAPI document at `/openapi.json` validates against the published Firecracker `firecracker.yaml` for every shared endpoint. | Schema-diff test in CI ([72-testing-strategy.md § 5](./72-testing-strategy.md#5-upstream-tracking)) |

## 10. Cross-references

- ← Depends on: [10-data-model.md](./10-data-model.md), [11-runtime-core.md](./11-runtime-core.md)
- → Consumed by: [21-api-compat-matrix.md](./21-api-compat-matrix.md), [50-cli.md](./50-cli.md), [70-security.md](./70-security.md) (input validation), [72-testing-strategy.md](./72-testing-strategy.md) (compat suite)
- ↔ Related research: [docs/research/firecracker-api-surface.md](../docs/research/firecracker-api-surface.md), [docs/research/firecracker-architecture.md](../docs/research/firecracker-architecture.md)
