# Developer guide

You want to hack on squib. This guide covers the workspace layout, the
build/test/lint loop, the codesigning dance HVF imposes on test binaries, and
where to plug in when you're adding a feature. It assumes you have already
read the [user guide](./user-guide.md) and have a working `make sign` build.

中文版：[dev-guide.zh-CN.md](./dev-guide.zh-CN.md)。

## 1. Mental model

Squib is one Rust workspace, one binary (`squib`), one supplementary binary
(`squib-jail`), and ~16 library crates organized by responsibility. The crate
graph is a strict DAG with `squib-core` (zero workspace deps) at the bottom
and `apps/squib-cli` at the top.

The two load-bearing rules:

1. **`unsafe` lives in two crates only**: `squib-hv` (the `applevisor` /
   `hv_*` boundary) and `squib-net::sys` (the hand-rolled `vmnet` FFI). Every
   other crate carries `#![forbid(unsafe_code)]`. CI greps for it.
2. **Specs are the source of truth.** Every load-bearing decision has a
   D-record in [`specs/99-key-decisions.md`](../specs/99-key-decisions.md).
   Every wire shape, error string, and CLI flag has a row in
   [`specs/21-api-compat-matrix.md`](../specs/21-api-compat-matrix.md). When
   the code and the spec disagree, fix one of them — never both, never
   neither.

## 2. Workspace layout

```
apps/
  squib/           CLI binary, codesigned with com.apple.security.hypervisor
  squib-jail/      Drop-in jailer shim, libc-only, no Apple entitlements

crates/
  core             squib-core      portable types & traits, zero deps
  api              squib-api       axum-on-UDS Firecracker API + JSON config loader
  hv               squib-hv        HVF binding via applevisor (unsafe boundary #1)
  arch             squib-arch      aarch64 layout, vCPU init regs, sysreg list, ESR_EL2, PSCI
  fdt              squib-fdt       FDT builder via vm-fdt
  loader           squib-loader    kernel loader: Image / Image.gz / Image.zst / PE
  bus              squib-bus       MMIO bus + BusDevice trait
  virtio           squib-virtio    virtio-MMIO transport + device sub-crates
  gic              squib-gic       in-kernel GICv3 wrapper (hv_gic_*)
  mmds             squib-mmds      ported dumbo + mmds packet interception
  net              squib-net       vmnet integration + gvproxy embed (unsafe boundary #2)
  snapshot         squib-snapshot  bitcode + serde state file, sparse memory, postcopy
  vmm              squib-vmm       VMM core: builder, vCPU thread, device manager, event loop
  host             squib-host      Mach-exception pager, signal handling, child-process management
  legacy           squib-legacy    PL011, RTC, boot timer

tests/
  firecracker-compat              one row in 21-api-compat-matrix per file

examples/
  reference-vm                    busybox-on-initramfs end-to-end demo
```

Detailed responsibilities and the full dependency graph live in
[`specs/61-crates-and-features.md`](../specs/61-crates-and-features.md).

## 3. Build, test, lint

Everything is wired through the Makefile. There are no shell scripts you need
to remember.

```bash
make build          # cargo build --workspace --all-targets
make build-release  # cargo build --release --bin squib --bin squib-jail
make test           # cargo test --workspace --all-features (fast tests only)
make lint           # cargo clippy --workspace --all-targets --all-features -- -D warnings
make fmt            # cargo +nightly fmt --all
make fmt-check      # check-only variant for CI
make audit          # cargo audit
make deny           # cargo deny check
make doc            # cargo doc --workspace --no-deps
```

Before sending a PR, run `make lint && make fmt-check && make test`. CI runs
the same trio plus `make compat-test`, plus the live HVF / vmnet suites on
self-hosted Apple Silicon runners.

The clippy bar is `pedantic` (warn) plus `-D warnings`. Boundary modules
(`squib-api`, controller dispatch) layer additional denies for
`unwrap_used`, `expect_used`, `indexing_slicing`, and `panic`. If you find
yourself reaching for `.unwrap()` outside a `#[cfg(test)]`, stop and use `?`
or an explicit match.

## 4. Codesigning for HVF tests

HVF refuses to initialise without `com.apple.security.hypervisor`. `cargo
test` does **not** codesign test binaries, so any test that talks to HVF or
`vmnet.framework` directly fails until the binary is signed. The pattern:

