---
title: 21-api-compat-matrix — Firecracker API line-by-line bookkeeping
type: design
status: draft
last_updated: 2026-05-03
depends_on: 00-prd.md, 20-firecracker-api.md
supersedes: squib-api-compat-design.md
---

# 21 · API Compatibility Matrix — line-by-line bookkeeping

Status: draft · Owner: squib-api · Depends on: [00-prd.md](./00-prd.md), [20-firecracker-api.md](./20-firecracker-api.md)

## 1. Purpose

[00-prd.md](./00-prd.md) says squib is interface-compatible. This file is the per-field bookkeeping. Every endpoint, field, CLI flag, and behavior in upstream Firecracker is listed with squib's status. When upstream adds a field, this file gets a new row; when squib's behaviour drifts, this file is the diff target.

Squib has **one backend** (HVF, Apple Silicon, aarch64 guest). There is no `--hypervisor` flag; the matrix is a single column.

## Status legend

- **F** — full parity, identical observable behavior.
- **P** — parity in shape; semantics differ in a documented way.
- **A** — accepted (parses without error) but **no-op** on macOS, with a one-time warning.
- **R** — rejected with a clear `fault_message`. Cannot work without lying to callers.

Day-1 commitments. No "Late" status — everything ships in 1.0.

## 1. HTTP API endpoints

