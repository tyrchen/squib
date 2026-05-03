---
title: Firecracker Auxiliary Subsystems
status: research
audience: engineers planning the squib reimplementation on macOS
source: vendors/firecracker submodule (snapshots, MMDS, vsock, jailer, logger, metrics, balloon, entropy, CPU templates, seccomp, PVH/initrd)
last_reviewed: 2026-05-03
---

# Firecracker Auxiliary Subsystems

A field guide for the subsystems that surround the VMM core. Each section ends with a one-line **macOS portability verdict**.

## 1. Snapshots

### File layout

A Firecracker snapshot is **two files**:

- **State file** (`snapshot_path`): vmstate header + serialized `MicrovmState`.
- **Memory file** (`mem_file_path`): contiguous binary dump of guest RAM. Sparse for diff snapshots.

State file structure (16-byte aligned):

| Field | Size | Description |
|-------|------|-------------|
| magic_id | u64 | Architecture identifier: `0x0710_1984_8664_0000` (x86_64), `0x0710_1984_AAAA_0000` (aarch64) |
| version | variable | Snapshot data format version (semver string) |
| state | variable | bitcode-serialized `MicrovmState` blob |
| crc64 | u64 | optional CRC64 over magic + version + state |

Squib must use the **same magic IDs and bitcode encoding** to be load-compatible with files produced by Firecracker on Linux (and vice versa for VMs whose hardware state can in fact be replayed cross-OS — i.e. mostly aarch64).

### Snapshot types