```bash
make hvf-test       # builds --no-run, codesigns each test binary, re-runs with --include-ignored
make vmnet-test     # same shape for the vmnet FFI tests
make demo           # boots the reference VM end-to-end
```

The `#[ignore]` annotation on every live HVF / vmnet test is deliberate: a
plain `cargo test` must pass for contributors who haven't run `make sign`
yet. The Makefile targets above flip `--include-ignored` after signing.

For a CI release build, `SIGN_ID=<DeveloperID-hash> make sign-all` produces
hardened-runtime binaries; `make pkg` and `make notarize` package and notarize
them. See [`macos-setup.md`](./macos-setup.md) for the full sequence.

## 5. The trait spine

If you're adding hypervisor functionality, the entry points are in
`squib-core`:

- `HypervisorBackend` — the factory; one impl, in `squib-hv`.
- `Vm` — owns memory regions, IRQ chip handle, snapshot interface.
- `Vcpu` — per-vCPU; runs to next `VmExit`.
- `VmExit` — the algebra (`Mmio`, `Hypercall`, `PsciCall`, `Shutdown`, …)
  that the VMM event loop dispatches against.

The HVF impl lives in `squib-hv`. Anything that needs `applevisor` or `hv_*`
goes there; everything else stays under `#![forbid(unsafe_code)]`. The trait
boundary is intentional — when `applevisor` goes stale we want a mechanical
swap, not a rewrite.

Read [`specs/11-runtime-core.md`](../specs/11-runtime-core.md) before
touching this layer.

## 6. Adding a feature

The dependency-ordered build plan is
[`specs/91-impl-plan.md`](../specs/91-impl-plan.md). Most feature work falls
into one of these buckets:

| You want to | Touch | Spec |
|-------------|-------|------|
| Add an API endpoint or change a request shape | `squib-api`, the controller, the compat suite | [20-firecracker-api.md](../specs/20-firecracker-api.md), [21-api-compat-matrix.md](../specs/21-api-compat-matrix.md) |
| Add a virtio device | `squib-virtio/<device>`, the device manager | [14-virtio-and-devices.md](../specs/14-virtio-and-devices.md) |
| Tweak the boot path | `squib-arch`, `squib-fdt`, `squib-loader`, `squib-vmm` | [13-arch-and-boot.md](../specs/13-arch-and-boot.md) |
| Touch HVF | `squib-hv` only | [12-hvf-backend.md](../specs/12-hvf-backend.md) |
| Change networking | `squib-net`, the device manager | [30-networking.md](../specs/30-networking.md) |
| Touch snapshots | `squib-snapshot`, `squib-host` (postcopy) | [16-snapshots.md](../specs/16-snapshots.md) |
| Add a CLI flag | `apps/squib-cli/src/cli.rs`, the compat matrix | [50-cli.md](../specs/50-cli.md) |

The pattern for any change that can affect a wire shape:

1. Add or update the row in `specs/21-api-compat-matrix.md`.
2. Add a test in `tests/firecracker-compat/` exercising the new shape.
3. Implement.
4. Run `make compat-test` until green.

If the change is load-bearing — a new error code, a new format, a new
behaviour you want to lock down against future drift — open a D-record in
`specs/99-key-decisions.md`. Never edit existing D-records in place; supersede
with a new D-id and a back-reference.

## 7. Test surface

| Layer | Where | When it runs |
|-------|-------|--------------|
| Unit tests | inline `#[cfg(test)] mod tests` per crate | every `cargo test` |
| Integration tests | `tests/` per crate | every `cargo test` |
| Compat suite | `tests/firecracker-compat/` (one file per matrix row) | `make compat-test`, every CI run |
| Live HVF | `crates/{hv,vmm}/tests/`, `#[ignore]`d | `make hvf-test` (signed) |
| Live vmnet | `crates/net/tests/`, `#[ignore]`d | `make vmnet-test` (signed) |
| End-to-end demo | `crates/vmm/tests/linux_boot_smoke.rs` | `make demo` |
| Snapshot smoke | `make snapshot-smoke` | manual / release gate |
| Cross-FS rejection | `make snapshot-cross-fs-test` | manual; needs `hdiutil` |
| SDK soak | `tools/soak/{firectl,firecracker-go-sdk,firecracker-containerd}/run.sh` | `make soak`; skips when binary missing |
| Bench harness | `cargo bench --features bench` | `make bench-publish` per release |

