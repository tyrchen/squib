# macOS Hypervisor Ecosystem — A 2025/2026 Landscape Report for `squib`

**Status:** research / decision input
**Last updated:** 2026-05-03
**Audience:** squib design and stack-selection

squib aims to be an interface-level drop-in replacement for AWS Firecracker on macOS — same OpenAPI surface, same JSON config, same CLI args — so developers can run Lambda/Fargate-style microVM workloads natively on a Mac without an x86 Linux VM in the loop. The core question this document answers is: **what does macOS actually give us in 2025/2026, and which existing pieces should we leverage, study, or avoid?**

The TL;DR is at the bottom (Section 11). The rest is the supporting evidence.

---

## 1. Apple Hypervisor.framework (HVF) — the low-level API

HVF is the kernel-supplied hypervisor entry point on macOS. It is comparable in conceptual level to KVM on Linux: it gives you vCPUs, second-stage page tables, and a "run until exit" loop. Everything above that — instruction emulation, MMIO routing, interrupt controllers, virtio devices — is your problem.

### 1.1 What HVF gives you (Apple Silicon, arm64)

The arm64 surface area, available since macOS 11 and steadily expanded:

- **VM lifecycle:** `hv_vm_create`, `hv_vm_destroy`, `hv_vm_map`/`hv_vm_unmap` to install host-backed memory at guest IPAs with R/W/X permissions, and `hv_vm_protect` to change permissions later (which is the building block for software dirty-page tracking via write-protect-and-fault).
- **vCPU lifecycle:** `hv_vcpu_create` (creates the vCPU and pins it to the calling pthread), `hv_vcpu_destroy`, `hv_vcpu_run`, `hv_vcpu_run_until` (returns on exit), and async interrupt injection via `hv_vcpus_exit`.
- **Register access:** `hv_vcpu_get_reg`/`hv_vcpu_set_reg` for general-purpose, SIMD/FP, and a curated list of system registers (`hv_vcpu_get_sys_reg`/`hv_vcpu_set_sys_reg`). `ESR_EL2`, `FAR_EL2`, `HPFAR_EL2`, `ELR_EL2` are exposed via `hv_vcpu_exit_t` after a return from `hv_vcpu_run`.
- **Exits:** the exit struct has a `reason` (exception, cancelled, vtimer-activated) and an exception payload with the syndrome; you decode `ESR_EL2` yourself to dispatch HVC, SMC, MMIO data abort, WFI/WFE traps, MSR/MRS traps, etc.
- **Generic Interrupt Controller v3:** **macOS 15+ (2024) added `hv_gic_*` APIs** that let you create a Hypervisor-managed GICv3 (`hv_gic_create`, distributor/redistributor base, SPI/PPI injection). Before macOS 15, you had to emulate the GIC entirely in userspace, which was the single biggest hurdle for arm64 VMMs on Mac. This is a major recent shift.
- **Nested virtualization (EL2):** macOS 15 added `hv_vm_config_set_el2_enabled()`, exposing nested virt on M3 Pro/Max and later silicon.
- **Scalable Matrix Extension (SME):** macOS 15.2 added SME state save/restore (`macos-15-2` feature in `applevisor`).
- **macOS 26 (Tahoe, 2025):** further IPA size / granule controls and refinements; `applevisor` defaults to `macos-26-0`.

### 1.2 HVF on Intel macOS (x86_64)

Functionally similar — `hv_vcpu_create`/`hv_vcpu_run`, VMCS-style reads via `hv_vmx_vcpu_read_vmcs`/`write_vmcs`, MSR access — but **Intel macOS is end-of-life for Apple's roadmap.** Apple Silicon is the only host architecture that gets meaningful new APIs (GIC, EL2, SME, Tahoe). Apple's own `apple/container` and `apple/containerization` are arm64-only by policy. New macOS VMM projects (lume, tart's VZ paths, libkrun's HVF backend) target arm64 first, Intel either as legacy or not at all.

### 1.3 Limits

