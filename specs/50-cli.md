---
title: 50-cli — squib CLI surface (clap)
type: design
status: draft
last_updated: 2026-05-28
depends_on: 20-firecracker-api.md, 21-api-compat-matrix.md, 22-embedding-facade.md
---

# 50 · CLI — squib CLI surface

Status: draft · Owner: apps/squib-cli · Depends on: [20-firecracker-api.md](./20-firecracker-api.md), [21-api-compat-matrix.md](./21-api-compat-matrix.md), [22-embedding-facade.md](./22-embedding-facade.md)

## 1. Purpose

Pin the surface of the `squib` binary's command-line. Two contracts:

1. Every Firecracker-CLI flag must parse and either apply, accept-and-warn, or reject with a documented `fault_message`.
2. Squib-only extension flags (e.g. `--network`) live in a clearly-marked block so the diff against upstream is visible.

The full per-flag bookkeeping is in [21-api-compat-matrix.md § 3](./21-api-compat-matrix.md#3-cli-flag-compatibility); this file covers the *parser shape* and the binary-level behaviours.

The Cargo package that owns the binary is `squib-cli` under `apps/squib-cli`. The emitted binary is still named `squib`. Runtime startup delegates to the public facade crate described in [22-embedding-facade.md](./22-embedding-facade.md), so the CLI and embedded callers share one controller/VMM loop implementation.

## 2. Parser

`clap = { version = "4.5", features = ["derive", "env", "wrap_help"] }`. Per CLAUDE.md § Code Style, derive macros only; no manual `Command::new()`.

```rust
#[derive(Debug, Parser)]
#[command(name = "squib", version, about = "Firecracker-compatible microVM monitor for Apple Silicon")]
pub struct Cli {
    #[arg(long, default_value = "/run/firecracker.socket")]
    pub api_sock: PathBuf,

    #[arg(long)]
    pub id: Option<String>,

    #[arg(long)]
    pub config_file: Option<PathBuf>,

    #[arg(long)]
    pub no_api: bool,

    #[arg(long)]
    pub metadata: Option<PathBuf>,

    /// Linux-only; accept-and-warn on macOS.
    #[arg(long)]
    pub seccomp_filter: Option<PathBuf>,

    /// Linux-only; accept-and-warn on macOS.
    #[arg(long)]
    pub no_seccomp: bool,

    #[arg(long)]
    pub log_path: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = LogLevel::Info)]
    pub level: LogLevel,

    #[arg(long)]
    pub module: Option<String>,

    #[arg(long)]
    pub show_level: bool,

    #[arg(long)]
    pub show_log_origin: bool,

    #[arg(long)]
    pub metrics_path: Option<PathBuf>,

    #[arg(long, default_value_t = 51200, value_parser = clap::value_parser!(u32).range(1024..=1_048_576))]
    pub http_api_max_payload_size: u32,

    #[arg(long, default_value_t = 51200)]
    pub mmds_size_limit: u64,

    #[arg(long)]
    pub boot_timer: bool,

    /// Accept-and-warn; squib uses virtio-MMIO regardless.
    #[arg(long)]
    pub enable_pci: bool,

    #[arg(long)]
    pub start_time_us: Option<u64>,

    #[arg(long)]
    pub start_time_cpu_us: Option<u64>,

    #[arg(long)]
    pub parent_cpu_time_us: Option<u64>,

    #[arg(long)]
    pub snapshot_version: bool,

    #[arg(long)]
    pub describe_snapshot: Option<PathBuf>,

    // -------- squib extensions --------
    #[arg(long, value_enum, default_value_t = NetworkMode::Shared)]
    pub network: NetworkMode,

    /// Path to the bundled gvproxy binary (when --network=userspace).
    #[arg(long, env = "SQUIB_GVPROXY_PATH")]
    pub gvproxy_path: Option<PathBuf>,

    /// Bundled sandbox-exec profile name (default | permissive).
    #[arg(long)]
    pub macos_sandbox_profile: Option<String>,

    /// Embed an OpenAPI document at GET /openapi.json.
    #[arg(long)]
    pub openapi: bool,
}
```

Exits cleanly on parse error with a 2-style exit code (clap default).

## 3. Mode selection

| Combination | Effect |
|-------------|--------|
| `--config-file` only | Static-config replay; api server starts after replay completes; client can `PUT /actions {InstanceStart}` |
| `--config-file` + `--no-api` | Static-config replay; replay ends with auto-start; api server never bound |
| no `--config-file`, no `--no-api` | API server only; caller drives boot via REST |
| `--no-api` without `--config-file` | Error (clap-validated); exit 2 |
| `--describe-snapshot <path>` | Read-only mode: print snapshot summary, exit 0 |
| `--snapshot-version` | Print snapshot format version, exit 0 |
| `--version` | Print squib version, exit 0 |

There is **no** `--hypervisor` flag — squib has one backend.

## 4. Tracing setup

`tracing-subscriber` initialised in `main` before any other code runs:

- `--level` controls the global filter.
- `--module <path>` adds a module-targeted filter.
- `--show-level` includes the `[level]` prefix on every line.
- `--show-log-origin` includes `<file>:<line>`.
- `--log-path` writes to file (or FIFO; `mkfifo` works on macOS) with the upstream `[level] origin: message` formatting; rate-limited per CLAUDE.md § Logging & Observability.

JSON formatting via `--level json` is a squib extension recorded in extensions.

## 5. Boot accounting

`--start-time-us`, `--start-time-cpu-us`, `--parent-cpu-time-us` are honored verbatim and surfaced in metrics. Useful when squib is invoked by a launcher that wants to report end-to-end VM boot time including its own startup.

## 6. Behaviour edges

- **Conflicting flags**: `--no-api` without `--config-file` is rejected at parse time, not runtime.
- **Unknown flags**: clap default behaviour — exit 2 with usage. Upstream jailer-launched calls always pass exactly the upstream-known set, so this only fires on user typos.
- **Env vars**: `SQUIB_GVPROXY_PATH` overrides the `--gvproxy-path` default. Per CLAUDE.md § Code Style, environment access is centralized in clap's `env` attribute, not scattered.
- **Help text**: `--help` and `-h` both produce the wrap-helped output. We override the bin name in synopses to `squib`, not `clap`'s auto-detected name.

## 7. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-CLI-1 | Every flag listed in [21-api-compat-matrix.md § 3](./21-api-compat-matrix.md#3-cli-flag-compatibility) is parseable. | Compat suite |
| I-CLI-2 | Conflicting flag combinations are rejected at parse time, not runtime. | clap-level test |
| I-CLI-3 | `--no-api` requires `--config-file`. | CLI integration test |
| I-CLI-4 | The accept-and-warn flags (`--seccomp-filter`, `--no-seccomp`, `--enable-pci`, etc.) emit exactly one warn-line at startup, never per-VM-action. | Log-capture test |

## 8. Cross-references

- ← Depends on: [20-firecracker-api.md](./20-firecracker-api.md), [21-api-compat-matrix.md](./21-api-compat-matrix.md), [30-networking.md](./30-networking.md)
- → Consumed by: [40-jailer.md](./40-jailer.md), [72-testing-strategy.md](./72-testing-strategy.md)
- ↔ Related research: [docs/research/firecracker-api-surface.md § CLI](../docs/research/firecracker-api-surface.md)