Property-based tests via `proptest` are welcome where invariants matter (FDT
builder, ESR_EL2 decoder, MMDS JSON Pointer). Mocking is a last resort —
real implementations beat mocks unless the real one is slow or non-Send.

## 8. Style highlights

The full style guide is in `CLAUDE.md` at the project root. The points that
trip people up most often:

- **No `unwrap` / `expect` outside `#[cfg(test)]`.** Use `?` or an explicit
  match. The boundary modules deny it at clippy level.
- **No `unsafe` outside `squib-hv` and `squib-net::sys`.** Every other crate
  has `#![forbid(unsafe_code)]`. If you find yourself wanting `unsafe` for
  performance, profile first — it's almost never the bottleneck.
- **Validate at the boundary.** Length-bound every `String`/`&str` derived
  from external input *in bytes*, not chars. Use the `validator` crate for
  struct-level validation. Newtype every domain primitive.
- **Native `async fn` in traits, except for object safety.** When a trait
  needs `Arc<dyn Trait>`, use `async-trait` and document why in the
  module-level doc.
- **`bytes::Bytes` for payloads, not `Vec<u8>`.** Cloning a `Bytes` is a
  refcount bump; cloning a `Vec` is a copy.
- **No comments that explain *what* the code does.** Only `// SAFETY: …`,
  `// Why: …`, hidden invariants, and workarounds for specific bugs. Names
  carry the *what*.
- **No deprecation cycles.** When something is dead, delete it. We do not
  do soft removal.

## 9. Specs and research

Two corpora to pull from:

- **`specs/`** is the design contract. Read top-to-bottom for the build order
  ([00-prd](../specs/00-prd.md) → [10-data-model](../specs/10-data-model.md) →
  [11-runtime-core](../specs/11-runtime-core.md) →
  [12-hvf-backend](../specs/12-hvf-backend.md) → …). Stakeholders read
  [00-prd](../specs/00-prd.md) and [90-roadmap](../specs/90-roadmap.md).
  Engineers building a phase read [91-impl-plan](../specs/91-impl-plan.md).
- **`docs/research/`** is prior-art memos: HVF deep dive, aarch64 guest
  stack, performance and snapshots, the macOS hypervisor ecosystem. When in
  doubt about a low-level detail, the memo cites the paper, header, or
  Apple doc.

The full reading order for the spec set is at
[`specs/index.md`](../specs/index.md). For research,
[`docs/research/index.md`](./research/index.md).

## 10. Releasing

```bash
make release                                    # cargo release tag + push
SIGN_ID=<DeveloperID-hash> make sign-all        # codesign release binaries
SIGN_ID=<DeveloperID-installer-hash> make pkg   # build the .pkg
APPLE_ID=… APPLE_TEAM_ID=… APPLE_NOTARY_PASSWORD=… make notarize
```

`make notarize` produces a stapleable `.pkg` per the Phase 6 exit criterion.
The notarytool submission is on a post-merge async lane (see
`.github/workflows/notarize.yml`); tags are not gated on it so a slow
notarytool round trip cannot stall a release.

After tagging, `make bench-publish` re-runs the criterion harness and stashes
the JSON + HTML under `docs/perf/<git-sha>/` per
[`specs/71-performance-budgets.md § 7`](../specs/71-performance-budgets.md#7-publication).
Published numbers are never borrowed from upstream — every claim in
`docs/perf/` came from the harness on a known commit.

## 11. Where to go next

- [`specs/91-impl-plan.md`](../specs/91-impl-plan.md) — engineer-facing
  build sequence with effort estimates and exit criteria.
- [`specs/93-improvements-review.md`](../specs/93-improvements-review.md) —
  the deferred-findings backlog. Pick something off it if you're looking
  for a starter task.
- [`specs/72-testing-strategy.md`](../specs/72-testing-strategy.md) — the
  test pyramid and the upstream-tracking discipline.
- [`specs/70-security.md`](../specs/70-security.md) — the threat model,
  unsafe boundaries, secrets handling, supply-chain pinning.
