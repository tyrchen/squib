---
title: Firecracker Public Interface Specification
status: research
audience: engineers implementing squib (macOS interface-compatible Firecracker)
source: vendors/firecracker @ submodule HEAD (firecracker.yaml swagger + src/firecracker, src/vmm/src/vmm_config, src/vmm/src/resources.rs)
last_reviewed: 2026-05-03
---

# Firecracker Public Interface Specification

This document is the canonical reference for **what squib must accept and return** at the wire level to be a drop-in replacement for Firecracker. Wherever schemas disagree between the OpenAPI spec and the Rust source, the source is authoritative — flagged inline.

## 1. HTTP/REST API (micro-http over Unix socket)

### Transport

- **Protocol**: HTTP/1.1 over a Unix domain socket (never TCP).
- **Default socket path**: `/run/firecracker.socket`, overridable via `--api-sock`.
- **Content-Type**: `application/json` for request and response bodies.
- **Server header**: `Server: Firecracker API`.
- **Error body shape**: `{"fault_message": "<reason>"}`.
- **Version**: 1.16.0-dev per the in-tree swagger; no version negotiation header — the OpenAPI doc is the implicit contract.
- **Implementation**: the (now-vendored) `micro-http` crate, single thread, epoll-driven, requests processed sequentially.

### Status codes

| Code | Used for |
|------|----------|
| 200 OK | Successful GET returning JSON |
| 204 No Content | Successful PUT/PATCH, no body |
| 400 Bad Request | Malformed JSON, invalid value, semantic error, unknown path |
| 413 Payload Too Large | MMDS data exceeds `--mmds-size-limit` |

### Endpoint catalog

#### Instance & introspection

| Method | Path | Pre/post boot | Purpose |
|--------|------|---------------|---------|
| GET | `/` | either | InstanceInfo (id, state, vmm_version, app_name) |
| GET | `/version` | either | `{"firecracker_version": "..."}` |
| GET | `/vm/config` | either | Full materialised VmmConfig |
| PATCH | `/vm` | post-boot | `{"state": "Paused"|"Resumed"}` |

#### Machine config

| Method | Path | Pre/post boot |
|--------|------|---------------|
| GET | `/machine-config` | either |
| PUT | `/machine-config` | pre-boot |
| PATCH | `/machine-config` | pre-boot |

```json
{
  "vcpu_count": 1,                  // 1..=32, must be even if smt=true
  "mem_size_mib": 1024,
  "smt": false,                     // x86_64 only; always false on aarch64
  "track_dirty_pages": false,       // diff snapshots prerequisite
  "cpu_template": "None",           // None|C3|T2|T2S|T2CL|T2A|V1N1
  "huge_pages": "None"              // None|2M
}
```

#### Boot source (pre-boot only)

```json
PUT /boot-source
{
  "kernel_image_path": "vmlinux.bin",   // required
  "initrd_path": "initrd.img",          // optional
  "boot_args": "console=ttyS0 reboot=k panic=1 pci=off"  // optional
}
```

#### Drives

| Method | Path | Pre/post boot | Notes |
|--------|------|---------------|-------|
| PUT | `/drives/{drive_id}` | pre-boot | full create/replace |
| PATCH | `/drives/{drive_id}` | post-boot | rate limiter / path only |

PUT body:
```json
{
  "drive_id": "rootfs",                       // ^[A-Za-z0-9_]+$
  "is_root_device": true,
  "path_on_host": "rootfs.ext4",              // virtio-block; omit for vhost-user
  "is_read_only": false,
  "cache_type": "Unsafe",                     // Unsafe|Writeback
  "io_engine": "Sync",                        // Sync|Async (Async = io_uring)
  "partuuid": null,
  "rate_limiter": null,
  "socket": null                              // path for vhost-user-block
}
```

PATCH body:
```json
{ "drive_id": "rootfs", "path_on_host": "...", "rate_limiter": { ... } }
```

#### Network interfaces

| Method | Path | Pre/post boot |
|--------|------|---------------|
| PUT | `/network-interfaces/{iface_id}` | pre-boot |
| PATCH | `/network-interfaces/{iface_id}` | post-boot (rate limiters only) |

```json
{
  "iface_id": "eth0",
  "host_dev_name": "tap0",        // Linux TAP device name
  "guest_mac": "06:00:00:00:00:00",
  "rx_rate_limiter": null,
  "tx_rate_limiter": null
}
```

