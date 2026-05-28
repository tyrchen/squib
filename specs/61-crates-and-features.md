---
title: 61-crates-and-features — workspace layout, dependency graph, feature flags
type: design
status: draft
last_updated: 2026-05-03
depends_on: 11-runtime-core.md
---

# 61 · Crates and Features — workspace layout, dependency graph

Status: draft · Owner: workspace · Depends on: [11-runtime-core.md](./11-runtime-core.md)

## 1. Purpose

Pin the crate graph. Squib is one Rust workspace; this file is the index of crates, their responsibilities, and their dependency edges. Adding a crate or moving a responsibility goes through this file.

## 2. Workspace layout

```
apps/
  squib-cli/              → CLI binary named `squib`, code-signed with entitlements
  squib-jail/             → drop-in jailer shim (Firecracker-flag-compatible)

crates/
  squib/       (squib)             → public facade for embedded callers and shared runtime wiring
  core/        (squib-core)        → portable types & traits
  api/         (squib-api)         → Firecracker-compatible REST + JSON config loader (axum on UDS)
  hv/          (squib-hv)          → HVF binding via applevisor; the unsafe boundary
  arch/        (squib-arch)        → aarch64 layout, vCPU initial regs, sysreg list, ESR_EL2 decode, PSCI
  fdt/         (squib-fdt)         → FDT builder via vm-fdt
  loader/      (squib-loader)      → kernel loader: Image / Image.gz / Image.zst / PE
  bus/         (squib-bus)         → MMIO bus + BusDevice trait
  virtio/      (squib-virtio)      → virtio-MMIO transport + device subcrates
  gic/         (squib-gic)         → in-kernel GICv3 wrapper (hv_gic_*)
  mmds/        (squib-mmds)        → ported dumbo + mmds
  net/         (squib-net)         → vmnet integration + gvproxy embed
  snapshot/    (squib-snapshot)    → bitcode + serde state file; sparse memory; Mach-exc postcopy
  vmm/         (squib-vmm)         → VMM core: builder, vCPU thread, device manager, event loop
  host/        (squib-host)        → Mach-exception pager, signal handling, child-process management
```

This mirrors the alioth/libkrun layout with squib-specific names. Each crate has a single responsibility; cross-crate types live in `squib-core`. Per CLAUDE.md § Dependencies, minimize dependencies; each crate increases compile time, binary size, and attack surface.

## 3. Dependency graph

```text
                ┌─── apps/squib-cli (binary: squib)
                │
       apps/squib-jail (binary; depends on nothing except std + libc)
                │
                ▼
          crates/squib (facade)
              │        │
              ▼        ▼
            squib-vmm ─────────── squib-api
            ╱   ╱   ╲                │
       squib-virtio  squib-snapshot  squib-mmds
            │            │               │
       squib-bus     squib-host      (vendored dumbo+mmds; std-only)
            │            │
       squib-gic      squib-arch
            │            │
       squib-hv ────────┘
            │
       squib-net (used by virtio-net device sub-crate)
            │
       squib-loader, squib-fdt
            │
        squib-core (zero deps; the bottom of the graph)
```

The `squib-core` crate has zero **squib-workspace** dependencies and only the lightest external ones (`thiserror`, `serde`, `smallvec`). Every other crate transitively depends on it. The "no workspace deps" half of I-CRATE-1 is the load-bearing half: it's what keeps the dependency DAG acyclic.

`squib-hv` is the only crate that links `applevisor`. `squib-net` is the only crate that opens an `unsafe` block for `vmnet`. `#![forbid(unsafe_code)]` everywhere else.

The package name `squib` is reserved for the embeddable facade crate. The CLI package is
`squib-cli`, but it still declares `[[bin]] name = "squib"` so release artifacts and operator
commands remain unchanged. See [22-embedding-facade.md](./22-embedding-facade.md).

## 4. External dependency catalogue

Pinned in `[workspace.dependencies]`:

