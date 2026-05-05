# squib

A macOS-native, Apple-Silicon-only microVM monitor with a Firecracker-compatible
HTTP API. Squib speaks the same JSON shapes, the same UDS surface, the same
SDK-friendly `Server: Firecracker API` header that upstream Firecracker
publishes — but on top of HVF + `vmnet.framework` rather than KVM + Linux TAP.

- **Apple-Silicon-only**, **HVF-only** (no VZ), **aarch64 Linux guests only**.
- Single 1.0 release with full Firecracker compat on day one.
- Snapshots: full and Diff (via `hv_vm_protect` dirty tracking) + Mach-exception postcopy.
- Networking: `--network={shared|bridged|host|userspace}` — vmnet for the
  entitled cases, gvproxy for the rest.

中文：[README.zh-CN.md](./README.zh-CN.md)。

## Status

> Pre-1.0. Phase 7 ("compat suite + perf + polish") is in flight; the live
> end-to-end boot path tail (vCPU thread + kernel image + PL011 wiring) is
> tracked under [`specs/93-improvements-review.md`](./specs/93-improvements-review.md)
> "Phase 1 (lands at end of Phase 1.6)".

## Quick start

```bash
# Build + ad-hoc-sign for local development (HVF needs the
# com.apple.security.hypervisor entitlement).
make sign

# Spin up an empty squib speaking the Firecracker API on a UDS.
target/release/squib --api-sock /tmp/squib.sock

# Drive it with curl, the upstream getting-started.md sequence, or any SDK that
# treats `Server: Firecracker API` as authoritative.
curl --unix-socket /tmp/squib.sock http://localhost/version
```

To boot a real Linux guest:

```bash
make build-reference-vm    # downloads vmlinux + alpine busybox into examples/reference-vm/build/
make demo                   # codesigned HVF integration test
```

## Documentation

- **User guide** in [`docs/user-guide.md`](./docs/user-guide.md)
  ([中文](./docs/user-guide.zh-CN.md)) — install, boot a guest, drive it from
  an SDK, snapshot/restore, troubleshoot.
- **Developer guide** in [`docs/dev-guide.md`](./docs/dev-guide.md)
  ([中文](./docs/dev-guide.zh-CN.md)) — workspace layout, build/test loops,
  HVF codesigning, where to plug in for new features.
- **Specs** under [`specs/`](./specs/index.md) — design, data model, runtime,
  HVF backend, devices, MMDS, snapshots, networking, jailer, CLI.
- **Research** under [`docs/research/`](./docs/research/index.md) — prior-art deep
  dive, HVF / aarch64 / vmnet write-ups.
- **API deviations** in [`docs/api-deviations.md`](./docs/api-deviations.md)
  — every P/A/R row from the compat matrix with reproductions.
- **macOS setup** in [`docs/macos-setup.md`](./docs/macos-setup.md) —
  entitlements, codesigning, gvproxy, network-mode tradeoffs.
- **Performance** numbers in [`docs/perf/`](./docs/perf/index.md) — published
  per revision, never borrowed from upstream.

## Compatibility

Per [`specs/21-api-compat-matrix.md`](./specs/21-api-compat-matrix.md), every
upstream Firecracker endpoint, field, and CLI flag has a documented status:
**F** (full parity), **P** (parity in shape, semantics differ), **A** (accepted
no-op with a one-time warning), or **R** (rejected with a stable
`fault_message`). The compat suite under
[`tests/firecracker-compat/`](./tests/firecracker-compat/) drives every row.

## License

Apache-2.0. See [LICENSE.md](./LICENSE.md).

Copyright 2025 Tyr Chen.
