---
title: 70-security — threat model, unsafe boundaries, validation, secrets
type: design
status: draft
last_updated: 2026-05-03
depends_on: 00-prd.md, 11-runtime-core.md
---

# 70 · Security — threat model, unsafe boundaries, validation, secrets

Status: draft · Owner: workspace · Depends on: [00-prd.md](./00-prd.md), [11-runtime-core.md](./11-runtime-core.md)

## 1. Threat model

Squib targets a **developer machine**. The operator is trusted; the threat model is not "an arbitrary tenant attacks the host." With that scoped down, the realistic adversaries are:

- **A misconfigured guest image** that emits malformed virtio frames, oversized MMIO bursts, or pathological ESR_EL2 patterns.
- **A malicious snapshot file** dropped by an attacker who controls a path on the host filesystem.
- **A misconfigured launcher** that posts oversized HTTP bodies, deeply nested JSON, or Unicode-pathological identifiers.
- **A compromised gvproxy upstream** when bundled (we ship a pinned binary; upstream supply-chain still matters).
- **An unrelated process on the host** that watches `/run/firecracker.socket`. UDS permissions matter.

Out of scope: side-channel attacks across guests, Spectre-class issues already mitigated by Apple Silicon, kernel-level escapes from a compromised host kernel.

## 2. Rust safety

Per CLAUDE.md § Safety & Security:

- `#![forbid(unsafe_code)]` at the crate root of every crate **except** `squib-hv`, `squib-net::sys`, `squib-host`, and `squib-jail`. The latter two were widened during Phase 5 / Phase 6: `squib-host` carries the Mach exception-port FFI for the postcopy pager (lifecycle skeleton today; live `mach_msg` server feature-gated per [`93-improvements-review.md` Phase 5](../specs/93-improvements-review.md#phase-5-lands-at-end-of-phase-5-review-pass)); `squib-jail` calls libc privilege-drop syscalls and `sandbox_init(3)`.
- Each unsafe-bearing crate carries a `// SAFETY:` comment per `unsafe` block referencing the framework contract it relies on. Code review for any change that adds or relaxes an `unsafe` block requires explicit sign-off — the per-crate justification is *FFI surface*, not relaxed soundness.
- No transmute between unrelated types, no aliasing `&mut`, no uninitialized reads, no out-of-bounds. `cargo +nightly miri test` runs against the test suite excluding HVF-touching tests.
- Boundary modules (`squib-api`, `RuntimeApiController`'s action dispatch) lint with `clippy::unwrap_used`, `clippy::expect_used`, `clippy::indexing_slicing`, `clippy::panic`, `clippy::expect_used` denied.
- Library crates use `thiserror`-derived enum errors; the CLI uses `anyhow` only at `main.rs`.

## 3. Unsafe boundaries

Two, total. Both are documented contracts with Apple frameworks.

### 3.1 `squib-hv`

- All `applevisor` calls (`hv_vm_*`, `hv_vcpu_*`, `hv_gic_*`).
- Mach exception helpers used by the postcopy pager (in `squib-host` actually; `squib-hv` only does the HVF-side mapping).

Each block carries a `// SAFETY:` comment referencing the relevant Apple Hypervisor Framework documentation section. Code review for any change that adds an `unsafe` block requires explicit sign-off.

### 3.2 `squib-net::sys`

- `vmnet.framework` FFI: `vmnet_start_interface`, `vmnet_read`, `vmnet_write`, `vmnet_stop_interface` against `dispatch_queue`.

~300 lines of `unsafe`, isolated. The safe wrapper above (`squib-net::VmnetIface`) exposes a Rust-typed surface. Per CLAUDE.md § FFI boundaries, never expose `*mut T` to safe callers.

## 4. Input validation

Per CLAUDE.md § Input Validation. The mechanism — `#[serde(try_from = "RawT")]` newtypes that run validation **inside** `TryFrom::try_from` — is pinned in [10-data-model.md § 2.3](./10-data-model.md#23-schema-layer). The point: validation is the only path from JSON to a domain type, and the type system makes "unvalidated `DriveConfig`" unrepresentable. Calling `.validate()` after the fact is not the contract; the constructor *is* the contract.

What that buys us:

- **Length caps on every string** from external input. Default 256 **bytes** (not chars; multi-byte exhaustion is a real attack). Raised deliberately per field. `User-Agent`-class amplification attacks (entire HTML in a header field) are real; we cap aggressively.
- **Range caps on every integer**. `vcpu_count: 1..=32` (matches upstream `MAX_SUPPORTED_VCPUS`), `mem_size_mib: 1..=host_ram_minus_overhead`, `http_api_max_payload_size: 1024..=1_048_576`.
- **Regex allowlists, never blocklists**. Identifiers (`drive_id`, `iface_id`, `id`): `^[A-Za-z0-9_]{1,64}$`. Slugs and free-form short fields use the same pattern.
- **Bounded collections**. `Vec<DriveConfig>`, `HashMap<...>` from external input have explicit element-count caps (e.g. `drives: max 8`, `network_interfaces: max 8`).
- **`#[serde(deny_unknown_fields)]`** on every endpoint struct *and* on the `"squib"` extension sub-object. The single exception is the **top-level** static-config envelope, which must tolerate `"squib": {...}` for forward-compat with future squib extension keys; the `"squib"` sub-object is itself `deny_unknown_fields` so a typo inside it still 400s.
- **Newtypes** for validated values (`DriveId(String)`, `IfaceId(String)`, `MemSizeMib(u64)`, `SafePath(PathBuf)`) with private fields and fallible constructors. The constructor is the only public entry point; downstream code is provably safe by construction.

The `validator` crate is a useful annotation surface (`#[validate(length(max = 256), regex = "...")]` on the `Raw*` shape) but its `.validate()` call lives **inside** `TryFrom::try_from`, not as a post-deserialization afterthought a handler might forget.

## 5. Path inputs

Per CLAUDE.md § Injection Prevention:

- Reject `..`, absolute paths in fields meant to be relative, NUL bytes, OS-specific separators in identifiers.
- For `path_on_host` (drives), `kernel_image_path`, `initrd_path`, `snapshot_path`, `mem_file_path`: cap at `PATH_MAX = 1024` bytes (Darwin), canonicalize, and verify the path opens with the expected file-type (`stat(2)` post-open and assert `S_IFREG`). Symlinks defeat naïve checks; we re-canonicalize after open where possible.
- For UDS paths (`api_sock`, `vsock.uds_path`, `mem_backend.backend_path` when `backend_type=Uffd`): cap at `sizeof(sockaddr_un.sun_path) − 1 = 103` bytes on Darwin. The UDS connect / bind silently truncates beyond this; a hard cap surfaces the misconfiguration as a 4xx instead of a "connection refused" debugging session.
- For the chroot in `squib-jail`: re-canonicalize after open.

## 6. Resource limits

Per CLAUDE.md § Resource Limits:

- **HTTP body size**: `--http-api-max-payload-size` (default 51200, range 1024..=1_048_576), enforced by `tower_http::limit::RequestBodyLimitLayer`.
- **Timeouts** (per `ApiAction` variant; pinned at the controller, not in the handler):

  | Action class | Default timeout | Rationale |
  |--------------|-----------------|-----------|
  | Pre-boot configuration (every PUT/PATCH/DELETE on `/drives`, `/network-interfaces`, `/machine-config`, ...) | 5 s | Pure config state mutation; if it stalls something is stuck |
  | `Action(InstanceStart)` | 30 s | Boot orchestration including FDT build, kernel load, GIC create |
  | `PUT /snapshot/create` | 5 min | Bounded by memory-file write throughput; hard cap at 5 min on a 32 GiB VM |
  | `PUT /snapshot/load` | 5 min | Symmetric; postcopy mode returns sooner but the open-file handshake counts |
  | `PATCH /vm` (Pause/Resume) | 5 s | Quiesce wait |
  | `PATCH /balloon` | 30 s | Ballooning to a large amount can take time as the guest releases pages |

  Implemented as a per-action `tokio::time::timeout` wrapping the `oneshot::recv()`. On timeout the action is *not* cancelled (would leave the VMM in an undefined state); instead the API returns 504 with a `fault_message` and the controller logs the still-pending action at `error`.

- **Concurrency caps**: bounded mpsc channels between API server and VMM event loop (capacity 1024). Bounded JoinSets for block-IO and per-disk worker threads.
- **Recursion limits**: `serde_json` default recursion limit halved for untrusted input. `validator`-derived rules kick in at deserialization time so deeply nested JSON is bounded by the field-tree depth, not the parser.

## 7. Cryptography & secrets

Per CLAUDE.md § Cryptography & Secrets:

- **Constant-time comparison** for any token check (V2 IMDS token in MMDS): `subtle::ConstantTimeEq`.
- **Randomness**: `aws-lc-rs::rand::SystemRandom` for IDs / nonces / virtio-rng output. Never `thread_rng()`.
- **No password hashing in 1.0** (no auth surface; UDS permissions are the boundary).
- **No secrets in logs**: MMDS data is never logged at info or below; the tracing layer redacts it. Custom `Debug` impls on request types redact the `Authorization` header field if present (defense-in-depth — there is no `Authorization` field in the upstream API, but we redact pre-emptively for forward compat).
- **Secret loading**: env / secret-manager only; never hard-code, never bake into binaries.
- **TLS**: not used in 1.0 (UDS only). When added, `rustls` with `aws-lc-rs` backend.

## 8. UDS permissions

`/run/firecracker.socket` is created with mode `0600`, owned by the invoking user. The launcher is responsible for adjusting permissions if it wants to share the socket with another uid. Squib does not chmod the socket up.

## 9. Code-signing & entitlements

Per [00-prd.md § R11](./00-prd.md#8-hard-requirements-10):

- Default binary signed with `com.apple.security.hypervisor` (self-claimable). The bridged-enabled build additionally embeds `com.apple.vm.networking` (restricted; requires Apple DTS approval) — shipped as a separately-signed binary, gated by the `bridged` cargo feature in `squib-net`. NAT (`--network=shared`) and host-only modes do **not** require `com.apple.vm.networking`; see [99-key-decisions.md § D17](./99-key-decisions.md#d17-vmnet-entitlement-clarification).
- Hardened runtime flag (`--options runtime`) on every signed binary.
- CI runs against an ad-hoc-signed local build; releases are notarized.
- `MACOSX_DEPLOYMENT_TARGET=15.0` pinned in `.cargo/config.toml`.

## 10. Supply chain

- `cargo audit` on every CI run.
- `cargo deny check` on every CI run, enforcing license allowlist (Apache-2.0, MIT, BSD; LGPL banned) and the dependency-ban list (`vmm-sys-util`, `kvm-*`, `vhost-*`, `seccompiler`, `openssl`, `native-tls`).
- Vendored bundled binaries (gvproxy) are pinned to a SHA-256 in `vendors/`; upgrade requires a deliberate PR with a fresh hash.
- Pre-commit hook scans for `.env*`, `credentials*`, `*.pem`, etc. — secret-scanning before commits leave the laptop.

## 11. Boundary panics

Reachable panics from external input are denied at lint level. The `RuntimeApiController` and `squib-api` handler modules carry `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]`. A vCPU thread panic transitions the VM to `Shutdown` but does not bring down the process; see [11-runtime-core.md § 5](./11-runtime-core.md#5-panic-policy).

## 12. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-SEC-1 | `unsafe` lives only in `squib-hv`, `squib-net::sys`, `squib-host`, and `squib-jail`. | CI grep + `#![forbid(unsafe_code)]` everywhere else |
| I-SEC-2 | Every external string field has a length cap and a regex / charset allowlist. | Per-field `validator` rules; compat suite asserts oversized inputs return 4xx |
| I-SEC-3 | Every external integer has a range cap. | Per-field `validator` rules |
| I-SEC-4 | No `panic!`, `unwrap`, `expect`, `[]`-indexing, `unreachable!`, `todo!` reachable from API input. | Lint denied in boundary modules |
| I-SEC-5 | The MMDS JSON tree never appears in logs at `info` or below. | Tracing-layer redaction unit test |
| I-SEC-6 | UDS is created `0600`, owned by the invoking user. | Integration test |
| I-SEC-7 | Releases are notarized; CI verifies stapled tickets. | `make notarize` + CI step |
| I-SEC-8 | `cargo audit` and `cargo deny check` are gating. | CI step |

## 13. Cross-references

- ← Depends on: [00-prd.md](./00-prd.md), [11-runtime-core.md](./11-runtime-core.md)
- → Consumed by: every component design (each invokes this for its boundary discipline)
- ↔ Related research: [docs/research/firecracker-architecture.md § Security](../docs/research/firecracker-architecture.md)