> **macOS note**: `host_dev_name` is a Linux TAP name; on macOS this field's semantics need to be remapped to a vmnet interface or a host-side networking handle. Squib must define a deterministic mapping (see design spec).

#### vsock (single device per VM)

```json
PUT /vsock
{
  "guest_cid": 3,                  // >= 3
  "uds_path": "/tmp/v.sock"
}
```

#### MMDS

| Method | Path | Pre/post boot |
|--------|------|---------------|
| GET | `/mmds` | either |
| PUT | `/mmds` | either (replace) |
| PATCH | `/mmds` | either (RFC7396 merge) |
| PUT | `/mmds/config` | pre-boot |

```json
PUT /mmds/config
{
  "version": "V1",                 // V1|V2 (V2 = AWS IMDSv2 token)
  "network_interfaces": ["eth0"],
  "ipv4_address": "169.254.169.254",
  "imds_compat": false             // when true, returns text/plain regardless of Accept
}
```

#### Balloon (single device per VM)

| Method | Path | Pre/post boot |
|--------|------|---------------|
| GET | `/balloon` | either |
| PUT | `/balloon` | pre-boot |
| PATCH | `/balloon` | either (amount_mib) |
| GET | `/balloon/statistics` | either (only if enabled pre-boot) |
| PATCH | `/balloon/statistics` | either |

```json
PUT /balloon
{
  "amount_mib": 128,
  "deflate_on_oom": true,
  "stats_polling_interval_s": 0,   // 0 disables stats
  "free_page_hinting": false,
  "free_page_reporting": false
}
```

#### Entropy / Serial / Pmem / Memory hotplug / CPU config

```json
PUT /entropy
{ "rate_limiter": null }

PUT /serial
{ "serial_out_path": "/tmp/fc-console", "rate_limiter": null }

PUT /pmem/{id}
{ "id":"pm0", "path_on_host":"/...", "root_device":false, "read_only":true, "rate_limiter":null }

PUT /hotplug/memory
{ "total_size_mib": 2048, "slot_size_mib": 128, "block_size_mib": 2 }

PUT /cpu-config
// arch-specific: x86_64 has cpuid_modifiers + msr_modifiers,
// aarch64 has reg_modifiers + vcpu_features.
```

#### Actions

```json
PUT /actions
{ "action_type": "InstanceStart" | "FlushMetrics" | "SendCtrlAltDel" }
```

`SendCtrlAltDel` is x86_64 only; on aarch64 it returns 400.

#### Snapshots

```json
PUT /snapshot/create     // VM must be Paused
{
  "snapshot_type": "Full" | "Diff",
  "snapshot_path": "/path/to/state",
  "mem_file_path": "/path/to/memory"
}

PUT /snapshot/load       // pre-boot only
{
  "snapshot_path": "/path/to/state",
  "mem_backend": {
    "backend_type": "File" | "Uffd",
    "backend_path": "/path/to/mem-or-uffd-socket"
  },
  "track_dirty_pages": false,
  "resume_vm": false,
  "network_overrides": [{ "iface_id":"eth0", "host_dev_name":"tap1" }],
  "vsock_override": { "uds_path": "/new/v.sock" },
  "clock_realtime": false           // x86_64 only
}
```

The deprecated top-level `mem_file_path` on `/snapshot/load` is still accepted; squib must accept both shapes.

#### Logger / Metrics

```json
PUT /logger
{
  "level": "Info",
  "log_path": "/tmp/fc.log",
  "show_level": false,
  "show_log_origin": false,
  "module": null
}

PUT /metrics
{ "metrics_path": "/tmp/fc.metrics" }
```

Both targets may be regular files or FIFOs. `FlushMetrics` action triggers an immediate dump.

### Token bucket schema (rate_limiter fields)

Used by drives, network, entropy, serial, balloon, vsock:
```json
{
  "size": 1048576,         // bucket capacity (bytes or ops, per device semantics)
  "refill_time": 1000,     // ms to fully refill
  "one_time_burst": 0      // initial burst (optional)
}
```

A field named `bandwidth` or `ops` on net interfaces wraps a TokenBucket plus a kind discriminator (refer to `vmm_config/net.rs`).

## 2. CLI surface

