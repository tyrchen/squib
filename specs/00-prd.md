---
title: squib — Product Requirements
type: prd
status: draft
last_updated: 2026-05-03
supersedes: squib-prd.md (flat layout)
---

# PRD — squib

Status: draft v1 · Owner: squib · Last updated: 2026-05-03

## 1. Problem

Lambda/Fargate-style backend engineers iterating on Firecracker workloads on a Mac laptop today have two bad options:

- Keep a Linux dev VM running on the Mac, paying ~30 s startup, ~2–4 GiB RAM, and SSH-tunnel friction. Every iteration crosses a hypervisor boundary the dev did not ask for.
- Use vfkit / lima / podman-machine, which do run Linux VMs natively on macOS but expose a *different* control-plane API. Configs, CLIs, and orchestrator integrations that work against Firecracker on a Linux fleet do not transfer.

Neither preserves the Firecracker contract — the OpenAPI surface, the JSON schema, the CLI flags, the snapshot wire format. Engineers building on top of Firecracker (firecracker-containerd, Flintlock, Kata, AWS Lambda Runtime Interface Emulator) cannot run their integration tests on a Mac without a separate code path for the macOS case.

## 2. Vision

Run Firecracker workloads natively on an Apple-Silicon Mac with the same OpenAPI, the same JSON config, the same CLI invocations. **Native HVF, aarch64 Linux guests, single 1.0 release with the full Firecracker-compatible feature set.**

```bash
# A Firecracker config file, unchanged, produces a working microVM on macOS.
$ squib --config-file vm.json
> [info] squib 1.0 listening on /run/firecracker.socket
$ firectl --firecracker-binary $(which squib) ...
> microVM running, /sbin/init exec at +320 ms
```

The wire surface is the contract. If `firectl`, `firecracker-go-sdk`, `firecracker-containerd`, and `weaveworks/ignite` all drive squib unmodified, we shipped the right thing.

## 3. Goals

| #   | Goal | Measure |
| --- | ---- | ------- |
| G1  | Firecracker API parity at 1.0 | ≥ 95% of upstream `docs/api_requests/` examples pass against squib unmodified |
| G2  | Boot performance suitable for an inner dev loop | p50 cold boot to `/sbin/init` ≤ 400 ms on M2 Pro / M3 |
| G3  | Memory overhead acceptable on a laptop | ≤ 15 MiB host overhead per microVM at idle |
| G4  | External orchestrator can target macOS | At least one of {firecracker-containerd, Flintlock} acknowledges squib as a supported target by 1.0+1 |
| G5  | Round-trip Full + Diff snapshots | Same-host save / restore produces a workload-equivalent VM; Diff snapshots use `hv_vm_protect` dirty tracking |
| G6  | Defence-in-depth on a developer laptop | `#![forbid(unsafe_code)]` outside `squib-hv` and `squib-net::sys`; every API field length-bounded; codesigned binary with hardened runtime |

## 4. Non-goals

