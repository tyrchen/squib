---
title: 15-mmds — instance metadata service via dumbo + mmds port
type: design
status: draft
last_updated: 2026-05-03
depends_on: 14-virtio-and-devices.md
---

# 15 · MMDS — instance metadata service

Status: draft · Owner: squib-mmds · Depends on: [14-virtio-and-devices.md](./14-virtio-and-devices.md)

## 1. Purpose

Expose the Firecracker MMDS (microVM metadata service) to the guest at the link-local address `169.254.169.254`, identical V1 / V2 (IMDSv2 token) semantics, JSON Pointer traversal, content negotiation. Same wire surface as Firecracker so guest-side IMDS clients work unchanged.

The implementation is a port of the upstream `dumbo` and `mmds` crates from Firecracker (Apache-2.0). Both are OS-agnostic; we vendor them with attribution. See [99-key-decisions.md § D9](./99-key-decisions.md#d9-mmds-vendor-from-firecracker).

## 2. Components

```
┌──────────────────┐     ┌────────────────┐     ┌──────────────┐
│ virtio-net frame │ ──► │ MmdsInterceptor│ ──► │ dumbo TCP    │
│ guest → host     │     │ (peel ARP +    │     │ stack        │
└──────────────────┘     │ TCP-to-IP)     │     └──────┬───────┘
                         └────────────────┘            │
                                                       ▼
                                              ┌────────────────┐
                                              │ mmds JSON tree │
                                              │ + token store  │
                                              └────────────────┘
```

`squib-mmds` exports:

- `MmdsInterceptor` — sits between the virtio-net frontend and the host backend. Peels ARP-for-MMDS-IP and TCP-to-MMDS-IP frames; everything else passes through.
- `Dumbo` — userspace TCP/IP stack handling the peeled frames. Implements ARP, IPv4, TCP. No outbound connections; only responds to guest-initiated traffic.
- `Mmds` — JSON tree store with `serde_json::Value` underneath; JSON Pointer (`/foo/0/bar`) traversal; V2 token store with bounded TTL.

## 3. Packet interception

Wire pattern (mirrors upstream Firecracker):

1. Guest emits ARP for `169.254.169.254` → `MmdsInterceptor` answers with the synthetic MAC `06:00:AC:1E:fe:fe`.
2. Guest emits TCP SYN to `169.254.169.254:80` → `Dumbo` accepts, emits SYN-ACK, completes the handshake.
3. Guest sends HTTP request → `Dumbo` parses the headers; `Mmds` services the path.
4. Response: V1 returns the JSON subtree at the requested pointer; V2 requires a `X-aws-ec2-metadata-token` header issued by a prior `PUT /latest/api/token` (TTL bounded by `mmds-config.token_ttl_seconds`).

Intercepted packets never reach the host backend (vmnet, gvproxy). The interceptor runs synchronously on the device thread.

TTL on response packets is fixed at 1 (link-local). Source MAC is the synthetic MMDS MAC. Source IP is `169.254.169.254`.

## 4. API surface

| Endpoint | Behaviour |
|----------|-----------|
| `PUT /mmds` | Replace the MMDS JSON tree with the request body |
| `PATCH /mmds` | RFC 7396 merge-patch the existing tree |
| `GET /mmds` | Return the tree |
| `PUT /mmds/config` | Configure: `version` (V1/V2), `network_interfaces` (which iface IDs the interceptor binds to), `ipv4_address` (link-local override; default 169.254.169.254), `imds_compat` (force EC2-IMDS plain-text format on `Accept: text/plain`) |

Pre-boot mutations only. Post-boot `PUT /mmds/config` is rejected with the upstream `fault_message`.

## 5. Behaviour edges

- **Race during boot**: if `mmds-config` arrives before `network-interfaces`, the controller queues a deferred bind and applies it when the matching `iface_id` is created. Symmetric for the reverse order.
- **Snapshot resume**: the JSON tree and the V2 token store persist across snapshots (carried in `MicrovmState.mmds_state` — see [10-data-model.md § 5](./10-data-model.md#5-microvmstate--the-snapshot-state-blob)). Active TCP connections do **not** persist; the dumbo stack drops them on resume and the guest reconnects.
- **Memory bound**: `--mmds-size-limit <bytes>` (default 51200) caps the JSON tree size in bytes. `PUT /mmds` exceeding the cap returns 413.
- **No outbound**: the dumbo stack never initiates outbound connections; if the JSON tree references external URLs, that's a guest-side concern.

## 6. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-MMDS-1 | All MMDS-bound traffic from the guest is intercepted; none reaches the host backend. | Integration test: capture host backend ingress, assert no MMDS-IP packets |
| I-MMDS-2 | Token TTL is enforced; expired tokens return 401. | Unit test with mocked clock |
| I-MMDS-3 | V1 and V2 content negotiation matches upstream Firecracker byte-for-byte for the catalogued fixtures. | Compat suite ([72-testing-strategy.md § 3](./72-testing-strategy.md#3-compat-suite)) |
| I-MMDS-4 | The MMDS JSON tree is never logged at `info` or below; per CLAUDE.md § Cryptography, secrets in the tree must not leak. | Unit test asserting redaction in the tracing layer |

## 7. Cross-references

- ← Depends on: [14-virtio-and-devices.md](./14-virtio-and-devices.md) (virtio-net interception point), [10-data-model.md](./10-data-model.md) (`MmdsState`)
- → Consumed by: [20-firecracker-api.md](./20-firecracker-api.md) (`/mmds` and `/mmds/config` endpoints), [16-snapshots.md](./16-snapshots.md) (state persistence)
- ↔ Related research: [docs/research/firecracker-subsystems.md § MMDS](../docs/research/firecracker-subsystems.md)