| Flag | Default | Notes |
|------|---------|-------|
| `--api-sock <path>` | `/run/firecracker.socket` | Unix socket bind path |
| `--id <str>` | `anonymous` | `^[A-Za-z0-9_]+$` |
| `--config-file <path>` | – | JSON boot-time config |
| `--metadata <path>` | – | JSON to seed MMDS |
| `--no-api` | – | Requires `--config-file`; no API socket |
| `--seccomp-filter <path>` | – | Custom BPF filter; conflicts with `--no-seccomp` |
| `--no-seccomp` | – | Disable seccomp |
| `--log-path <path>` | – | Equivalent to PUT /logger at startup |
| `--level <level>` | `Info` | Error/Warning/Info/Debug/Trace/Off |
| `--module <path>` | – | tracing module filter |
| `--show-level` | false | |
| `--show-log-origin` | false | |
| `--metrics-path <path>` | – | |
| `--http-api-max-payload-size <bytes>` | 51200 | |
| `--mmds-size-limit <bytes>` | machine-config dependent | |
| `--boot-timer` | false | Enable boot timer device |
| `--enable-pci` | false | PCIe transport for virtio (newer FC) |
| `--start-time-us`, `--start-time-cpu-us`, `--parent-cpu-time-us` | – | Set by jailer for boot accounting |
| `--version` | – | Prints binary version |
| `--snapshot-version` | – | Prints supported snapshot version |
| `--describe-snapshot <path>` | – | Prints version embedded in a snapshot file |

Squib must accept all of these without error. macOS-irrelevant flags (e.g. `--seccomp-filter`, `--enable-pci`) should still parse; unsupported behavior should warn-and-continue rather than reject, so existing launchers do not break.

## 3. Static config file shape (`--config-file`)

JSON, kebab-case top-level keys. Required: `boot-source`. Everything else optional/nullable.

```json
{
  "boot-source": { "kernel_image_path": "...", "initrd_path": "...", "boot_args": "..." },
  "machine-config": { "vcpu_count": 2, "mem_size_mib": 1024, "smt": false, "track_dirty_pages": false, "cpu_template": "None", "huge_pages": "None" },
  "cpu-config": null,
  "drives": [ { /* PUT /drives schema */ } ],
  "network-interfaces": [ { /* PUT /network-interfaces schema */ } ],
  "vsock": { "guest_cid": 3, "uds_path": "/tmp/v.sock" },
  "balloon": { /* PUT /balloon schema */ },
  "entropy": { "rate_limiter": null },
  "pmem": [],
  "logger": { /* PUT /logger schema */ },
  "metrics": { "metrics_path": "..." },
  "mmds-config": { /* PUT /mmds/config schema */ },
  "memory-hotplug": { /* PUT /hotplug/memory schema */ }
}
```

Parsed by `VmmConfig::from_json()` in `src/vmm/src/resources.rs`. The kebab-case at the top level is unique to the file format — the API uses snake_case nested in JSON.

## 4. Unix-socket transport details

- Long-lived connection; epoll multiplexes accepts.
- `Content-Length` framing; chunked TE not commonly used by clients.
- Single-threaded request handling — sequential ordering of API calls is observable to the user.
- Max payload (`--http-api-max-payload-size`, default 51200 bytes) caps both request body and response body.

Example:
```
PUT /machine-config HTTP/1.1
Content-Type: application/json
Content-Length: 73

{"vcpu_count":2,"mem_size_mib":1024,"smt":false,"track_dirty_pages":false}

HTTP/1.1 204 No Content
Server: Firecracker API
```

## 5. MMDS interface

### Host side
PUT/PATCH `/mmds` with arbitrary JSON; PUT `/mmds/config` to bind to interfaces.

### Guest side (link-local)
- Endpoint: `http://169.254.169.254/`.
- Implementation: `dumbo` (a minimal userspace TCP/IP stack) lives inside the VMM and intercepts virtio-net frames whose dst IP matches the configured MMDS address. The TAP / host backend never sees those packets — they are answered in-process.
- **V1**: `GET <path>` → `Content-Type: application/json` (or `text/plain` if `imds_compat=true`).
- **V2 / IMDSv2**: client first PUTs `/latest/api/token` with header `X-aws-ec2-metadata-token-ttl-seconds`, gets a token, then includes `X-aws-ec2-metadata-token` on subsequent GETs.
- Path semantics traverse the JSON tree (e.g. `/foo/bar` → `body["foo"]["bar"]`); leaves render as text under `imds_compat`.

## 6. vsock semantics

The host side is a **multiplexed Unix socket** at `uds_path`, NOT AF_VSOCK on Linux either — Firecracker's vsock is a userspace virtio-vsock implementation that proxies to/from a Unix-socket convention.