- **Full**: every guest page written to the memory file.
- **Diff**: sparse memory file with only pages dirtied since the last snapshot. Requires either KVM dirty-page logging (when `track_dirty_pages: true` in `machine-config`) or `mincore(2)` (slower; doesn't see swapped pages).

### Memory backends on load

- `File`: kernel handles page faults via `mmap(MAP_PRIVATE)`. The memory file is the authoritative copy; mutations to it during the VM's lifetime are undefined.
- `Uffd`: userspace page-fault handler over `userfaultfd(2)`. Firecracker creates the UFFD, sends the FD over the configured Unix socket via `SCM_RIGHTS`, and the external page-server then services `UFFDIO_COPY`/`UFFDIO_ZEROPAGE` requests on demand. This enables postcopy / lazy resume.

### State components

`MicrovmState` includes:
- `vm_info`: mem size, SMT, cpu_template, boot source, hugepages
- `kvm_state`: KVM capability modifiers
- `vm_state`: emulated HW (timers, GIC distributor, etc.)
- `vcpu_states[]`: registers, sregs, MSRs, XSAVE
- `device_states`: every attached virtio device (block/net/vsock/balloon/rng/MMDS/...).

### Behaviors triggered by snapshot ops

- After a snapshot: dirty bitmap is reset; vsock issues `VIRTIO_VSOCK_EVENT_TRANSPORT_RESET` to the guest (active sessions die, listeners persist with the new CID); on x86_64 a kvmclock notification is injected; VMGenID rotates a 16-byte GUID and Linux ≥5.18 reseeds its PRNG.
- On load: VM is created Paused; auto-resumes if `resume_vm: true`. `vsock_override` and `network_overrides` allow re-pointing host paths.

### Tools

- `snapshot-editor`: `edit-memory rebase` (collapse a chain of diff layers onto a base), `edit-vmstate remove-regs` (strip aarch64 regs by KVM ID), `info-vmstate version|vcpu-states|vm-state`.
- `rebase-snap`: deprecated predecessor.

**macOS portability verdict**: feasible with substitution. The bitcode format and file layout are portable; the gaps are (a) replacing UFFD with a Mach-exception-based on-demand pager (or stubbing to File-only), and (b) ensuring HVF dirty-page tracking is wired so diff snapshots remain possible.

## 2. MMDS

### Three-component architecture

1. **Backend** — the host-side HTTP API at `/mmds` (PUT/PATCH/GET) and `/mmds/config`.
2. **Data store** — a global `serde_json::Value`, capped by `--mmds-size-limit` (default 51200 bytes).
3. **dumbo** — a minimalist HTTP/TCP/IPv4 stack that runs *inside the VMM process* and intercepts virtio-net frames whose destination IP is the configured MMDS address.

### Configuration

```json
PUT /mmds/config
{
  "version": "V1" | "V2",
  "network_interfaces": ["eth0"],
  "ipv4_address": "169.254.169.254",
  "imds_compat": false
}
```

### Guest packet flow (dumbo interception)

```
Guest sends frame → virtio-net device
  ├── if EtherType=ARP and target IP=MMDS_IP → dumbo replies with MAC 06:01:23:45:67:01
  ├── if EtherType=IPv4 and dst IP=MMDS_IP and proto=TCP → dumbo TCP handler
  │     ├── per-connection state machine, no congestion control
  │     ├── reassembles request bytes
  │     └── HTTP request → walk JSON tree using the URI as JSON Pointer (RFC 6901)
  └── otherwise → forwarded to host TAP/vmnet as normal
```

### v1 vs v2

- **V1**: plain `GET /path`, no auth. Counters `mmds.rx_invalid_token` / `rx_no_token` warn users to migrate.
- **V2**: AWS IMDSv2 token flow. Guest first PUTs `/latest/api/token` with `X-metadata-token-ttl-seconds: <1..=21600>`; receives a token; supplies it on every subsequent GET via `X-metadata-token`.

### Output format

- `Accept: application/json` → JSON.
- `Accept: text/plain` (or absent) → IMDS plain-text format (directories as newline-separated keys; leaf values as plaintext).
- `imds_compat: true` → always plaintext, ignoring Accept.

### Snapshot behavior

The MMDS data store is **NOT** persisted across snapshots (avoids leaking VM-scoped secrets into golden images). MMDS version, network bindings, and IP address **are** persisted. If the snapshotted version is V2 but the loader doesn't support V2, it transparently falls back to V1.

**macOS portability verdict**: wire-compatible reimplementation feasible. dumbo is pure userspace logic; only the virtio-net interception point shifts (still inside our virtio-net device implementation, just running on top of vmnet on the host instead of TAP).

## 3. vsock host-guest bridge

Firecracker implements a **userspace virtio-vsock** that mediates between AF_UNIX on the host and AF_VSOCK in the guest. There is **no AF_VSOCK on the host side**, even on Linux — that is critical for macOS portability.

### Configuration

```json
PUT /vsock
{ "guest_cid": 3, "uds_path": "/tmp/v.sock" }
```

### Multiplexing protocol

#### Host-initiated (host → guest port)

1. Connect to the AF_UNIX socket at `uds_path`.
2. Send `CONNECT <port>\n` (decimal, ASCII LF).
3. If the guest is listening on that port, Firecracker replies `OK <ephemeral_host_port>\n`. Otherwise the socket closes.
4. Bytes flow bidirectionally thereafter.

#### Guest-initiated (guest → host port)

1. The host runs an AF_UNIX listener at `<uds_path>_<port>` (e.g. `/tmp/v.sock_52`).
2. Guest connects to `(CID=2, port=52)`.
3. Firecracker pairs that with the per-port listener and bridges the streams.

### Wire details

Three virtio queues (rx, tx, event); 64 KiB max packet. Op codes per the kernel virtio-vsock UAPI: `REQUEST=1`, `RESPONSE=2`, `RST=3`, `SHUTDOWN=4`, `RW=5`, `CREDIT_UPDATE=6`. CID 2 reserved for host; guest CID minimum 3.

**macOS portability verdict**: wire-compatible reimplementation **trivial**. The host-side wire is `SOCK_STREAM` AF_UNIX, which macOS supports identically. Squib reuses the entire host-side bridge logic; only the virtio device's queue handling needs to be re-plumbed onto whichever hypervisor backend we choose.

## 4. Jailer

`jailer` is a separate binary that sandboxes Firecracker before exec'ing it: chroot, cgroups (v1/v2), uid/gid drop, mount/PID/net namespaces, seccomp, ulimits.

### CLI

Required: `--id`, `--exec-file`, `--uid`, `--gid`. Optional: `--cgroup-version`, `--cgroup KEY=VALUE` (repeatable), `--parent-cgroup`, `--chroot-base-dir` (default `/srv/jailer`), `--netns`, `--resource-limit fsize=…|no-file=…`, `--daemonize`, `--new-pid-ns`. Args after `--` forward to firecracker.

### Steps

1. Validate inputs.
2. Close all FDs except 0/1/2; clear environment.
3. `mkdir -p <chroot_base>/<exec>/<id>/root` and copy the firecracker binary in (avoids text-segment sharing across tenants).
4. setrlimit; create cgroup hierarchy; write parameters.
5. `unshare(CLONE_NEWNS)` + `pivot_root` into the chroot.
6. mknod `/dev/net/tun` and `/dev/kvm`; chown to uid:gid.
7. `setns()` into `--netns` if specified.
8. `--daemonize` → `setsid`, redirect 0/1/2 to `/dev/null`.
9. `--new-pid-ns` → `clone(CLONE_NEWPID)`; jailer becomes pseudo-init.
10. `setgid` + `setuid` to drop privileges.
11. `execve` the firecracker binary with `--id`, `--start-time-us`, `--start-time-cpu-us`.

### Security model

The jailer is part of the trusted compute base; all paths and IDs are presumed safe inputs. Cleanup of cgroups and chroot dirs is the operator's responsibility (or via cgroup `notify_on_release`).

**macOS portability verdict**: no macOS equivalent. chroot/cgroups/namespaces/seccomp simply do not exist as a uniform mechanism on Darwin. The closest cousins:

- `sandbox-exec` / `sandbox_init` (a profile-policy sandbox; static, not per-process resource accounting).
- macOS 26 *Containerization* framework (built on VZ; container-level isolation, much higher level than jailer).
- launchd resource limits and per-user/agent profiles.

**Squib stance**: ship a `squib-jail` binary that emulates the *interface* of the jailer (same flags, same exit codes), implements the safe subset (chroot-equivalent via `chroot(2)` which exists on macOS but is far less useful, ulimits, uid/gid drop, optional `sandbox-exec` profile), and warns on the rest. This keeps existing launchers like Firecracker tests and Kata-on-macOS scripts working without rewrites.

## 5. Logger & Metrics

### Logger

`PUT /logger` with `log_path`, `level`, `show_level`, `show_log_origin`. One-time configurable. Output is plaintext, line-per-event:

```
[2026-05-03T10:30:45.123Z] [INFO]  api_server::request: PUT /machine-config
```

Macros split into rate-limited (`error!`, `warn!`, `info!`) and unrestricted (`*_unrestricted!`) variants — the former protect against guest-induced log floods.

### Metrics

`PUT /metrics` with `metrics_path`. Single-line JSON object per flush. Periodic auto-flush every 60s; manual flush via `PUT /actions {FlushMetrics}`. Top-level fields include:

`utc_timestamp_ms`, `api_server`, `balloon`, `block`, `block_<id>`, `entropy`, `get_api_requests`, `put_api_requests`, `patch_api_requests`, `latencies_us`, `logger`, `mmds`, `net`, `net_<iface>`, `rtc` (aarch64), `uart`, `vhost_user_*`, `vsock`, `vcpu`, `seccomp`, `signals`, `vmm`.

Naming convention encodes units: `_bytes`, `_ms`, `_us`, otherwise count. Two atomic types: `SharedIncMetric` (delta-on-flush counter) and `SharedStoreMetric` (set-and-read gauge). Metrics are **not** persisted across snapshots.

**macOS portability verdict**: wire-compatible reimplementation. Pure userspace data; same JSON schemas, same flush semantics — the only adaptation is replacing FIFO writes if we want async, but POSIX named pipes work identically on macOS.

## 6. Balloon, Entropy, CPU templates

### Balloon

`PUT /balloon { amount_mib, deflate_on_oom, stats_polling_interval_s, free_page_hinting, free_page_reporting }`. APIs: GET/PUT/PATCH on `/balloon` and `/balloon/statistics`; `/balloon/hinting/{start,status,stop}` for the developer-preview hinting feature. Statistics: SWAP_IN/OUT, MAJFLT/MINFLT, MEMFREE/MEMTOT, AVAIL/CACHES, HTLB_*, OOM_KILL, ALLOC_STALL, ASYNC/DIRECT_SCAN/RECLAIM (Linux 6.12+).

Free-page reporting calls `madvise(MADV_DONTNEED)` against the host mmap to actually return memory to the OS.

**Verdict**: feasible with substitution — `madvise` on macOS supports `MADV_DONTNEED` (since 10.x) so the same path works.

### Entropy

`PUT /entropy { rate_limiter? }`. Stateless virtio-rng device sourcing bytes from `aws-lc-rs`.

**Verdict**: portable as-is. (Optionally swap source to `SecRandomCopyBytes` for native macOS ergonomics.)

### CPU templates

Two flavors:
- **Static templates** (deprecated): C3, T2, T2A, T2CL, T2S, V1N1. Configured via `machine-config.cpu_template`.
- **Custom templates**: JSON with `kvm_capabilities`, `cpuid_modifiers`, `msr_modifiers` (x86_64), `vcpu_features`, `reg_modifiers` (aarch64). Bitmaps use `x`/`0`/`1` characters with optional underscores; e.g. `"0bxxxx000000000011xx00011011110010"`. Configured via `PUT /cpu-config`.

Templates are advisory only — they hide CPUID bits and zero MSRs but do not guarantee the guest cannot execute the underlying instructions.

**Verdict**:
- Apple Silicon: not applicable (different ISA; aarch64 reg_modifiers can be honored if the underlying register exists).
- Intel macOS: feasible with severe limits — HVF allows some CPUID shaping but doesn't expose the full leaf/subleaf control KVM does. Squib should accept the field and apply what it can; warn on each unsupported leaf.

## 7. Seccomp filters

Three categories: `vmm`, `api`, `vcpu`. JSON DSL compiled by `seccompiler-bin` into bitcode-encoded BPF tied to thread names. Top-level shape:

```json
{
  "vmm": { "default_action": "Trap", "filter_action": "Allow", "filter": [ ... ] },
  "api": { ... },
  "vcpu": { ... }
}
```

A rule is `{syscall, args: [{index, type, op, val}]}` with `op` ∈ `eq|ne|lt|le|gt|ge|masked_eq` and `type` ∈ `dword|qword`. Default JSON files live under `resources/seccomp/{x86_64,aarch64}-unknown-linux-musl.json`. Loaded at startup via `--seccomp-filter` (custom binary), or compiled-in defaults, or `--no-seccomp` to disable.

**Verdict**: no macOS equivalent. Squib accepts the flags as a no-op with a single startup warning; relies on entitlements + optional `sandbox-exec` profile for defence-in-depth.

## 8. PVH boot, initrd, boot timer

### PVH boot

`xen/pvh.h`-style direct boot. Kernel must carry the `XEN_ELFNOTE_PHYS32_ENTRY` ELF note (Linux: `CONFIG_PVH=y`; FreeBSD ≥14 ships PVH by default). Avoids bootloader emulation, faster boot.

### Initrd

CPIO `newc` archive. Configured via `/boot-source.initrd_path`. Cannot coexist with `is_root_device: true` on a drive. Use `switch_root` (not `pivot_root`) once user-space mounts the real root.

### Boot timer

An implicit virtio device that records the time from `InstanceStart` to first guest write — used for the ≤125 ms boot SLA.

**Verdict**: portable. PVH is a CPU+kernel ABI, not a Linux kernel feature; HVF can host it. Initrd is a guest payload only. Boot timer is a virtio device — same as any other.

## Subsystem portability summary

| Subsystem | Portability | Squib approach |
|-----------|-------------|----------------|
| Snapshots | feasible w/ substitution | bitcode + magic identical; HVF dirty-tracking; UFFD → optional Mach-exception pager |
| MMDS / dumbo | wire-compatible | reuse logic; intercept inside our virtio-net |
| vsock | wire-compatible (trivial) | host-side AF_UNIX bridge unchanged |
| Jailer | no equivalent | ship `squib-jail` shim that accepts the flags and applies the safe subset (chroot, ulimits, uid/gid drop, optional sandbox-exec) |
| Logger / metrics | wire-compatible | reuse JSON schemas verbatim |
| Balloon | feasible w/ substitution | `madvise(MADV_DONTNEED)` works on macOS |
| Entropy | portable | reuse aws-lc-rs or swap to `SecRandomCopyBytes` |
| CPU templates | partial | accept; apply best-effort on HVF; warn on unsupported leaves |
| Seccomp | no equivalent | accept-and-warn; rely on entitlements + sandbox profile |
| PVH / initrd / boot timer | portable | reuse generators verbatim |
