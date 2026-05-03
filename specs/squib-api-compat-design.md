---
title: squib — Firecracker API Compatibility Matrix
type: design
status: draft
last_updated: 2026-05-03
depends_on: squib-prd.md, squib-design.md, docs/research/firecracker-api-surface.md
supersedes: prior dual-backend matrix (2026-05-03 morning)
---

# squib — Firecracker API Compatibility Matrix

The PRD says squib is interface-compatible. This spec is the line-by-line bookkeeping. Every endpoint, field, CLI flag, and behavior in upstream Firecracker is listed here with squib's status.

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
| GET | `/` | F | InstanceInfo: id, state, vmm_version="1.16-firecracker-compat (squib X.Y.Z)", app_name="Firecracker" (per spec; we identify as the API surface) |
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
| `vcpu_count` | F | bounded `1..=hv_vm_get_max_vcpu_count()` (≥ host physical cores cap) |
| `mem_size_mib` | F | bounded by host RAM minus hypervisor overhead |
| `smt` | R | rejected with `fault_message` ("SMT not supported on Apple Silicon"); upstream restricts to even vcpu_count when true, on Apple Silicon there's no SMT |
| `track_dirty_pages` | F | enables `hv_vm_protect`-based dirty bitmap |
| `cpu_template` | P | "V1N1" applies aarch64 sysreg subset; x86 templates ("C3"/"T2"/etc.) accept-and-warn |
| `huge_pages` | A | "2M" warns once; macOS manages page sizes |

### `/boot-source`

| Field | Status | Comment |
|-------|--------|---------|
| `kernel_image_path` | F | Image, Image.gz (flate2), Image.zst (zstd), PE (linux-loader::pe) all supported |
| `initrd_path` | F | placed at 1 GiB-aligned offset above kernel |
| `boot_args` | F | passed verbatim; no defaults injected unless field is absent |

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
| `network_interfaces` | F | binds dumbo intercept to listed iface IDs |
| `ipv4_address` | F | link-local |
| `imds_compat` | F | |

### `/balloon` PUT

| Field | Status | Comment |
|-------|--------|---------|
| `amount_mib` | F | |
| `deflate_on_oom` | F | |
| `stats_polling_interval_s` | F | |
| `free_page_hinting` | F | |
| `free_page_reporting` | F | uses `madvise(MADV_DONTNEED)` |

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
| `--metadata <path>` | F | |
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

There is **no** `--hypervisor` flag. The earlier dual-backend draft has been removed; squib has one backend.

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
| State file magic (`0x07101984_AAAA_0000` aarch64) | F |
| State file magic (`0x07101984_8664_0000` x86_64) | n/a (squib does not produce x86 snapshots) |
| `bitcode + serde` encoding (matches upstream Firecracker post-1.10) | F |
| Memory file: full | F |
| Memory file: sparse-of-dirty | F |
| `--describe-snapshot` cross-format | best-effort |
| Cross-VMM (KVM↔HVF) replay | not supported (different sysreg subset, different timer/GIC state); explicit non-goal |
| Same-VMM save/restore | F |

## 8. Logger / Metrics field schema

We adopt upstream's metric struct definitions verbatim (port from `src/vmm/src/logger/metrics.rs`). All top-level keys (`api_server`, `balloon`, `block`, `block_<id>`, `entropy`, `get_api_requests`, `put_api_requests`, `patch_api_requests`, `latencies_us`, `logger`, `mmds`, `net`, `net_<iface>`, `rtc`, `uart`, `vhost_user_*`, `vsock`, `vcpu`, `seccomp`, `signals`, `vmm`, `utc_timestamp_ms`) are emitted; macOS-irrelevant counters (e.g. `seccomp.num_faults`) are pinned to zero rather than removed.

Logger: same `[level] origin: message` shape, same rate-limited macros.

## 9. Error response shape

Identical: `{"fault_message": "<reason>"}` with the same set of HTTP status codes (200, 204, 400, 413). `Server: Firecracker API` header on every response.

Common 400 causes squib emits with the documented messages:
- `"Invalid arch field for SMT: SMT not supported on Apple Silicon"` (R: `smt: true` rejected).
- `"Invalid action: SendCtrlAltDel is x86-only and not supported on aarch64"` (R: x86-only action).
- `"Invalid drive: vhost-user backend not supported on this build"` (A — actually accept-and-warn, not reject).

## 10. Test surface

For each row in this matrix, the verification plan defines a parity test:
- **F rows**: pass-through replay of the upstream Firecracker integration test cases (suitably aarch64-adjusted).
- **P rows**: a squib-specific test asserts the documented deviation (e.g. `host_dev_name` mapping to vmnet handle).
- **A rows**: a test asserts the field is accepted, the warning is emitted, and the VM otherwise boots.
- **R rows**: a test asserts a 400 response with the documented `fault_message` substring.

CI runs the full matrix against ad-hoc-signed local builds; releases additionally run against notarized builds.

## 11. What's removed from the prior draft

The earlier matrix had per-backend columns (VZ vs HVF) and a "Late (L)" status for features deferred past 1.0. Both are gone. Single column. No "Late" — the only timeline status is "1.0" or "stretch" (and stretches are flagged in the PRD, not here).

The earlier matrix had **24 rows in R or P state** for VZ-related limitations (Diff snapshots, dirty tracking, Uffd backend, CPU templates, custom virtio devices, per-queue rate limiters). With HVF as the only backend, **all 24 of those are now F**. That's the substantive change.