```toml
# error / async core
anyhow = "1.0"
thiserror = "2.0"
tokio = { version = "1.52", features = ["rt-multi-thread", "macros", "net", "sync", "fs", "io-util", "time", "signal"] }

# serialization
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
bitcode = "0.6"

# observability
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt", "json"] }

# CLI
clap = { version = "4.5", features = ["derive", "env", "wrap_help"] }

# data
bytes = "1"
parking_lot = "0.12"
smallvec = { version = "1", features = ["serde", "union"] }

# validation
validator = { version = "0.20", features = ["derive"] }

# HTTP server (squib-api)
axum = { version = "0.8", default-features = false, features = ["http1", "json", "matched-path", "tokio"] }
http = "1.3"
tower = { version = "0.5", features = ["util"] }
tower-http = { version = "0.6", features = ["set-header", "trace", "limit"] }

# rust-vmm
vm-memory = "0.17"
vm-fdt = "0.3"                                      # last release Nov 2023; stable API (research doc § 2.1)
linux-loader = { version = "0.13", features = ["pe"] }
virtio-queue = "0.16"                               # pin to current minor; bump deliberately
virtio-bindings = "0.2"

# HVF — feature pinned to macOS 15 minimum (D2). See 12-hvf-backend.md § 1.
applevisor = { version = "1.0", features = ["macos-15-0"] }

# compression
flate2 = "1"
zstd = "0.13"

# Mach exceptions
mach2 = "0.4"

# crypto / RNG
aws-lc-rs = "1"

# parsing
winnow = "0.6"

# benchmarking
criterion = "0.5"
```

We **do not** depend on `vmm-sys-util`, `kvm-bindings`, `kvm-ioctls`, `vhost-*`, or `seccompiler` — Linux-only crates that have no business in our build graph.

`cargo-deny` enforces the license allowlist (Apache-2.0, MIT, BSD; LGPL banned) and the dependency policy. `cargo-audit` runs on every CI build.

## 5. Feature flags

Squib is both a facade library and a CLI binary. Feature flags are minimal and stay tied to
runtime capability rather than product variants. Per crate:

| Crate | Feature | Effect |
|-------|---------|--------|
| squib | `bridged` | forwards to `squib-net/bridged` so embedded callers and the CLI use the same entitlement-gated network path. |
| squib-vmm | `bench` | enables criterion bench harnesses |
| squib-snapshot | `postcopy` | compile in the Mach-exception pager (default-on for `squib`, off for tests) |
| squib-net | `bridged` | compile in bridged-mode codepath (off by default; flipped by build that has the `com.apple.vm.networking` restricted entitlement) |
| squib-api | `openapi` | embed the OpenAPI document; `--openapi` then serves `/openapi.json` |

`apps/squib-cli` enables the facade defaults and produces the `squib` binary; CI also exercises a build with all features off to catch flag-rot.

## 6. Build configuration

`rust-toolchain.toml` pins the stable channel (current: `1.95`). `MACOSX_DEPLOYMENT_TARGET=15.0` set in `.cargo/config.toml`.

`[workspace.lints]`:

```toml
[workspace.lints.rust]
missing_debug_implementations = "warn"
missing_docs = "warn"        # in library crates; relaxed in apps via crate-level allow
rust_2024_compatibility = "warn"
unreachable_pub = "warn"
unused_qualifications = "warn"

[workspace.lints.clippy]
pedantic = { level = "warn", priority = -1 }
module_name_repetitions = "allow"
missing_errors_doc = "allow"
missing_panics_doc = "allow"
must_use_candidate = "allow"
```

Boundary modules (`squib-api`, the VMM event loop's `ApiAction` dispatch) layer additional denies:

```rust
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
```

Per CLAUDE.md § Toolchain & Build, `cargo clippy -- -D warnings` is gating CI.

## 7. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-CRATE-1 | `squib-core` has no squib-workspace dependencies (and only minimal external deps — `serde`, `smallvec`, `thiserror`). | CI grep over `crates/core/Cargo.toml` |
| I-CRATE-2 | `unsafe` blocks live only in `squib-hv` and `squib-net::sys`. | `#![forbid(unsafe_code)]` in every other crate; CI grep |
| I-CRATE-3 | `vmm-sys-util`, `kvm-*`, `vhost-*`, `seccompiler` are not in the dependency graph. | `cargo-deny` ban list |
| I-CRATE-4 | `applevisor` is consumed only by `squib-hv`. | `cargo-deny` ban list with `wrappers` allowlist |

## 8. Cross-references

- ← Depends on: [11-runtime-core.md](./11-runtime-core.md)
- → Consumed by: every component design (each names the crate it lives in)
- ↔ Related research: [docs/research/hvf-prior-art-deep-dive.md § Recommended squib crate layout](../docs/research/hvf-prior-art-deep-dive.md)