- **vCPU cap:** The historical "8 vCPU" number on Apple Silicon is no longer a hard ceiling — recent Apple Silicon and recent macOS allow more, but the recommendation remains to **not exceed physical core count** and you should query `hv_vm_get_max_vcpu_count` (or read `applevisor`'s reported max) at runtime rather than hard-code. `applevisor` exposes `Vcpu::max_count()` for this.
- **vCPU<->thread binding:** every `hv_vcpu_*` call must run on the thread that called `hv_vcpu_create`. This forces a "one OS thread per vCPU" architecture; you cannot multiplex vCPUs onto a smaller thread pool.
- **Memory:** host-backed mapping only. There is no built-in dirty-bit tracking like KVM's `KVM_GET_DIRTY_LOG`; live-migration / snapshot-resume implementations have to roll their own using `hv_vm_protect` + write-fault (or accept full-memory snapshots).
- **No nested-paging "soft" features** (no built-in IOMMU emulation, no hot-plug memory above what you map up front — though you can map more lazily).

### 1.4 Entitlements & code signing

Three relevant entitlements:

- `com.apple.security.hypervisor` — required for **any** HVF use. Self-signing with a developer certificate works for individual development; getting it through notarization for distribution is straightforward.
- `com.apple.security.virtualization` — required for VZ (the higher-level framework).
- `com.apple.vm.networking` — required for **bridged** networking (`VZBridgedNetworkDeviceAttachment` and `vmnet`-bridged mode). This entitlement is **restricted by Apple to virtualization vendors** and requires an out-of-band request via Developer Technical Support. NAT/host-only modes do **not** need it. squib should plan around being unable to ship bridged mode by default and offer NAT as the primary network plus a userspace stack alternative.

The binary must be code-signed *after* compile with these entitlements embedded; `codesign --entitlements ... -s -` works for local dev, App Store / DTS contact required for `com.apple.vm.networking`.

---

## 2. Apple Virtualization.framework (VZ) — the high-level API

VZ first shipped in macOS 11 and is built **on top of** HVF inside Apple's frameworks. It hides the vCPU/exit loop entirely. You hand it a `VZVirtualMachineConfiguration` declaratively, call `start`, and Apple operates the VM.

### 2.1 What VZ gives you for free

- **Boot:**
  - `VZLinuxBootLoader` — direct Linux kernel + initrd + cmdline boot. **Format requirement:** raw, uncompressed `Image` (ARM64 boot executable, little-endian, 4K pages). Compressed `Image.gz` and PE32+ EFI kernels do **not** boot directly via this loader; users have to decompress first or switch to EFI.
  - `VZEFIBootLoader` — EFI boot from a disk for distros that need GRUB / shim (macOS 13+).
  - `VZMacOSBootLoader` — boot a macOS guest from a restore image (Apple Silicon only, max 2 macOS guests at once on consumer hardware).
  - **No PVH boot protocol** (which Firecracker uses on x86 for fastest boot). VZ is "Linux direct boot or EFI" only.
- **Devices, all VirtIO over Apple's VirtIO transport:**
  - `VZVirtioBlockDeviceConfiguration` — raw image file or block device. **No qcow2, no qed, no vhd** — sparse files yes, copy-on-write images no.
  - `VZNVMExpressControllerDeviceConfiguration` — NVMe (added in recent macOS).
  - `VZUSBMassStorageDeviceConfiguration` — USB block.
  - `VZVirtioNetworkDeviceConfiguration` — three attachments: `VZNATNetworkDeviceAttachment` (default, no entitlement), `VZBridgedNetworkDeviceAttachment` (bridged, requires entitlement), `VZFileHandleNetworkDeviceAttachment` (you supply an `fd`, used by socket_vmnet/vfkit/lima for plumbing custom networking).
  - `VZVirtioFileSystemDeviceConfiguration` — virtio-fs for shared host directories; multiple tags supported.
  - `VZVirtioSocketDeviceConfiguration` — vsock; see Section 6 for the gotchas.
  - `VZVirtioEntropyDeviceConfiguration`, `VZVirtioTraditionalMemoryBalloonDeviceConfiguration`, `VZVirtioConsoleDeviceConfiguration`.
  - `VZVirtioGraphicsDeviceConfiguration` (Apple Silicon only, paravirtualized GPU; macOS 14+).
  - `VZLinuxRosettaDirectoryShare` — exposes Rosetta 2 inside a Linux VM so the guest can run x86_64 Linux binaries natively-translated (huge for Lambda parity since Lambda binaries are typically x86_64).
- **Lifecycle:** `start`, `pause`, `resume`, `stop`, `requestStop`, save-state to file (`saveMachineStateTo`) and `restoreMachineStateFrom` (macOS 14+). State save is a true memory snapshot; this is the fastest path to "fork" semantics on Mac.

### 2.2 VZ constraints (the hard limits)

- **Closed device model.** You cannot add a custom MMIO device, a custom virtio device with a non-Apple-blessed feature bit, or expose a host PCI device. The VirtIO transport itself is Apple's; you cannot ride alongside it with `linux,mmio,addr=...` like QEMU.
- **No CPU template / CPUID masking.** You get the host CPU as-is. Firecracker users who rely on CPU templates for live-migration parity will have to drop that.
- **Single VirtIO bus, fixed.** The bus is internal to VZ; there is no introspection.
- **No live migration.** Snapshot/restore is to-disk, same-host only.
- **No PVH.** See above.
- **macOS-version skew:** features (`VZNVMe`, snapshots, GPU, Rosetta-share) gate on specific macOS versions; squib has to feature-detect.

### 2.3 Containerization framework (macOS 26 Tahoe)

Apple shipped two related projects in 2025 alongside macOS Tahoe:

- **`apple/containerization`** — a Swift package (Apache-2.0) that sits on top of VZ and adds: an OCI image puller, an EXT4 filesystem builder written in Swift, a `vminitd` Swift init binary that runs as PID 1 inside each guest and exposes a gRPC API over vsock, and `container-network-vmnet` for per-container networking. **One container = one VM** (Kata-style) — every container gets its own kernel and address space, on top of VZ.
- **`apple/container`** — the CLI (`container run`, `container build`, `container ps`) and a system daemon (`container system start`). OCI-compatible image registry, but not Docker-CLI-compatible. Apple Silicon only, macOS 26 minimum (some fallback for Sequoia in dev preview).

For squib's purposes, Containerization is **architecturally interesting but not a dependency**: it's Swift-first, container-shaped not microVM-shaped, and its API is gRPC-over-vsock to `vminitd` rather than Firecracker's REST. It does **prove** that VZ is good enough to build a sub-second-boot, OCI-compliant runtime on, which is useful evidence for the build-vs-borrow decision.

---

## 3. Rust crates for HVF / VZ

| Crate | What it wraps | Verdict |
|---|---|---|
| **applevisor** ([Impalabs](https://github.com/Impalabs/applevisor)) | Apple Silicon HVF (safe Rust) | **Would-leverage.** Last release 0.1.3 (Oct 2024) but feature-flagged through `macos-26-0` (default), tracks GIC (15+), EL2, SME. Apple-Silicon-only, which matches our target. Cleanest of the three. |
| **applevisor-sys** | Bindgen of `Hypervisor/Hypervisor.h` | **Would-leverage** (transitively, via `applevisor`). |
| **ahv** ([crates.io](https://crates.io/crates/ahv)) | HVF arm64 | Active but smaller surface; `applevisor` supersedes it for new code. **Would-study** for API design, **would-avoid** as primary dep. |
| **xhypervisor** ([RWTH-OS](https://github.com/RWTH-OS/xhypervisor)) | HVF on **both** Intel & Apple Silicon (used by uhyve) | **Would-study.** Useful if squib wants Intel macOS too; uhyve is a working example of using it. Less recent on the Apple Silicon side than `applevisor`. |
| **hv / hv-sys** ([cloud-hypervisor/hypervisor-framework](https://github.com/cloud-hypervisor/hypervisor-framework)) | HVF (high-level safe + sys) | **Would-study.** Cloud-Hypervisor org maintains it, but not the most active. |
| **hvf** crate | Older binding | **Would-avoid.** Superseded. |
| **objc2** ([madsmtm/objc2](https://github.com/madsmtm/objc2)) | Objective-C runtime in Rust | **Would-leverage** if any VZ usage. Top-tier ecosystem. |
| **objc2-virtualization** ([docs.rs](https://docs.rs/objc2-virtualization)) | VZ via objc2 — `VZVirtualMachine`, `VZVirtualMachineConfiguration`, all device configs, attachments, boot loaders | **Would-leverage.** This is the cleanest path to call VZ from Rust without a Swift bridge. Auto-generated, cargo-feature-gated, kept up to date by madsmtm. |
| **block2** | Objective-C blocks | **Would-leverage** (transitive, needed for VZ async callbacks). |
| **vm-memory** (rust-vmm) | Guest memory abstractions | **Would-leverage.** OS-agnostic; Linux-isms live in adjacent crates. Works on macOS. |
| **virtio-queue** (rust-vmm) | Virtio split/packed queue | **Would-leverage.** Pure-Rust, no Linux deps. |
| **virtio-bindings** (rust-vmm) | Virtio constants from Linux headers | **Would-leverage.** Header constants only. |
| **linux-loader** (rust-vmm) | bzImage / ELF / PE / PVH loading | **Would-leverage** for the HVF path. PE (`Image`) for arm64, ELF for x86, PVH for x86 fast-boot. |
| **vmm-sys-util** (rust-vmm) | fd/event/ioctl helpers | **Would-avoid as a dep on macOS.** Linux + partial Windows support; macOS not supported. squib will need its own thin equivalents (or carefully feature-gate). [Issue context: vhost-device #857.] |
| **kvm-bindings / kvm-ioctls** (rust-vmm) | Linux KVM | **Would-avoid.** Linux only by definition. |
| **vhost / vhost-user-backend** (rust-vmm) | vhost-user device protocol | **Would-avoid on macOS** for now (Linux-only); [there's a tracking issue but no port]. If squib ever wants offloaded device backends, this is a future Linux-host path. |

The "use rust-vmm where it's pure and write-fresh where it's Linux-bound" pattern is exactly what **libkrun** has been doing successfully — see Section 4.

---

## 4. Existing macOS VMM projects to learn from

### 4.1 libkrun + krunvm + krunkit ([containers/libkrun](https://github.com/containers/libkrun))

**Single most relevant prior art for squib.** Rust-based VMM, dual-backend: KVM on Linux, **HVF on macOS/arm64**. Latest release **1.18.0 in April 2026**, very active.

- Architecture: a C-API library (`libkrun.so` / `.dylib`) that embeds a VMM. Consumers (krun, crun-krun, krunvm, krunkit, muvm) link against it.
- Incorporates code from **Firecracker, rust-vmm, and Cloud-Hypervisor** — exactly the synthesis pattern squib needs.
- Devices on macOS: virtio-console, virtio-block, virtio-fs, virtio-net, virtio-vsock, virtio-balloon, virtio-rng, virtio-gpu (with Mesa Venus driver for paravirtualized Vulkan in the guest — non-trivial achievement on Mac).
- Networking: two strategies — **virtio-vsock + TSI** (Transparent Socket Impersonation, requires custom guest kernel) and **virtio-net + passt/gvproxy** (standard guest kernel).
- Boot: custom init binary statically linked into the guest image (not a Firecracker-style direct-kernel-with-rootfs separation by default, though variants exist).
- API: **C API**, not REST. No Firecracker-OpenAPI compatibility.

**Verdict: would-leverage / would-study heavily.** This is the closest existing thing to squib in spirit. squib differs in that we want Firecracker REST API parity and Firecracker JSON config compatibility, which libkrun does not provide. We can study libkrun's HVF + virtio integration patterns and potentially borrow code (Apache-2.0 LGPL — check license interplay).

### 4.2 vfkit ([crc-org/vfkit](https://github.com/crc-org/vfkit))

Go binary wrapping VZ via [Code-Hex/vz](https://github.com/Code-Hex/vz). Latest release **v0.6.3 in January 2026.** Used by podman-machine, minikube, crc, ovm.

- **VZ-based**, not HVF. Inherits VZ's constraints (raw images only, no PVH, no CPU templates, no custom devices) but inherits its devices for free.
- Devices: virtio-blk, NVMe, USB-mass-storage, NBD (interesting — exposes a network block device interface on the host side), virtio-net (NAT, fd, unix-socket), virtio-vsock, virtio-fs, virtio-rng, virtio-gpu, virtio-input, virtio-serial, Rosetta share.
- Boot: `linux` (direct kernel), `macos`, `efi`.
- **REST API** at `/vm/state` (GET/POST), `/vm/inspect` — much smaller surface than Firecracker. Configuration is via CLI args + JSON; no OpenAPI-compatible API server.
- Networking: `unixSocketPath` mode is the standard plumbing point — gvproxy / gvisor-tap-vsock listens on the unix socket and provides user-mode NAT + DNS + port forwarding.

**Verdict: would-study.** vfkit is the closest thing in spirit to squib in the VZ camp: a small, headless, CLI-driven VMM with a REST API. Studying its CLI<->REST<->VZ mapping is high-value. It's Go, so direct code reuse is out; the design patterns transfer.

### 4.3 Code-Hex/vz ([Code-Hex/vz](https://github.com/Code-Hex/vz))

Go bindings to VZ. Latest **v3.7.1 in August 2025.** The de-facto Go binding for VZ; vfkit and lima (in VZ mode) both use it. Not directly relevant to a Rust project except as a reference for how VZ APIs are exposed to a non-Swift language. **Verdict: would-study** as the analog of what `objc2-virtualization` does for Rust.

### 4.4 lume ([trycua/cua](https://github.com/trycua/cua), libs/lume)

Swift CLI + local HTTP API (port 7777, also seen documented as 3000) for managing macOS and Linux VMs on Apple Silicon, using VZ. Part of the trycua "Computer-Use Agent" stack. Apple Silicon and macOS 15+ only.

- Pattern that's interesting for squib: **local REST API server that fronts VZ**, written in Swift, exposing a small "list/run/stop" API. This is the same shape as Firecracker's API server, just over VZ instead of HVF + custom devices.
- Returns HTTP 202 Accepted for long-running operations and processes them asynchronously — same pattern Firecracker uses for `actions`.

**Verdict: would-study.** Confirms the "REST-over-VZ" approach is viable; their API surface is closer to "Lambda function pool manager" than "Firecracker-equivalent."

### 4.5 Tart ([cirruslabs/tart](https://github.com/cirruslabs/tart))

Swift, VZ-based, focused on **CI-grade macOS and Linux VMs** with **OCI-registry image distribution** ("docker pull, but for a VM image"). 25k+ installs, used in production by Atlassian, Figma, Mullvad, etc.

- Not microVM-focused — Tart VMs are full GUI-capable macOS guests.
- The OCI-registry-as-VM-image-store pattern is interesting for squib if we want an analog of "Firecracker rootfs.ext4 image distribution" via container registries.

**Verdict: would-study** for the OCI distribution pattern; **would-avoid** as a runtime dependency (wrong shape).

### 4.6 lima / colima

Lima is a CNCF Incubating project that runs Linux VMs on macOS. Two backends: QEMU+HVF (older default) and **VZ mode** (`vmType: vz`). Colima is a container-runtime layer (Docker / containerd) on top of Lima.

- VZ mode highlights: `vzNAT` network is faster than the QEMU+socket_vmnet path, no sudoers setup needed.
- For non-VZ networking: **socket_vmnet** ([lima-vm/socket_vmnet](https://github.com/lima-vm/socket_vmnet)) is the rootless `vmnet.framework` shim that all the QEMU-based macOS VMMs converge on — runs as a small daemon, applications connect via unix socket and get a virtual ethernet.
- Userspace network stack: **gvisor-tap-vsock** for user-mode networking; same library vfkit uses.

**Verdict: would-study** the networking patterns (`vzNAT`, `socket_vmnet`, `gvisor-tap-vsock`) — these are the templates squib will follow.

### 4.7 Apple Containerization / `apple/container`

See Section 2.3. **Verdict: would-study**, not depend on. Wrong language (Swift), wrong abstraction (one-VM-per-container), proves the perf envelope is achievable on VZ.

### 4.8 UTM ([utmapp/UTM](https://github.com/utmapp/UTM))

GUI-focused, wraps QEMU and offers HVF-acceleration mode and VZ mode. Cross-architecture emulation (x86 on arm). **Verdict: would-avoid** as a code source (GPL, GUI-focused, QEMU-tied), **would-acknowledge** as the reference "what does QEMU+HVF give you" that squib has to outperform on boot time.

### 4.9 OrbStack and Veertu Anka (proprietary)

OrbStack is a closed-source, Swift+Go+Rust+C macOS Docker/Linux runtime with very low overhead (<0.1% idle CPU) — proves squib's perf goals are attainable. Anka is a proprietary macOS-VM-on-Mac CI product. **Verdict: would-acknowledge** as existence proof, **would-avoid** as inspiration we can verify.

### 4.10 xhyve / hyperkit

Pre-VZ HVF-based VMMs derived from FreeBSD bhyve. **hyperkit** powered Docker Desktop for Mac through ~2020 before Apple shipped VZ. Both are **effectively unmaintained** for new platform features (no Apple Silicon support — they're Intel-only and the Apple-Silicon issue has sat open since 2020 on the moby/hyperkit repo). Minishift, which depended on xhyve, was archived June 2025.

**Verdict: would-avoid.** Historical reference only; their device implementations might inform squib's HVF path, but they're C and target the older HVF API.

### 4.11 Community Firecracker-on-Mac PoCs

A January 2025 proof-of-concept ([firecracker discussion #5019](https://github.com/firecracker-microvm/firecracker/discussions/5019)) booted aarch64 Linux on Apple Silicon by **swapping the KVM backend for Apple's Virtualization framework (VZ)** — explicitly chose VZ over HVF, "disabled many KVM-specific features," near-native perf via openssl benchmark. The Firecracker maintainers stated they have **no plans to support macOS officially.** This is squib's opportunity.

**Verdict: would-study** the patches (if published) for the specific KVM-isms that need replacement; **would-avoid** the "fork upstream Firecracker" path because of (a) the maintainer pushback and (b) the architectural mismatch (VZ does not give KVM-equivalent semantics).

---

## 5. Networking on macOS

### 5.1 vmnet.framework

Apple's user-space networking-for-VMs API. Three modes:

- **Shared (NAT) — `vmnet-shared` / `VMNET_SHARED_MODE`:** guests get an IP behind a host-side NAT, can talk to other guests on the same vmnet, can reach the internet via the host. **No special entitlement needed.** This is the squib default.
- **Host — `VMNET_HOST_MODE`:** guests can talk to host and other guests, no internet. No entitlement.
- **Bridged — `VMNET_BRIDGED_MODE`:** guest gets an IP on the physical LAN. **Requires `com.apple.vm.networking` entitlement, restricted by Apple to virtualization vendors via DTS.** squib should not assume access; we provide the code path but document the entitlement requirement.

Performance: vmnet-shared is good (kernel-side NAT), vzNAT in VZ mode is reportedly the fastest. Bridged is line-rate when accessible.

### 5.2 TAP alternatives — gone

The historical **tuntaposx** kernel extension is dead on Apple Silicon; **kexts are no longer loadable on Apple Silicon by default** (require recovery-mode toggles plus a downgrade of system security). For practical squib distribution, "TAP" on Mac means "user-mode TUN-shaped thing on top of vmnet or unix-socket." Do not pursue tuntaposx.

### 5.3 Userspace TCP/IP options

- **gvisor-tap-vsock** ([containers/gvisor-tap-vsock](https://github.com/containers/gvisor-tap-vsock)) — Go, userspace TCP/IP stack derived from gVisor. The host runs a `gvproxy` daemon; the guest's virtio-net is attached via unix socket / vsock to that daemon; the daemon does NAT + DNS + host port forwards. Used by vfkit, podman-machine, lima (VZ and QEMU paths). **No entitlement needed.** Cost: a Go binary on the host and slightly higher CPU than vmnet-shared.
- **socket_vmnet** ([lima-vm/socket_vmnet](https://github.com/lima-vm/socket_vmnet)) — small daemon (replaces vde_vmnet) that exposes vmnet as a unix-domain-socket connection point so multiple unprivileged VMMs can share a vmnet. Faster than gvproxy for raw throughput but needs sudoers-like setup.
- **passt/pasta** — Linux-host userspace networking, **not portable to macOS** at the moment.

### 5.4 How the existing stack does it

| Project | Default network | Notes |
|---|---|---|
| Apple `container` | per-container vmnet | container-network-vmnet helper |
| vfkit | virtio-net + unix-socket → gvproxy | most common podman/crc setup |
| lima (VZ) | vzNAT (vmnet-shared via VZ) | fastest |
| lima (QEMU) | socket_vmnet or gvproxy | configurable |
| libkrun | virtio-vsock + TSI **or** virtio-net + passt/gvproxy | TSI requires custom kernel |
| Tart | vzNAT or bridged | bridged needs entitlement |

**For squib:** dual-mode by default. Primary: virtio-net + vmnet-shared (no entitlement, works out of the box). Optional: virtio-net via unix-socket → bundled `gvproxy` (or our own gvisor-tap-vsock embedding) for users who want fully userspace networking with no kernel network state.

---

## 6. vsock on macOS — the gap, and how to bridge it

**macOS has no `AF_VSOCK` in its kernel.** This is the single biggest interface-compatibility issue for any Firecracker drop-in.

### 6.1 What VZ provides

`VZVirtioSocketDevice` exposes vsock to the guest. The guest sees a normal AF_VSOCK socket and connects to host CIDs/ports. **On the host side, however, there is no AF_VSOCK to accept on.** Instead VZ surfaces vsock connections as:

- **Host-listens-for-guest:** a Unix Domain Socket path you specify; when the guest connects to vsock port N on the host CID, the host gets a UDS accept on the configured path.
- **Host-initiates-to-guest:** an Objective-C method (`VZVirtioSocketDevice.connectToPort:completionHandler:`) returning a file descriptor you can read/write.

This is **exactly Firecracker's host-side multiplexer model** (firecracker uses one UDS per port with a `CONNECT N` handshake). Mapping is straightforward.

### 6.2 What HVF provides

Nothing. If squib uses HVF directly, we implement virtio-vsock entirely ourselves: a virtio device backed by guest memory queues, a mux on the host that exposes the same UDS-per-port semantics Firecracker does. This is well-trodden code (Firecracker's, libkrun's) but it's our code, not a free dependency.

### 6.3 What squib should expose

Firecracker's vsock API is: `PUT /vsock { guest_cid, uds_path }`. The host is a unix socket; per-port subdirectories or a `_$port` suffix convention. squib should keep that exact contract — userland clients calling vsock work without modification. The implementation under the hood is either VZ's `VZVirtioSocketDevice` (free) or our own virtio-vsock device on top of HVF (bring your own).

There is **no clean way** to expose host-side AF_VSOCK semantics to host apps that genuinely want `AF_VSOCK` syscalls — they have to use the unix-socket bridge. This is the same compromise vfkit, libkrun, podman-machine, and Apple's own containerization make. squib should not try to fight it.

---

## 7. Block storage

### 7.1 VZ block backends

- `VZDiskImageStorageDeviceAttachment(url:readOnly:)` — raw disk image (sparse files supported, `truncate -s` to size).
- `VZNetworkBlockDeviceStorageDeviceAttachment` — NBD URL (supported in vfkit).
- `VZUSBMassStorageDeviceConfiguration` — USB mass-storage shape.
- `VZNVMExpressControllerDeviceConfiguration` — NVMe disk (newer macOS).

**No native qcow2.** No vmdk. No vhd. To support qcow2 (Firecracker doesn't, but real-world Linux dev images are qcow2), squib has two options: (a) decompress to raw on import, or (b) sit a userspace qcow2 layer behind NBD and use the NBD attachment. (b) is what vfkit's NBD support enables.

### 7.2 HVF block backends

You bring your own. Standard Rust play: `virtio-blk` device backed by a `File`/`io_uring`-style abstraction (no io_uring on macOS — use `kqueue` + `pwrite`/`preadv2`-equivalents, or just blocking IO in a thread pool). Firecracker's `virtio-blk` source under `vendors/firecracker/src/vmm/src/devices/virtio/block/` is directly portable.

---

## 8. Boot

### 8.1 VZ Linux boot

`VZLinuxBootLoader` takes a kernel URL, optional initrd URL, optional cmdline. Format: **raw arm64 `Image` (or x86_64 `bzImage` on Intel), little-endian, 4K pages.** Gzipped images do not boot directly; you must decompress. Initrd compression is mostly tolerated. **No PVH boot protocol** is exposed — Firecracker on x86 uses PVH for fastest boot, but on macOS we don't have that lever. On arm64 the kernel-protocol equivalents (the `Image` boot protocol, dtb passed in `x0`) are what VZ uses.

### 8.2 HVF Linux boot

Bring your own loader. `linux-loader` (rust-vmm) handles **bzImage / ELF / PE / PVH** — squib can use it directly. The trickiest part on arm64 is constructing the **device tree blob (FDT)** and putting it where the kernel expects, plus configuring PSCI for SMP. There's no exit from `applevisor` / HVF that does this for you.

### 8.3 Firecracker config compatibility

Firecracker's `boot-source` JSON is `{ kernel_image_path, initrd_path, boot_args }`. Mapping:

- VZ path: pass through to `VZLinuxBootLoader`. Decompress kernel if needed.
- HVF path: pass through to `linux-loader` + custom FDT builder.

Both work for the squib API.

---

## 9. Memory limits, perf, and Mac-specific characteristics

### 9.1 Boot time

- **Firecracker on KVM:** documented 125ms guest userspace, < 5 MiB overhead, 150 microVMs/host/sec.
- **VZ Linux direct boot:** vfkit reports ~30s to login prompt for full Fedora. **But Apple's own Containerization framework reports sub-second container start** because they ship a tiny custom kernel + minimal init. So the VZ overhead floor is more like **150-300 ms** when you control the kernel, plus VZ-internal setup. Lume reports HTTP 202 + background processing because even VZ start isn't instant.
- **HVF directly (libkrun, uhyve):** libkrun reports sub-200ms startup, which matches Firecracker's envelope. This is the strongest boot-time argument for HVF over VZ.

### 9.2 Memory overhead

- Firecracker microVM: ~3-5 MiB per VM beyond guest memory.
- VZ: tens of MiB per VM (the framework is heavier; an `XPC`-mediated VZ helper process exists per VM).
- libkrun (HVF): closer to Firecracker.

### 9.3 Architecture targeting

- **Apple Silicon arm64:** primary target. All new APIs (GIC, EL2, Containerization, Rosetta-share, NVMe in VZ) gate here.
- **Intel macOS:** legacy. Apple's roadmap doesn't add features. Lume, `apple/container`, libkrun-on-Mac are arm64-only. **Recommendation: squib supports arm64 first-class, Intel only if `xhypervisor` makes it cheap and a user actually asks.**

### 9.4 vCPU caps

No fixed number in 2025/2026. Query at runtime via `hv_vm_get_max_vcpu_count`. M3 Max has 16 cores; you'll get close to that. Practical cap for boot-time-sensitive workloads: 1-4 vCPUs per microVM, dozens of microVMs in flight.

---

## 10. Architecture decisions for squib

### 10.1 VZ vs HVF — the core choice

| Axis | VZ | HVF |
|---|---|---|
| Devices ready to use | virtio-blk/net/fs/vsock/rng/balloon/console/gpu, all free | nothing — we write all of them |
| Boot time | 150-300ms with tuned kernel | sub-200ms (Firecracker-class) |
| API surface | declarative config object | vCPU run-loop |
| CPU templates | none (Firecracker-incompatible) | possible (decode + filter MSRs / sysregs) |
| PVH boot | no | yes (via linux-loader) |
| Snapshots | yes (macOS 14+) | bring your own |
| Custom devices | impossible | trivial |
| Memory overhead per VM | tens of MiB | ~5 MiB |
| Code we maintain | thin Swift/objc bridge + API server | full VMM (~Firecracker-sized) |
| Time-to-MVP | weeks | months |
| Firecracker semantic parity | partial (no PVH, no CPU templates, no rate limiters tied to virtio queue depth) | full |

For a **dev-machine local microVM runner**, where the goal is "Lambda parity, fast enough, works out of the box," **VZ wins on time-to-MVP and breadth of working devices.** HVF wins on boot time, semantic parity, and control.

### 10.2 Recommended stack: hybrid VZ-first with an HVF escape hatch

1. **squib-api** (Rust, axum) — Firecracker-OpenAPI-compatible REST server. JSON config compatibility. Lives in `crates/squib-api`.
2. **squib-core** (Rust) — the VMM trait and type system. Defines `MicroVm`, `Device`, `BootSource`, etc. Lives in `crates/squib-core`.
3. **squib-vz** (Rust) — VZ backend using `objc2-virtualization`. Implements the `MicroVm` trait. Default backend. `crates/squib-vz`.
4. **squib-hvf** (Rust) — HVF backend using `applevisor` + rust-vmm crates (`vm-memory`, `virtio-queue`, `linux-loader`) + ports of Firecracker's virtio devices. Opt-in via `--hypervisor=hvf` for users who need PVH/CPU-templates/sub-200ms boot. `crates/squib-hvf`.
5. **squib-net** — networking adapters: `vmnet-shared` (default, no entitlement), `gvproxy-bundled` (userspace, no entitlement, slightly slower), `bridged` (gated on entitlement). `crates/squib-net`.
6. **squib-block** — block backends: raw file, NBD-bridged-qcow2 (using the NBD attachment in VZ; in HVF, a raw virtio-blk port from Firecracker).
7. **squib-vsock** — vsock backend: VZ's `VZVirtioSocketDevice` for VZ path, custom virtio-vsock for HVF path. Both expose Firecracker's UDS-per-port surface.

This is the **same shape `libkrun` arrived at** (rust-vmm + Firecracker code + HVF binding + simple host API), with two changes that suit our goal: (a) the API is Firecracker-OpenAPI rather than a C library, and (b) we offer VZ as the default backend so we get GPU, virtio-fs, and Rosetta-in-VM for free without rewriting them.

### 10.3 Has anyone done this exact pattern?

Pieces of it, yes; the specific combination of **Firecracker-OpenAPI-compatible REST + VZ + HVF fallback + Rust** does **not exist** in the public ecosystem as of May 2026. Closest:

- **vfkit** = small REST + VZ + Go. No Firecracker-API-compatibility.
- **libkrun** = HVF-or-KVM + Rust + C-API + their own design. No REST, no Firecracker-API-compatibility.
- **lume** = REST + VZ + Swift. Different API shape, more "macOS GUI VMs on demand" than microVM.
- **Community Firecracker-on-Mac PoC (Jan 2025)** = forked Firecracker + VZ. Not maintained, Firecracker upstream rejects.

**There is a clear, defensible niche for squib here.**

### 10.4 Networking and vsock gap-bridging

- **vsock:** mirror Firecracker's UDS-per-port API exactly. Map onto `VZVirtioSocketDevice` (UDS path attachment) in VZ; implement virtio-vsock with the same UDS surface in HVF. Document clearly that host-side `AF_VSOCK` is unavailable on macOS (no project provides it).
- **network:** default is `vmnet-shared` (NAT, no entitlement, fastest with VZ). Offer `--network=userspace` that boots a bundled `gvproxy` for fully unprivileged setups. Document `--network=bridged` as requiring `com.apple.vm.networking` and ship without it enabled.
- **TAP fdname semantics from Firecracker (`PUT /network-interfaces`):** `host_dev_name` is meaningless on Mac. Map it to a vmnet interface name or accept it and ignore. Document the deviation in a small "macOS portability notes" section of squib's API docs. This is the smallest interface-compat compromise.

---

## 11. TL;DR — recommendations

1. **Target Apple Silicon (arm64) first-class. Treat Intel macOS as best-effort if cheap.** Apple isn't adding features there.
2. **Default VMM backend = VZ via `objc2-virtualization`.** You get virtio-blk/net/fs/vsock/rng/balloon/console/gpu/Rosetta-share *and* `VZLinuxBootLoader` *and* snapshots for free. Time-to-MVP measured in weeks.
3. **Optional VMM backend = HVF via `applevisor` + rust-vmm crates + Firecracker code ports.** Targets users who need PVH, sub-200ms boot, CPU templates, custom devices. Time-to-feature-parity measured in months. **Study libkrun's HVF backend** — it's the closest existing prior art (Apache-2.0 LGPL — confirm license compatibility before code-borrowing).
4. **API server = Firecracker-OpenAPI-compatible, in Rust, with axum.** Translate Firecracker's `PUT /machine-config`, `PUT /boot-source`, `PUT /drives/{id}`, `PUT /network-interfaces/{id}`, `PUT /vsock`, `PUT /actions` to backend operations. Document the small set of unavoidable deviations (no host-side AF_VSOCK, no PVH on VZ path, no `host_dev_name` literal interpretation, CPU templates only on HVF backend).
5. **Networking primary = `vmnet-shared` (NAT).** No entitlement required, fastest in VZ mode (`vzNAT`). Secondary = embedded `gvproxy` (gvisor-tap-vsock) for fully userspace plumbing. Bridged mode shipped but disabled by default — document the entitlement.
6. **vsock = UDS-per-port mux** identical to Firecracker. On VZ, this is `VZVirtioSocketDevice` straight-through. On HVF, port Firecracker's virtio-vsock to use `applevisor` memory APIs.
7. **Block = raw file primary, NBD-via-userspace-qcow2 as a stretch goal.** Don't try to add qcow2 to VZ directly; use the NBD attachment.
8. **Boot = VZLinuxBootLoader on the VZ path** (decompress gzipped kernels at config-load time), **`linux-loader` on the HVF path** (bzImage/ELF/PE, plus our own FDT for arm64).
9. **rust-vmm dependency policy:** `vm-memory`, `virtio-queue`, `virtio-bindings`, `linux-loader` — yes. `vmm-sys-util`, `kvm-*`, `vhost-*` — no on macOS, write or copy minimal local equivalents.
10. **License-aware code reuse:** Firecracker (Apache-2.0) → fine to port virtio device source verbatim. libkrun (LGPL-2.1) → study, but don't copy/link in unless we accept LGPL terms. Cloud-Hypervisor (Apache-2.0) → fine.

The build-vs-borrow line is clear: **borrow VZ wholesale for the easy 80% (the dev-machine sweet spot) via `objc2-virtualization`, build HVF for the hard 20% (Firecracker-grade boot/control) on top of `applevisor` + rust-vmm + ported Firecracker devices, and put a Firecracker-API-compatible Rust REST server in front of both.** That's a stack no existing project has assembled, and it's the right shape for squib.

---

## Sources

- [Apple Hypervisor framework](https://developer.apple.com/documentation/hypervisor)
- [Apple Hypervisor updates](https://developer.apple.com/documentation/updates/hypervisor)
- [Apple Virtualization framework](https://developer.apple.com/documentation/virtualization)
- [VZLinuxBootLoader](https://developer.apple.com/documentation/virtualization/vzlinuxbootloader)
- [VZVirtioFileSystemDeviceConfiguration](https://developer.apple.com/documentation/virtualization/vzvirtiofilesystemdeviceconfiguration)
- [Apple vmnet framework](https://developer.apple.com/documentation/vmnet)
- [Apple Containerization (Swift package)](https://github.com/apple/containerization)
- [Apple container CLI](https://github.com/apple/container)
- [Anil Madhavapeddy — Under the hood with Apple Containerization](https://anil.recoil.org/notes/apple-containerisation)
- [applevisor (Impalabs)](https://github.com/Impalabs/applevisor)
- [applevisor docs.rs](https://docs.rs/applevisor/latest/applevisor/)
- [ahv crate](https://docs.rs/ahv/latest/ahv/)
- [xhypervisor (RWTH-OS)](https://github.com/RWTH-OS/xhypervisor)
- [cloud-hypervisor/hypervisor-framework](https://github.com/cloud-hypervisor/hypervisor-framework)
- [objc2 (madsmtm)](https://github.com/madsmtm/objc2)
- [objc2-virtualization](https://docs.rs/objc2-virtualization)
- [rust-vmm/vm-memory](https://github.com/rust-vmm/vm-memory)
- [rust-vmm/vm-virtio](https://github.com/rust-vmm/vm-virtio)
- [rust-vmm/linux-loader](https://github.com/rust-vmm/linux-loader)
- [rust-vmm/vmm-sys-util](https://github.com/rust-vmm/vmm-sys-util)
- [containers/libkrun](https://github.com/containers/libkrun)
- [crc-org/vfkit](https://github.com/crc-org/vfkit)
- [vfkit usage doc](https://github.com/crc-org/vfkit/blob/main/doc/usage.md)
- [Code-Hex/vz](https://github.com/Code-Hex/vz)
- [trycua/cua (lume)](https://github.com/trycua/cua)
- [cirruslabs/tart](https://github.com/cirruslabs/tart)
- [lima-vm/lima](https://lima-vm.io/docs/)
- [lima socket_vmnet](https://github.com/lima-vm/socket_vmnet)
- [containers/gvisor-tap-vsock](https://github.com/containers/gvisor-tap-vsock)
- [moby/hyperkit](https://github.com/moby/hyperkit)
- [utmapp/UTM](https://github.com/utmapp/UTM)
- [OrbStack architecture](https://docs.orbstack.dev/architecture)
- [Veertu Anka](https://veertu.com/)
- [Firecracker repo](https://github.com/firecracker-microvm/firecracker)
- [Firecracker discussion #5019 — running on Apple Silicon](https://github.com/firecracker-microvm/firecracker/discussions/5019)
- [Whexy — Arm VMM with Apple's Hypervisor Framework](https://www.whexy.com/en/posts/simpple_01)
- [emirb — State of MicroVM Isolation in 2026](https://emirb.github.io/blog/microvm-2026/)
- [The Coders Blog — Apple Silicon Virtualization 2026](https://thecodersblog.com/the-fundamental-shift-in-virtualization-on-apple-silicon-2026)
- [vfkit FOSDEM 2023 slides](https://archive.fosdem.org/2023/schedule/event/govfkit/attachments/slides/5847/export/events/attachments/govfkit/slides/5847/fosdem2023_go_devroom_vfkit.pdf)
- [Hacker News — com.apple.vm.networking entitlement](https://news.ycombinator.com/item?id=25382885)
- [Apple Developer Forums — vm.networking entitlement](https://developer.apple.com/forums/thread/729686)
