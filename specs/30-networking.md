---
title: 30-networking — vmnet integration and gvproxy fallback
type: design
status: draft
last_updated: 2026-05-03
depends_on: 14-virtio-and-devices.md
---

# 30 · Networking — vmnet integration and gvproxy fallback

Status: draft · Owner: squib-net + squib-host · Depends on: [14-virtio-and-devices.md](./14-virtio-and-devices.md)

## 1. Purpose

Provide host-side networking for virtio-net via Apple `vmnet.framework`, with a `gvproxy` fallback for environments where the `com.apple.vm.networking` entitlement is unavailable. The frontend (the virtio-net device) is in [14-virtio-and-devices.md § 4.2](./14-virtio-and-devices.md#42-virtio-net); this spec is the host backend.

## 2. Modes

| Mode | Entitlement (beyond `com.apple.security.hypervisor`) | Notes |
|------|-------------------------------------------------------|-------|
| `--network=shared` (default) | **none** | NAT through host via `VMNET_SHARED_MODE` |
| `--network=host` | **none** | host-only via `VMNET_HOST_MODE` |
| `--network=bridged` | `com.apple.vm.networking` (restricted) | bridged via `VMNET_BRIDGED_MODE`; gated on Apple DTS approval; ships disabled by default |
| `--network=userspace` | none | bundled `gvproxy` child process; no entitlement, slightly slower |

Per Apple's `vmnet.framework` documentation and the project research memo (`docs/research/macos-hypervisor-ecosystem.md` § 5.1): only `VMNET_BRIDGED_MODE` requires the restricted `com.apple.vm.networking` entitlement. NAT (`shared`) and host-only modes work with just `com.apple.security.hypervisor` (which any HVF-using binary already carries). This corrects an earlier draft of this spec that conflated the entitlement requirements; recorded as [99-key-decisions.md § D17](./99-key-decisions.md#d17-vmnet-entitlement-clarification).

The `host_dev_name` field in `PUT /network-interfaces/{id}` is mapped deterministically to a vmnet handle named `squib-tap-<iface_id>`. Literal Linux TAP names are accepted as opaque labels — the field is preserved for snapshot round-trip but the bytes do not influence host setup.

## 3. vmnet binding (`squib-net::sys`)

The second of two `unsafe` boundaries in the workspace (the first being `squib-hv`). Hand-rolled FFI to `vmnet.framework`; no maintained Rust crate exists. Per [99-key-decisions.md § D13](./99-key-decisions.md#d13-vmnet-via-hand-rolled-ffi).

Surface:

```rust
pub struct VmnetIface {
    handle: vmnet_interface_ref,        // opaque framework type
    queue:  dispatch_queue_t,           // libdispatch queue for callbacks
    mtu:    u32,
    mac:    [u8; 6],
}

impl VmnetIface {
    pub fn start(mode: VmnetMode, params: &VmnetParams) -> Result<Self>;
    pub fn read(&self, frames: &mut [PacketBuf]) -> Result<usize>;
    pub fn write(&self, frames: &[PacketBuf]) -> Result<usize>;
    pub fn stop(self) -> Result<()>;
}
```

Each method carries a `// SAFETY:` comment describing the vmnet API contract it depends on. ~300 lines of `unsafe`, no more. The dispatch queue is a serial libdispatch queue dedicated to the interface; the read/write callbacks land on that queue.

Frame buffers come from a pre-allocated `bytes::BytesMut` pool sized to MTU × queue_depth, per CLAUDE.md § Performance (avoid unnecessary allocations; bring in bytes when handling payload).

## 4. Userspace mode (`gvproxy`)

When the user has no `com.apple.vm.networking` entitlement (e.g. a non-admin developer), `--network=userspace` bundles `gvproxy` as a child process:

- `gvproxy` is invoked at VM start with a UDS for the control plane and a socketpair for the L2 frame stream.
- Bundled binary path: `<install-prefix>/libexec/squib/gvproxy` (configurable via `"squib": { "gvproxy_path": "..." }` or env `SQUIB_GVPROXY_PATH`).
- Throughput is lower than vmnet shared mode but adequate for inner-dev-loop workloads.
- Bundling licence: `gvproxy` is Apache-2.0; attribution in `NOTICE`.

The squib process reaps the gvproxy child on shutdown. Per CLAUDE.md § Async & Concurrency, the child is managed via `tokio::process::Command` with explicit await / kill on the `Drop` path.

## 5. Bridged mode

Requires the **restricted** form of `com.apple.vm.networking`. Apple grants this on application; squib ships with bridged disabled by default. Operators with the entitlement re-sign the binary or install a separately-signed build that flips the flag.

Per [00-prd.md § 13 Risks](./00-prd.md#13-risks): `com.apple.vm.networking` denied for bridged is *certain* for general users; gvproxy is the mitigation.

## 6. Behaviour edges

- **Snapshot resume**: vmnet handles are not snapshot-portable. On restore, squib starts fresh interfaces with the same `squib-tap-<iface_id>` names; the guest sees `VIRTIO_NET_F_LINK_DOWN` followed by re-up, matching upstream Firecracker behaviour.
- **MTU**: defaults to vmnet's reported MTU (typically 1500); can be overridden via the squib extension config.
- **MAC collision**: `guest_mac` is honored verbatim; if absent, auto-generated using `06:00:` + 4 random bytes (locally-administered range).
- **Promiscuous mode**: `gvproxy` does not support; `vmnet` does in `host` mode. Not exposed via the API; controlled by `--network` mode selection.

## 7. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-NET-1 | The only `unsafe` blocks outside `squib-hv` live in `squib-net::sys`. | CI grep + `#![forbid(unsafe_code)]` everywhere else |
| I-NET-2 | `--network=userspace` works without `com.apple.vm.networking`. | Integration test on a build with no entitlement |
| I-NET-3 | `host_dev_name` is round-trip-preserved through snapshot save/restore even though it is opaque. | Snapshot golden test |
| I-NET-4 | Frame allocation uses the pre-allocated `BytesMut` pool; no per-packet `Vec<u8>` allocation in the hot path. | Allocation benchmark in [71-performance-budgets.md § 5](./71-performance-budgets.md#5-network-throughput) |
| I-NET-5 | gvproxy child is reaped on every shutdown path (Drop, signal, panic). | Unit test with a sentinel child |

## 8. Cross-references

- ← Depends on: [14-virtio-and-devices.md](./14-virtio-and-devices.md)
- → Consumed by: [70-security.md § 3](./70-security.md#3-unsafe-boundaries) (unsafe boundary), [71-performance-budgets.md](./71-performance-budgets.md), [50-cli.md](./50-cli.md)
- ↔ Related research: [docs/research/macos-hypervisor-ecosystem.md](../docs/research/macos-hypervisor-ecosystem.md) (vmnet API summary, gvproxy)