### Host-initiated (host → guest port)
1. Connect to `uds_path`.
2. Send ASCII line `CONNECT <port>\n`.
3. On success the VMM replies `OK <ephemeral_host_port>\n` and the bytes that follow are bidirectional.
4. On failure the socket is closed.

### Guest-initiated (guest → host port)
1. Host runs a Unix-socket listener at the path `<uds_path>_<port>` (e.g. `/tmp/v.sock_52`).
2. Guest connects to `(CID=2, port=52)`. The VMM finds the per-port listener and bridges the streams.

Because the host-side wire is ordinary `SOCK_STREAM` Unix sockets, this is portable to macOS unchanged — squib does **not** need AF_VSOCK on the host.

## 7. Logger & Metrics on FIFOs / files

### Logger
Plaintext, line-per-event:
```
[2026-05-03T10:30:45.123Z] [INFO]  api_server::request: PUT /machine-config
```
Fields gated by `show_level` / `show_log_origin`. Errors/warnings are rate-limited (≈10/5s) by default to prevent log floods.

### Metrics
A single JSON object emitted as one line per flush. Fields are stable counter/gauge names enumerated in `src/vmm/src/logger/metrics.rs`. Flush triggers: periodic timer, `PUT /actions {FlushMetrics}`, graceful shutdown. `lost_metrics` / `lost_logs` counters track FIFO-full drops.

## 8. Snapshot file formats

A snapshot is two files:

- `snapshot_path` — the **state file**: serialized VM + device state (vCPU regs, MSRs, virtio device state, etc.). Format is `versionize`-encoded with a leading version header. `--describe-snapshot <path>` prints that version.
- `mem_file_path` — the **memory file**: contiguous binary dump of guest RAM. For Diff snapshots only the dirty pages are written, sparse.

UFFD load mode replaces the memory file path with a Unix socket path; an external page-server connects, receives a page-fault FD via SCM_RIGHTS, and serves pages on demand. This enables lazy / postcopy resume.

`/snapshot/create` requires the VM to be Paused (PATCH `/vm` first). `/snapshot/load` is pre-boot only — the loaded VM may be auto-resumed via `resume_vm: true`.

## 9. Error response shape

```json
{ "fault_message": "Block device with ID 'rootfs' already exists." }
```

Common 400 causes: malformed JSON, invalid enum, ID not matching `^[A-Za-z0-9_]+$`, file not found, illegal state transition, vCPU count constraints, missing required field. 413 is reserved for MMDS over-limit.

## 10. Versioning & stability

- `GET /version` is the only programmatic version surface.
- No HTTP version header negotiation. The OpenAPI document at `src/firecracker/swagger/firecracker.yaml` is the contract.
- Fields are deprecated by leaving them accepted while documenting replacements (e.g. top-level `mem_file_path` in snapshot/load).

## 11. Linux/KVM-specific fields squib must handle gracefully

| Field | Linux meaning | Squib strategy |
|-------|---------------|----------------|
| `cpu_template` | KVM CPUID/MSR overrides | accept, no-op, log warning unless template plausibly maps to host arch |
| `smt` | x86 hyperthreading | accept; refuse if true on Apple Silicon |
| `track_dirty_pages` | KVM dirty bitmap | requires HVF dirty-tracking on macOS 14+ — accept, fall back to full snapshots if unsupported |
| `huge_pages` | hugetlbfs | accept, no-op (Darwin VM has its own page sizing) |
| `clock_realtime` (snapshot/load) | kvmclock realignment | x86_64 only; accept and ignore on aarch64 |
| `path_on_host` for net | Linux TAP name | remap to vmnet handle deterministically |
| `--seccomp-filter`, `--no-seccomp` | BPF filter installation | accept; no-op or warn (see design spec for sandbox-exec alternative) |
| `--enable-pci` | virtio-PCI transport | accept; squib uses MMIO transport same as upstream default |

The principle: **never break a launcher**. Reject only when a misconfiguration would silently produce wrong behavior; otherwise warn and continue.

## 12. Boot vs runtime API ordering

```
firecracker --api-sock $SOCK &
PUT  /machine-config
PUT  /boot-source
PUT  /drives/rootfs
PUT  /network-interfaces/eth0
PUT  /mmds/config         (optional)
PATCH /mmds                (optional)
PUT  /actions {InstanceStart}
PATCH /vm {Paused}         (optional, e.g. to snapshot)
PUT  /snapshot/create
```

After `InstanceStart`, only PATCH on rate limiters, balloon, MMDS, vm-state and snapshot/create are accepted; squib must enforce the same gate.
