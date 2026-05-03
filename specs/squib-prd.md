---
title: squib — Product Requirements
type: prd
status: draft
last_updated: 2026-05-03
supersedes: prior VZ-default draft (2026-05-03 morning)
---

# squib — Product Requirements

## Mission

Run Firecracker workloads natively on an Apple-Silicon Mac. Same OpenAPI, same JSON config, same CLI invocations. **Native HVF, aarch64 Linux guests, single 1.0 release with the full Firecracker-compatible feature set.**

## What changed

The earlier draft of this PRD proposed a VZ-default backend with HVF as a later opt-in. That direction is rejected. New constraints:

1. **Apple Silicon only.** No Intel macOS support, ever. Apple has frozen Intel Mac framework features; the user base for "Firecracker-on-Mac" is Apple Silicon developers.
2. **HVF only.** No VZ, no dual-backend, no `--hypervisor` flag. Performance is the explicit priority and HVF gives us the per-µs control we need; VZ's closed device model would make CPU templates, snapshot dirty-page tracking, and several Firecracker semantics impossible.
3. **Day-1 parity.** The first public release is 1.0 and contains the full Firecracker-compatible API surface with snapshots, vsock, MMDS, balloon, entropy, dirty tracking, and PSCI/GIC SMP — not a phased rollout.
4. **aarch64 Linux guests only.** Apple Silicon HVF is aarch64. x86_64 guest emulation (Rosetta-share, QEMU TCG) is out of scope — users with x86_64 workloads recompile for arm64 (the same migration AWS Lambda customers do for Graviton) or run x86 binaries inside the guest via guest-side `binfmt_misc` + `qemu-user`.

## Why

Firecracker powers AWS Lambda and Fargate. Internal teams running Lambda-style workloads want to:

- Iterate on microVM images and configurations on a Mac laptop, not in a Linux VM running on the Mac.
- Run integration tests against the same VMM their Linux fleets use, modulo arch (Graviton parity).
- Prototype snapshot-based fork/clone designs without an EC2 hop.
- Get a sub-second feedback loop instead of "spin up a Linux dev VM."