1. **Production multi-tenant isolation.** Squib targets dev machines. Defence-in-depth is welcome; the threat model is not "an arbitrary tenant attacks the host."
2. **Bit-exact KVM register fidelity.** Snapshots taken on Linux/KVM aarch64 will *not* restore on macOS/HVF. Different sysreg sets, different timer state, different GIC representation. Squib aims for *workload-equivalence*, not *snapshot-binary-equivalence*. Cross-host memory-only restore is a stretch goal at most.
3. **x86_64 guest support.** Apple Silicon HVF is aarch64. x86 binaries inside the guest run via guest-side `binfmt_misc + qemu-user` — that is a guest-image concern, not a squib feature.
4. **Intel macOS.** Skipped entirely. The build does not target `x86_64-apple-darwin`.
5. **Replacing Containerization, Orbstack, Lima.** Different layer; we expose the Firecracker API specifically.
6. **Production-grade boot times of 125 ms.** That number is from Firecracker on Linux/KVM x86 with PVH and zero-page setup. On native HVF aarch64 with FDT-and-Image boot, the realistic floor is 250–400 ms. Squib publishes its own measured numbers.
7. **VZ.framework integration of any kind.** The earlier dual-backend draft is rejected. See [99-key-decisions.md § D1](./99-key-decisions.md#d1-hvf-only-no-vz).
8. **Userspace GICv3 emulation.** macOS 15 minimum, `hv_gic_*` only.
9. **virtio-PCI transport.** virtio-MMIO only; `--enable-pci` accepts and warns.
10. **macOS App Store distribution.** HVF is incompatible with the App Sandbox.

## 5. Users

| Audience | Use case | Success looks like |
|----------|----------|--------------------|
| Lambda / Fargate-style backend engineers | Run microVM workloads locally on Apple Silicon | `firectl` against squib works as against Firecracker on Linux Graviton |
| Platform / orchestrator authors (Kata, Flintlock, firecracker-containerd) | Drive squib via the same REST contract | Their integration tests run unchanged on macOS |
| Researchers / educators | Snapshot-based cold-start, microVM internals | Snapshots create and restore on squib |
| AWS Lambda RIE users | Local invocation of Lambda functions | Configs that work in Firecracker work in squib |

Anti-persona: an Intel-Mac developer who needs x86 guests. We do not serve them; vfkit + Rosetta is their option.

## 6. Compatibility scope (the contract)

Squib must accept and produce the same wire surface as Firecracker. Day-1 commitments:

1. **HTTP API over a Unix socket** — same paths, JSON shapes, status codes, `{"fault_message": "..."}` body, `Server: Firecracker API` header, identical pre-boot vs post-boot admissibility.
2. **CLI flags** — every Firecracker flag parses; every Linux-only flag (`--seccomp-filter`, `--no-seccomp`, `--enable-pci`, `--cpu-template C3|T2|...`) accepts and either applies a documented best-effort or warns and no-ops.
3. **Static config file** — `--config-file <path>` consuming the same kebab-case JSON document, with `boot-source` mandatory.
4. **MMDS** — link-local 169.254.169.254, V1 and V2 (IMDSv2 token), JSON Pointer traversal.
5. **vsock** — UDS-multiplex protocol exact: host-initiated `CONNECT <port>\n` → `OK <port>\n`, guest-initiated `<uds_path>_<port>` listener.
6. **Logger / Metrics** — same JSON metric field names, same rate-limited log-line format, file or FIFO targets.
7. **Snapshot file format** — bit-identical outer envelope to upstream Firecracker: `bitcode::serialize(Snapshot{header: SnapshotHdr{magic, version: semver::Version}, data: MicrovmState})` followed by an 8-byte LE CRC-64 ISO 3309. Magic `0x07101984_AAAA_0000` (aarch64) lives **inside** the bitcode envelope, not as a raw byte prefix. Memory file layout (full + sparse-of-dirty) matches upstream. `MicrovmState` *contents* are HVF-shaped (different sysreg subset, different GIC-state blob), explicitly documented as squib-1.0 not cross-VMM-compatible. See [10-data-model.md § 6.1](./10-data-model.md#61-state-file-idsnap) and [99-key-decisions.md § D5](./99-key-decisions.md#d5-snapshot-encoding-bitcode-encoded-snapshotmicrovmstate-not-raw-byte-prefixes).
8. **Boot source** — `kernel_image_path`, `initrd_path`, `boot_args` honored. Linux raw `Image`, `Image.gz`, PE-formatted Image — all decompressed/loaded at config-load time.
9. **Dirty page tracking** — `track_dirty_pages: true` honored via `hv_vm_protect` write-protect-and-fault scheme; Diff snapshots work.
10. **PSCI SMP** — multi-vCPU guests boot via PSCI on HVC; CPU_ON / CPU_OFF / SYSTEM_OFF / SYSTEM_RESET implemented.

The full per-field bookkeeping lives in [21-api-compat-matrix.md](./21-api-compat-matrix.md).

## 7. Permitted deviations (each documented in API docs)

We accept that some Firecracker semantics do not translate. For each: (a) accept the request, (b) apply the closest macOS approximation, (c) emit a startup warning, (d) document. None are deferred — all decided on day-1.

| API element | Firecracker meaning | Squib behavior |
|-------------|---------------------|----------------|
| `network-interfaces.host_dev_name` | Linux TAP device name | Mapped deterministically to a vmnet handle (`squib-tap-<iface_id>`); literal Linux TAP names are an opaque label |
| `cpu_template: "C3"\|"T2"\|"T2A"\|"T2CL"\|"T2S"` | x86 CPUID/MSR overrides | Accept-and-warn (these are x86 templates; we run aarch64) |
| `cpu_template: "V1N1"` | aarch64 sysreg overrides | Best-effort applied via `hv_vcpu_set_sys_reg` for the registers we control |
| `PUT /cpu-config` aarch64 `reg_modifiers`, `vcpu_features` | Custom aarch64 CPU template | Best-effort applied; warn on unsupported registers |
| `PUT /cpu-config` x86 `cpuid_modifiers`, `msr_modifiers` | Custom x86 CPU template | Accept-and-warn |
| `--seccomp-filter`, `--no-seccomp` | Linux BPF | Accept-and-warn (no Linux BPF on macOS) |
| `--enable-pci` | virtio-PCI transport | Accept-and-warn (squib uses virtio-MMIO) |
| `huge_pages: "2M"` | hugetlbfs | Accept-and-warn (Darwin manages page sizes) |
| `clock_realtime` (snapshot/load) | KVM-clock realignment | Accept-and-ignore (x86-only field) |
| `smt: true` | x86 hyperthreading | Reject with `fault_message` (Apple Silicon has no SMT) |
| `jailer` binary | chroot/cgroup/seccomp | Replaced by `squib-jail` shim with the same flags; applies safe subset (chroot, ulimits, uid/gid drop, optional `sandbox-exec` profile); accept-and-warn on cgroups/netns |

The principle: never break a launcher. Reject only when the configuration would silently produce wrong behavior; otherwise accept-and-warn.

## 8. Hard requirements (1.0)

- **R1. Wire-level compatibility.** The 25 most common Firecracker API call patterns from `tests/integration_tests/` exercise squib without modification. The compat suite is gating CI.
- **R2. Static config file.** `firecracker --config-file vm.json` runs verbatim against squib (binary renamed to `squib`).
- **R3. CLI launcher integration.** `firectl`, `firecracker-go-sdk`, `firecracker-containerd`, `weaveworks/ignite` clients drive squib without code changes.
- **R4. Snapshot round-trip.** Squib snapshots a running microVM, kills it, reloads, resumes. Both Full and Diff snapshots work. **Cross-host (KVM↔HVF) is not required.**
- **R5. PSCI SMP.** Multi-vCPU guests up to host physical core count boot and run.
- **R6. Boot performance.** From `PUT /actions {InstanceStart}` to `/sbin/init` exec: **p50 ≤ 400 ms** on M2 Pro / M3 with a Firecracker-tuned aarch64 vmlinux + busybox initrd. Published numbers, not borrowed.
- **R7. Memory overhead.** ≤ 15 MiB host overhead per microVM at idle.
- **R8. Stability.** No crashes from external misuse — every error path returns a 4xx with a `fault_message`. `cargo clippy -- -D warnings` clean. `#![forbid(unsafe_code)]` outside `squib-hv` and `squib-net::sys`.
- **R9. macOS 15 Sequoia or later.** Pinned floor, justified by reliance on `hv_gic_*` for in-kernel GICv3 (avoiding ~3K LoC of userspace GIC emulation).
- **R10. Apple Silicon arm64 native.** No Rosetta translation, no x86_64 build target. `aarch64-apple-darwin` only.
- **R11. Code-signed and entitlement-bearing.** Default binary ships with `com.apple.security.hypervisor` only (sufficient for HVF, vmnet shared-mode NAT, vmnet host-only, and gvproxy userspace mode). A separately-signed bridged-enabled build embeds `com.apple.vm.networking` (restricted; gated on Apple DTS approval). See [99-key-decisions.md § D17](./99-key-decisions.md#d17-vmnet-entitlement-clarification). CI runs against an ad-hoc-signed local build; releases are notarized.

## 9. Soft requirements

- **S1. Userspace networking fallback.** A `--network=userspace` mode bundling `gvproxy` for users who cannot run vmnet at all (e.g. heavily locked-down corporate machines that strip `com.apple.security.hypervisor`-permitted vmnet calls, or users who want a fully userspace TCP/IP path). Note: NAT via `--network=shared` does **not** require any extra entitlement beyond `com.apple.security.hypervisor` (D17), so userspace mode is genuinely a fallback rather than a default. Stretch in 1.0.
- **S2. Compat coverage report.** Each row in [21-api-compat-matrix.md](./21-api-compat-matrix.md) has a passing test or a documented skip. Generated and published with each release.
- **S3. Single static binary.** `squib` and `squib-jail` ship as code-signed `aarch64-apple-darwin` binaries with embedded entitlements.
- **S4. Homebrew formula.** Available alongside direct `.pkg` download.
- **S5. virtio-vsock TSI.** libkrun's Transparent Socket Impersonation pattern, useful for Lambda-shaped guests that open AF_VSOCK sockets inside.

## 10. Non-functional requirements

- **NFR1. License-aware reuse.** Code ported from upstream Apache-2.0 projects (Firecracker, libkrun, cloud-hypervisor, alioth) carries proper attribution in `NOTICE`. No GPL/LGPL code in the binary.
- **NFR2. Test discipline.** Real integration tests against a real HVF. No mocked vCPU runs in the compat suite. CI includes at least two macOS versions (15 Sequoia and 26 Tahoe).
- **NFR3. Benchmarks from week 1.** Boot time and memory overhead benchmarked from the first end-to-end boot, not retrofitted.

## 11. Success metrics

- **Compatibility coverage**: % of Firecracker's API examples passing against squib unmodified. Target ≥ 95% at 1.0.
- **Boot time p50**: ≤ 400 ms to `/sbin/init` (R6).
- **Memory overhead p50**: ≤ 15 MiB.
- **External adoption signal**: at least one orchestrator (firecracker-containerd or Flintlock) acknowledging squib as a supported macOS target by 1.0+1.

## 12. Naming conventions (binding)

These names are the contract for the rest of the spec set; renames are expensive after 1.0.

- **Workspace name**: `squib` (lowercase). Crate prefix `squib-`. Binary names `squib`, `squib-jail`.
- **HTTP API identity**: `Server: Firecracker API` header preserved verbatim (consumers sniff it).
- **OpenAPI version reported**: `1.16-firecracker-compat (squib X.Y.Z)`.
- **Snapshot magic-id**: `0x07101984_AAAA_0000` (aarch64), matching upstream.
- **vmnet handle naming**: `squib-tap-<iface_id>` for `host_dev_name` mapping.
- **Squib JSON extension key**: `"squib": { ... }` at the top level of the static config; upstream Firecracker silently ignores it, preserving file portability.
- **Crate layout**: see [61-crates-and-features.md](./61-crates-and-features.md).
- **Numbered spec files**: this directory uses `NN-name.md` numbering as the build order — see [index.md](./index.md).

## 13. Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `applevisor` crate goes stale mid-cycle | Medium | Have to fork or hand-bind | Isolate inside `squib-hv`; <50 call sites; alioth-style trait wrapper makes swap mechanical |
| `hv_vm_protect` TLB shootdown cost makes dirty tracking unworkable | Medium | Diff snapshots fail R4 | 2 MiB granularity by default, 4 KiB only for hot regions; perf-tune in week 1 of dirty-tracking work |
| `com.apple.vm.networking` denied for bridged | Certain | No bridged mode | Default to NAT; ship gvproxy fallback; document |
| Mach exception port edge cases break LLDB | Medium | Dev UX pain | Save and forward to prior handlers; specifically test with LLDB attach |
| HVF behavioral skew between macOS 15 / 26 | Medium | Subtle bugs | CI matrix covers both; pin minimum SDK in `MACOSX_DEPLOYMENT_TARGET` |
| Firecracker API drifts faster than we track | Medium | Compat gap grows | Pin to 1.16 minor; bump in lockstep each cycle |
| HVF boot time floor exceeds 400 ms target | Medium | R6 missed | Hand-tune kernel config; pre-decompress kernels; HVC console; minimal initramfs |
| Notarization stalls release | Low | Release delay | Notarize post-merge async, not pre-tag |

## 14. Approach summary

Single Rust workspace, single binary (`squib`), one supplementary binary (`squib-jail`). HVF backend via the `applevisor` crate. aarch64-only architecture code. Devices ported from upstream Apache-2.0 projects (libkrun for HVF specifics and TSI vsock; cloud-hypervisor for virtio-block/net/balloon/rng; Firecracker for MMDS/dumbo and the OpenAPI server). Snapshot serialization via `bitcode + serde` mirroring Firecracker's outer-container shape. Min macOS = 15 Sequoia; recommended = macOS 26 Tahoe.

See [11-runtime-core.md](./11-runtime-core.md) for the trait spine, [12-hvf-backend.md](./12-hvf-backend.md) for the HVF implementation, [90-roadmap.md](./90-roadmap.md) for the milestone shape, and [91-impl-plan.md](./91-impl-plan.md) for the engineer-facing build sequence.