| Method | Path | Status | Notes |
|--------|------|--------|-------|
| GET | `/` | F | InstanceInfo: id, state, vmm_version="1.16-firecracker-compat (squib X.Y.Z)", app_name="Firecracker". `state` serializes to exactly the upstream three-value vocabulary: `"Not started"` (literal space, lowercase 's'), `"Running"`, `"Paused"`. Internal richer phases (`Uninitialized`/`Starting`/`Shutdown`) are collapsed via `LifecyclePhase::wire_state` ([11-runtime-core.md § 3.1](./11-runtime-core.md#31-internal-lifecyclephase-vs-wire-vmstate)). |
| GET | `/version` | F | `{"firecracker_version": "1.16.0"}` for SDK version sniffers |
| GET | `/vm/config` | F | full materialized VmmConfig |
| PATCH | `/vm` | F | Pause/Resume |
| GET / PUT / PATCH | `/machine-config` | F | see field-level rules below |
| PUT | `/boot-source` | F | gzip / zstd kernels auto-decompressed at config-load; raw `Image` and PE both supported |
| PUT / PATCH / DELETE | `/drives/{id}` | F | DELETE post-boot only |
| PUT / PATCH / DELETE | `/network-interfaces/{id}` | F | `host_dev_name` semantics differ (P at field level) |
| PUT | `/vsock` | F | UDS multiplex protocol bit-identical |
| GET / PUT / PATCH | `/mmds` | F | |
| PUT | `/mmds/config` | F | |
| GET / PUT / PATCH | `/balloon` | F | |
| GET / PATCH | `/balloon/statistics` | F | |
| PATCH | `/balloon/hinting/{start,status,stop}` | F | virtio-balloon free-page hinting (preview); we implement |
| PUT | `/entropy` | F | |
| PUT | `/serial` | F | |
| PUT / PATCH / DELETE | `/pmem/{id}` | F | virtio-pmem on memory-mapped file |
| PUT / GET / PATCH | `/hotplug/memory` | F | virtio-mem with HVF memory slot management |
| PUT | `/cpu-config` | P | aarch64 best-effort applied; x86 fields accept-and-warn |
| PUT | `/actions` | F | InstanceStart, FlushMetrics; SendCtrlAltDel rejected (R) — x86-only |
| PUT | `/snapshot/create` | F | Full and Diff both supported (Diff via `hv_vm_protect` dirty tracking) |
| PUT | `/snapshot/load` | F | File and Uffd backends both supported (Uffd via Mach exception ports) |
| PUT | `/logger` | F | |
| PUT | `/metrics` | F | |

Every endpoint defined in upstream Firecracker's `firecracker.yaml` (1.16.x) is covered.

## 2. Field-level compatibility

### `/machine-config`

| Field | Status | Comment |
|-------|--------|---------|
| `vcpu_count` | F | bounded `1..=32`, matching upstream `MAX_SUPPORTED_VCPUS` (`vendors/firecracker/src/vmm/src/vmm_config/machine_config.rs`). Effective ceiling is `min(32, host_physical_cores, hv_vm_get_max_vcpu_count())` so we never accept a count we cannot actually run. |
| `mem_size_mib` | F | bounded by host RAM minus hypervisor overhead |
| `smt` | F | accepted as `false` (default); rejected with `fault_message` only when `true` is passed. Matches upstream behaviour on aarch64 (smt=true is restricted to x86 in the OpenAPI). |
| `track_dirty_pages` | F | enables `hv_vm_protect`-based dirty bitmap |
| `cpu_template` | P | "V1N1" applies aarch64 sysreg subset; x86 templates ("C3"/"T2"/etc.) accept-and-warn |
| `huge_pages` | A | "2M" warns once; macOS manages page sizes |

### `/boot-source`

| Field | Status | Comment |
|-------|--------|---------|
| `kernel_image_path` | F | Image, Image.gz (flate2), Image.zst (zstd), PE (linux-loader::pe) all supported |
| `initrd_path` | F | placed at 1 GiB-aligned offset above kernel |
| `boot_args` | F | user value passed verbatim (no rewriting); the FDT builder *appends* `console=ttyAMA0` and `panic=1` only if absent, and `root=PARTUUID=<uuid>` for the root drive when applicable. See [13-arch-and-boot.md § 6.1](./13-arch-and-boot.md#61-boot-args-composition). |

### `/drives/{id}` PUT

| Field | Status | Comment |
|-------|--------|---------|
| `drive_id` | F | regex `^[A-Za-z0-9_]+$` |
| `is_root_device` | F | |
| `path_on_host` | F | regular file, opened with `F_NOCACHE` (the macOS `O_DIRECT` analogue) |
| `is_read_only` | F | |
| `cache_type` | F | Unsafe / Writeback both honored |
| `io_engine` | F | Sync = blocking; Async = tokio `spawn_blocking` pool |
| `partuuid` | F | passed through to kernel cmdline as `root=PARTUUID=<uuid>` if `is_root_device` |
| `rate_limiter` | F | token bucket on per-device queue |
| `socket` (vhost-user) | A | accept-and-warn; vhost-user is Linux-only |

### `/network-interfaces/{id}` PUT

| Field | Status | Comment |
|-------|--------|---------|
| `iface_id` | F | |
| `host_dev_name` | P | mapped to a vmnet handle name `squib-tap-<iface_id>`; literal Linux TAP names are not honored |
| `guest_mac` | F | auto-generated if missing |
| `rx_rate_limiter`, `tx_rate_limiter` | F | token bucket |

### `/vsock` PUT

| Field | Status | Comment |
|-------|--------|---------|
| `guest_cid` | F | minimum 3 |
| `uds_path` | F | bit-identical multiplex protocol |
| `tsi` (squib extension) | new | opt-in TSI mode |

### `/mmds/config` PUT

| Field | Status | Comment |
|-------|--------|---------|
| `version` | F | V1 / V2 |
| `network_interfaces` | F | binds dumbo intercept to listed iface IDs (max 8, matching the per-class cap on `network_interfaces`) |
| `ipv4_address` | F | link-local; must be in `169.254.0.0/16`; default `169.254.169.254` |
| `imds_compat` | F | |
| `token_ttl_seconds` (V2 only) | F | bounded `1..=21600` (6 h, matches upstream `MAX_TOKEN_TTL_SECONDS`); default 21600 |

### `/balloon` PUT

| Field | Status | Comment |
|-------|--------|---------|
| `amount_mib` | F | bounded `0..=mem_size_mib − 32` (matches upstream `MAX_BALLOON_SIZE_MIB`); a `PATCH` exceeding the cap returns 400 |
| `deflate_on_oom` | F | guest's virtio-balloon driver respects this; squib forwards verbatim |
| `stats_polling_interval_s` | F | bounded `0..=255`; `0` disables polling (matches upstream) |
| `free_page_hinting` | F | |
| `free_page_reporting` | F | uses `madvise(MADV_DONTNEED)` to return memory to the host |

### `/snapshot/create` PUT

| Field | Status | Comment |
|-------|--------|---------|
| `snapshot_type=Full` | F | |
| `snapshot_type=Diff` | F | requires `track_dirty_pages: true` in machine-config |
| `snapshot_path`, `mem_file_path` | F | |

### `/snapshot/load` PUT

| Field | Status | Comment |
|-------|--------|---------|
| `snapshot_path` | F | bitcode + magic-id matched |
| `mem_backend.backend_type=File` | F | |
| `mem_backend.backend_type=Uffd` | F | postcopy via Mach exception ports; the `backend_path` is a Unix socket the page-server connects to |
| `track_dirty_pages` | F | for subsequent diff snapshots |
| `resume_vm` | F | |
| `clock_realtime` | A | accept-and-ignore (x86_64-only field; no kvmclock on aarch64) |
| `network_overrides` | F | |
| `vsock_override` | F | |
| top-level `mem_file_path` (deprecated) | F | accepted for back-compat |

### `/cpu-config` PUT

| Field | Status | Comment |
|-------|--------|---------|
| aarch64 `reg_modifiers` | F (best-effort) | applied via `hv_vcpu_set_sys_reg` for registers we own; warns per unsupported |
| aarch64 `vcpu_features` | F (best-effort) | similar |
| x86 `cpuid_modifiers` | A | accept-and-warn |
| x86 `msr_modifiers` | A | accept-and-warn |
| `kvm_capabilities` | A | accept-and-warn |

### `/actions` PUT

| `action_type` | Status | Comment |
|---------------|--------|---------|
| `InstanceStart` | F | |
| `FlushMetrics` | F | |
| `SendCtrlAltDel` | R | x86-only; rejected with `fault_message` |

### `/logger` and `/metrics` PUT

All fields F. File or FIFO targets both work; `mkfifo` is supported on macOS.

## 3. CLI flag compatibility

| Flag | Status | Notes |
|------|--------|-------|
| `--api-sock <path>` | F | default `/run/firecracker.socket` retained |
| `--id <str>` | F | |
| `--config-file <path>` | F | |
| `--metadata <path>` | F | path to a JSON file whose contents seed the MMDS tree at startup, *before* the API server binds. Equivalent to issuing `PUT /mmds` immediately on first boot. File must be ≤ `--mmds-size-limit` bytes; oversize file is fatal at startup (clap-validated). |
| `--no-api` | F | requires `--config-file` |
| `--seccomp-filter <path>` | A | accept-and-warn (no Linux BPF on macOS) |
| `--no-seccomp` | A | accept-and-warn |
| `--log-path <path>` | F | |
| `--level <Error\|Warning\|Info\|Debug\|Trace\|Off>` | F | |
| `--module <path>` | F | tracing module filter |
| `--show-level` | F | |
| `--show-log-origin` | F | |
| `--metrics-path <path>` | F | |
| `--http-api-max-payload-size <bytes>` | F | |
| `--mmds-size-limit <bytes>` | F | |
| `--boot-timer` | F | virtio boot-timer device |
| `--enable-pci` | A | accept-and-warn (squib uses virtio-MMIO regardless) |
| `--start-time-us`, `--start-time-cpu-us`, `--parent-cpu-time-us` | F | for boot accounting |
| `--version` | F | |
| `--snapshot-version` | F | prints squib's snapshot format version |
| `--describe-snapshot <path>` | F | reads upstream-format files where structurally compatible |
| **squib-only**: `--network <shared\|bridged\|host\|userspace>` | new | `shared` default |

There is **no** `--hypervisor` flag.

## 4. Static config file (`--config-file`) field map

Same JSON schema, kebab-case top-level keys. `boot-source` is the only mandatory member. Each nested object follows the per-endpoint table above.

Squib-specific extension keys (silently ignored when consumed by upstream Firecracker so files remain portable in the squib→firecracker direction):

```json
{
  "squib": {
    "network": "shared" | "bridged" | "host" | "userspace",
    "vsock_tsi": false,
    "gvproxy_path": "/opt/squib/libexec/gvproxy",
    "macos_sandbox_profile": null
  }
}
```

These keys are `#[serde(default)]` and never required.

## 5. MMDS guest-side compatibility

| Behavior | Status |
|----------|--------|
| ARP for MMDS IP | F |
| TCP/HTTP to MMDS IP, V1 | F |
| IMDSv2 token PUT/GET | F |
| JSON Pointer path traversal | F |
| `Accept: application/json` → JSON | F |
| `Accept: text/plain` → IMDS plain | F |
| `imds_compat: true` overrides Accept | F |
| TTL 1 on response packets | F |

## 6. vsock guest-side compatibility

| Behavior | Status |
|----------|--------|
| host→guest: `CONNECT <port>\n` then `OK <port>\n` | F |
| guest→host: `<uds_path>_<port>` listener | F |
| VIRTIO_VSOCK_OP_RST on missing listener | F |
| VIRTIO_VSOCK_EVENT_TRANSPORT_RESET on snapshot resume | F |
| TSI mode (squib extension) | new (opt-in) |

## 7. Snapshot file format

| Element | Status |
|---------|--------|
| Outer envelope: `bitcode::serialize(Snapshot{header: SnapshotHdr{magic, version: semver::Version}, data: MicrovmState})` followed by 8-byte LE CRC-64 ISO 3309 (bit-identical to upstream `vendors/firecracker/src/vmm/src/snapshot/mod.rs`) | F |
| State file magic (`0x07101984_AAAA_0000` aarch64) — carried *inside* the bitcode envelope, not as a raw u64 prefix | F |
| State file magic (`0x07101984_8664_0000` x86_64) | n/a (squib does not produce x86 snapshots) |
| `version` field of type `semver::Version`, validated by upstream's `major == SNAPSHOT_VERSION.major && minor <= SNAPSHOT_VERSION.minor` rule | F |
| `MicrovmState` contents (sysreg subset, GIC blob shape) | P — wire envelope identical, *contents* are HVF-shaped. `firecracker --describe-snapshot` against a squib file deserialises and reports header/version/CRC; the embedded vCPU & GIC state is not consumable by KVM Firecracker. |
| Memory file: full | F |
| Memory file: sparse-of-dirty | F |
| `--describe-snapshot <squib-file>` invoked from upstream Firecracker | structurally compatible (header/version/CRC verify); inner state opaque |
| Cross-VMM (KVM↔HVF) replay | not supported (different sysreg subset, different timer/GIC state); explicit non-goal — see [99-key-decisions.md § D10](./99-key-decisions.md#d10-cross-host-snapshot-replay-not-supported) |
| Same-VMM save/restore | F |

## 8. Logger / Metrics field schema

We adopt upstream's metric struct definitions verbatim (port from `src/vmm/src/logger/metrics.rs`). All top-level keys (`api_server`, `balloon`, `block`, `block_<id>`, `entropy`, `get_api_requests`, `put_api_requests`, `patch_api_requests`, `latencies_us`, `logger`, `mmds`, `net`, `net_<iface>`, `rtc`, `uart`, `vhost_user_*`, `vsock`, `vcpu`, `seccomp`, `signals`, `vmm`, `utc_timestamp_ms`) are emitted; macOS-irrelevant counters (e.g. `seccomp.num_faults`) are pinned to zero rather than removed.

Logger: same `[level] origin: message` shape, same rate-limited macros.

## 9. Error response shape

Identical: `{"fault_message": "<reason>"}` with the same set of HTTP status codes (200, 204, 400, 413). `Server: Firecracker API` header on every response. Squib additionally emits **504 Gateway Timeout** when an `ApiAction` exceeds its per-class timeout from [70-security.md § 6](./70-security.md#6-resource-limits) — upstream Firecracker has no equivalent because its actions are bounded by KVM ioctls that complete or hard-fault, not by long-running orchestration; squib needs a way to surface "the VMM is wedged" without leaving the client hanging. 504 is documented in `docs/api-deviations.md` as a squib-only response code.

Common 400 causes squib emits with the documented messages:
- `"Invalid arch field for SMT: SMT not supported on Apple Silicon"` (R: `smt: true` rejected; `smt: false` and absence are F).
- `"Invalid action: SendCtrlAltDel is x86-only and not supported on aarch64"` (R: x86-only action).
- `"Invalid drive: vhost-user backend not supported on this build"` (A — actually accept-and-warn, not reject).
- `"Snapshot rejected: <SnapshotError variant message>"` (R for malformed files; covers MagicMismatch / VersionMismatch / CrcMismatch / Incompatible per [11-runtime-core.md § 6](./11-runtime-core.md#6-error-types)).

## 10. Test surface

For each row in this matrix, [72-testing-strategy.md § 3](./72-testing-strategy.md#3-compat-suite) defines a parity test:

- **F rows**: pass-through replay of the upstream Firecracker integration test cases (suitably aarch64-adjusted).
- **P rows**: a squib-specific test asserts the documented deviation (e.g. `host_dev_name` mapping to vmnet handle).
- **A rows**: a test asserts the field is accepted, the warning is emitted, and the VM otherwise boots.
- **R rows**: a test asserts a 400 response with the documented `fault_message` substring.

CI runs the full matrix against ad-hoc-signed local builds; releases additionally run against notarized builds.

## 11. Cross-references

- ← Depends on: [00-prd.md](./00-prd.md), [20-firecracker-api.md](./20-firecracker-api.md)
- → Consumed by: [50-cli.md](./50-cli.md), [72-testing-strategy.md](./72-testing-strategy.md)
- ↔ Related research: [docs/research/firecracker-api-surface.md](../docs/research/firecracker-api-surface.md), [docs/research/firecracker-subsystems.md](../docs/research/firecracker-subsystems.md)
