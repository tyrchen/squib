# macOS setup

Squib needs an Apple-Silicon Mac running macOS 15+ ("Sequoia") and a codesigned
binary that carries the right entitlements. This document covers what to install,
how to sign, and which networking mode to pick for which use case.

## Prerequisites

- **Hardware**: Apple Silicon (M1/M2/M3/M4). HVF is mandatory; rosetta is not
  enough.
- **OS**: macOS 15.0 or newer. The HVF GIC API (`hv_gic_*`) lands in macOS 15;
  D2 / D18 cover the rationale for cutting earlier macOS support.
- **Rust**: 1.95 (pinned in `rust-toolchain.toml`).
- **Toolchain**: clang from Xcode CLI tools (`xcode-select --install`).
- **Optional**: `jq`, `python3` for the bench-publish / pkg-builder flows.

## Codesigning + entitlements

Per [`specs/70-security.md § 9`](../specs/70-security.md#9-code-signing--entitlements)
and [D17](../specs/99-key-decisions.md#d17-entitlement-set-claimed-only-the-self-claimable-hypervisor-default-restricted-vmnetworking-only-on-the-bridged-build):

- The default squib binary carries only `com.apple.security.hypervisor`. This
  entitlement is self-claimable — ad-hoc codesigning works for local
  development; Apple does not gate it.
- The bridged-mode binary carries `com.apple.vm.networking` *additionally*. That
  entitlement is restricted; you need a Developer ID with the entitlement
  approval from Apple. Bridged mode is gated behind the `bridged` cargo
  feature.

### Building locally

```bash
make sign        # ad-hoc-sign squib + squib-jail with com.apple.security.hypervisor
make verify      # codesign --display + --verify on both
```

The `Makefile` codesigns from `apps/squib/squib.entitlements`. To build the
bridged variant:

```bash
cargo build --release --features bridged --bin squib
make sign-bridged
```

### Releasing

```bash
SIGN_ID=<DeveloperID-hash> make sign-all
SIGN_ID=<DeveloperID-installer-hash> make pkg
SIGN_ID=<DeveloperID-installer-hash> APPLE_ID=… APPLE_TEAM_ID=… APPLE_NOTARY_PASSWORD=… make notarize
```

`make notarize` produces a stapleable `.pkg` per the Phase 6 exit criterion.

### Test binaries

`cargo test` does not codesign test binaries. `make hvf-test` and
`make vmnet-test` build `--no-run`, codesign each test binary with the right
entitlement, then re-invoke `cargo test --include-ignored`. Every live HVF or
vmnet integration test is `#[ignore]`d so a vanilla `cargo test` passes
without the entitlement.

## Networking modes

Pick the mode that matches the intersection of (entitlement available, network
shape needed). See [`specs/30-networking.md`](../specs/30-networking.md) for
the full matrix.

| Mode | Entitlement | Use case | Throughput target | Status |
|------|-------------|----------|--------------------|--------|
| `shared` (default) | `com.apple.security.hypervisor` (self-claimable) | NAT'd guests with internet access. The most common pick. | ≥ 1 Gbit/s on Apple SSD | shipped |
| `host` | same | Host-only network; no internet. Useful for isolated test guests. | n/a | shipped |
| `bridged` | `com.apple.vm.networking` (restricted) | Guest gets an L2 address on the host's physical network. | ≥ 1 Gbit/s | shipped (separate signed build) |
| `userspace` | none | gvproxy-shaped userland network. Inner-dev-loop friendly; no entitlement-related friction. | ~300–400 Mbit/s | shipped |

### gvproxy

`--network=userspace` runs gvproxy as a child process. The Phase 4 / 6 work
covers vendoring the binary; pre-vendor, point squib at a system gvproxy:

```bash
brew install gvproxy   # if a tap publishes it
squib --network=userspace --gvproxy-path "$(which gvproxy)" …
```

A documented fallback path of `/usr/local/libexec/squib/gvproxy` is logged on
startup if `--gvproxy-path` is not set.

## HVF entitlement revocation

If `make sign` succeeds but `target/release/squib` exits with `EX_NOPERM` from
HVF, check:

1. `codesign --display --entitlements - target/release/squib` shows the
   entitlement is bound.
2. macOS isn't blocking the binary via the Endpoint Security framework
   (System Settings → Privacy & Security).
3. The binary's filesystem location is not in a Quarantine-attribute zone
   (`xattr -d com.apple.quarantine target/release/squib`).

## Cross-references

- ← Code-signing spec: [`specs/70-security.md § 9`](../specs/70-security.md#9-code-signing--entitlements).
- ← Networking spec: [`specs/30-networking.md`](../specs/30-networking.md).
- → API deviations: [`docs/api-deviations.md`](./api-deviations.md).
- → Performance: [`docs/perf/index.md`](./perf/index.md).