Today they either keep a Linux dev VM running (cost: ~30 s startup, ~2-4 GiB RAM, SSH-tunnel friction) or use vfkit/lima/podman-machine (which run Linux VMs but with **a different API**, so configs and tooling don't transfer). Neither preserves the Firecracker contract.

## Non-goals

1. **Production multi-tenant isolation.** Squib is for dev machines. Defence-in-depth is welcome; the threat model is not "an arbitrary tenant attacks the host."
2. **Bit-exact KVM register fidelity.** Snapshots taken on Linux/KVM aarch64 will *not* restore on macOS/HVF — different sysreg sets, different timer state, different GIC state representation. Squib aims for *workload-equivalence*, not *snapshot-binary-equivalence*. Cross-host memory-only restore is a stretch goal at most.
3. **x86_64 guest support.** Apple Silicon HVF is aarch64. x86 binaries inside the guest run via guest-side `binfmt_misc + qemu-user` — that's a guest-image concern, not a squib feature.
4. **Intel macOS.** Skipped entirely. The build does not target `x86_64-apple-darwin`.
5. **Replacing Containerization, Orbstack, Lima.** Different layer; we expose the Firecracker API specifically.
6. **Production-grade boot times of 125 ms.** That's a number measured for Firecracker on Linux/KVM x86, with PVH boot and zero-page setup. On native HVF aarch64 with FDT-and-Image boot, the realistic floor is 250–400 ms. Squib publishes its own measured numbers; it does not borrow Firecracker's.

## Audiences

| Audience | Use case | Success looks like |
|----------|----------|--------------------|
| Lambda/Fargate-style backend engineers | Run microVM workloads locally on Apple Silicon | `firectl` against squib works as against Firecracker on Linux Graviton |
| Platform/orchestrator authors (Kata, Flintlock, firecracker-containerd) | Drive squib via the same REST contract | Integration tests of those projects run unchanged on macOS |
| Researchers / educators | Snapshot-based cold-start, microVM internals | Snapshots create and restore on squib |
| AWS Lambda Runtime Interface Emulator users | Local invocation of Lambda functions | Configs that work in Firecracker work in squib |

## Compatibility scope (the contract)

Squib must accept and produce the same wire surface Firecracker does. See `docs/research/firecracker-api-surface.md` for the catalogue and `specs/squib-api-compat-design.md` for the per-field bookkeeping. Day-1 commitments:

1. **HTTP API over a Unix socket** — same paths, same JSON shapes, same status codes, `{"fault_message": "..."}` error body, `Server: Firecracker API` header, identical pre-boot vs post-boot admissibility.
2. **CLI flags** — every Firecracker flag parses; every Linux-only flag (`--seccomp-filter`, `--no-seccomp`, `--enable-pci`, `--cpu-template C3|T2|...`) accepts and either applies a documented best-effort or warns and no-ops.
3. **Static config file** — `--config-file <path>` consuming the same kebab-case JSON document, with `boot-source` mandatory.
4. **MMDS** — link-local 169.254.169.254, V1 and V2 (IMDSv2 token), JSON Pointer traversal.
5. **vsock** — UDS-multiplex protocol exact: host-initiated `CONNECT <port>\n` → `OK <port>\n`, guest-initiated `<uds_path>_<port>` listener.
6. **Logger / Metrics** — same JSON metric field names, same rate-limited log-line format, file or FIFO targets.
7. **Snapshot file format** — same magic-id (`0x07101984_AAAA_0000` aarch64), same outer container shape (magic, version, state-blob, crc), same memory-file layout (full + sparse-of-dirty). State blob contents are HVF-shaped (different sysreg subset), explicitly documented as squib-1.0 not cross-VMM-compatible.
8. **Boot source** — `kernel_image_path`, `initrd_path`, `boot_args` honored. Linux raw `Image`, `Image.gz`, PE-formatted Image — all decompressed/loaded at config-load time.
9. **Dirty page tracking** — `track_dirty_pages: true` honored via `hv_vm_protect` write-protect-and-fault scheme; Diff snapshots work.
10. **PSCI SMP** — multi-vCPU guests boot via PSCI on HVC; CPU_ON / CPU_OFF / SYSTEM_OFF / SYSTEM_RESET implemented.

## Permitted deviations (each documented in API docs)

We accept that some Firecracker semantics do not translate. For each: (a) accept the request, (b) apply the closest macOS approximation, (c) emit a startup warning, (d) document. None of these are deferred — all are decided on day-1.

| API element | Firecracker meaning | Squib behavior |
|-------------|---------------------|----------------|
| `network-interfaces.host_dev_name` | Linux TAP device name | Mapped deterministically to a vmnet handle (`squib-tap-<iface_id>`); literal Linux TAP names are an opaque label |
| `cpu_template: "C3"|"T2"|"T2A"|"T2CL"|"T2S"` | x86 CPUID/MSR overrides | Accept-and-warn (these are x86 templates; we run aarch64) |
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

## Hard requirements (1.0)

- **R1. Wire-level compatibility.** The 25 most common Firecracker API call patterns from `tests/integration_tests/` exercise squib without modification. The compat suite is gating CI.
- **R2. Static config file.** `firecracker --config-file vm.json` runs verbatim against squib (binary renamed to `squib`).
- **R3. CLI launcher integration.** `firectl`, `firecracker-go-sdk`, `firecracker-containerd`, `weaveworks/ignite` clients drive squib without code changes.
- **R4. Snapshot round-trip.** Squib snapshots a running microVM, kills it, reloads, resumes. Both Full and Diff snapshots work. **Cross-host (KVM↔HVF) is not required.**
- **R5. PSCI SMP.** Multi-vCPU guests up to host physical core count boot and run.
- **R6. Boot performance.** From `PUT /actions {InstanceStart}` to `/sbin/init` exec: **p50 ≤ 400 ms** on M2 Pro / M3 with a Firecracker-tuned aarch64 vmlinux + busybox initrd. Published numbers, not borrowed.
- **R7. Memory overhead.** ≤ 15 MiB host overhead per microVM at idle.
- **R8. Stability.** No crashes from external misuse — every error path returns a 4xx with a `fault_message`. `cargo clippy -- -D warnings` clean. `#![forbid(unsafe_code)]` outside `squib-hv`.
- **R9. macOS 15 Sequoia or later.** Pinned floor, justified by reliance on `hv_gic_*` for in-kernel GICv3 (avoiding ~3K LoC of userspace GIC emulation).
- **R10. Apple Silicon arm64 native.** No Rosetta translation, no x86_64 build target. `aarch64-apple-darwin` only.
- **R11. Code-signed and entitlement-bearing.** Binary ships with `com.apple.security.hypervisor` and `com.apple.vm.networking` entitlements; CI runs against an ad-hoc-signed local build, releases are notarized.

## Soft requirements

- **S1. Userspace networking fallback.** A `--network=userspace` mode bundling `gvproxy` so users with no `com.apple.vm.networking` entitlement and no admin rights still get NAT. Ships as a stretch in 1.0.
- **S2. Compat coverage report.** Each row in `squib-api-compat-design.md` has a passing test or a documented skip. Generated and published with each release.
- **S3. Single static binary.** `squib` and `squib-jail` ship as code-signed `aarch64-apple-darwin` binaries with embedded entitlements.
- **S4. Homebrew formula.** Available alongside direct `.pkg` download.
- **S5. virtio-vsock TSI.** libkrun's Transparent Socket Impersonation pattern, useful for Lambda-shaped guests that open AF_VSOCK sockets inside.

## Non-functional requirements

- **NFR1. License-aware reuse.** Code ported from upstream Apache-2.0 projects (Firecracker, libkrun, cloud-hypervisor, alioth) carries proper attribution in `NOTICE`. No GPL/LGPL code in the binary.
- **NFR2. Test discipline.** Real integration tests against a real HVF. No mocked vCPU runs in the compat suite. CI includes at least two macOS versions (15 Sequoia and 26 Tahoe).
- **NFR3. Benchmarks from week 1.** Boot time and memory overhead benchmarked from the first end-to-end boot, not retrofitted.

## Success metrics

- **Compatibility coverage**: % of Firecracker's API examples (`docs/api_requests/`) that pass against squib unmodified. Target ≥ 95% at 1.0.
- **Boot time p50**: ≤ 400 ms to `/sbin/init` (R6).
- **Memory overhead p50**: ≤ 15 MiB.
- **External adoption signal**: at least one orchestrator (firecracker-containerd or Flintlock) acknowledging squib as a supported macOS target by 1.0+1.

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `applevisor` crate goes stale mid-cycle | Medium | Have to fork or hand-bind | Isolate `applevisor` types inside `squib-hv`; <50 call sites; alioth-style trait wrapper makes swap mechanical |
| `hv_vm_protect` TLB shootdown cost makes dirty tracking unworkable | Medium | Diff snapshots fail R4 | Engineer for 2 MiB granularity by default, 4 KiB only for hot regions; perf-tune in week 1 of dirty-tracking work |
| `com.apple.vm.networking` denied for bridged | Certain | No bridged mode | Default to NAT; ship gvproxy fallback; document |
| Mach exception port edge cases break LLDB | Medium | Dev UX pain | Save and forward to prior handlers; specifically test with LLDB attach |
| HVF behavioral skew between macOS 15 / 26 | Medium | Subtle bugs | CI matrix covers both; pin minimum SDK in `MACOSX_DEPLOYMENT_TARGET` |
| Firecracker API drifts faster than we track | Medium | Compat gap grows | Pin to 1.16 minor; bump in lockstep each cycle |
| HVF boot time floor exceeds 400 ms target | Medium | R6 missed | Hand-tune kernel config; pre-decompress kernels; HVC console; minimal initramfs |
| Notarization stalls release | Low | Release delay | Notarize post-merge async, not pre-tag |

## Out of scope (explicitly named)

- VZ.framework integration of any kind.
- Intel macOS support.
- Linux host support (the workspace is `aarch64-apple-darwin` only).
- x86_64 Linux guests on Apple Silicon (use guest-side `binfmt_misc + qemu-user`).
- macOS guests inside squib (use `tart` for that — different layer).
- Live migration to/from Linux Firecracker hosts.
- Userspace GICv3 emulation (we require macOS 15+ for `hv_gic_*`).
- virtio-PCI transport.
- TDX / SEV / TEE features.
- macOS App Store distribution (HVF is incompatible with the App Sandbox).

## Approach summary

Single Rust workspace, single binary (`squib`), one supplementary binary (`squib-jail`). HVF backend via the `applevisor` crate. aarch64-only architecture code. Devices ported from upstream Apache-2.0 projects (libkrun for HVF specifics and TSI vsock; cloud-hypervisor for virtio-block/net/balloon/rng; Firecracker for MMDS/dumbo and the OpenAPI server). Snapshot serialization via `bitcode + serde` mirroring Firecracker's outer-container shape. Min macOS = 15 Sequoia; recommended = macOS 26 Tahoe.

See `specs/squib-design.md` for the architecture and `specs/squib-impl-plan.md` for the build sequence.
