---
title: HVF Performance, Dirty Page Tracking, Snapshots, and macOS UFFD-Equivalent
status: research
audience: engineers building squib's snapshot, dirty-tracking, postcopy-paging, and IO subsystems
last_reviewed: 2026-05-03
---

# HVF Performance, Dirty Page Tracking, Snapshots, and macOS UFFD-Equivalent

Research deliverable for **squib** — a macOS-native, Apple-Silicon-only, Firecracker-API-compatible microVM monitor on Hypervisor.framework.

This document targets the operationally hard parts: HVF's performance ceiling, dirty page tracking without `KVM_GET_DIRTY_LOG`, postcopy snapshot restore without `userfaultfd`, snapshot file format strategy, IO subsystem choice, and code-signing realities.

## 1. HVF Performance Ceiling on Apple Silicon

### 1.1 The current floor

Firecracker's published reference is **~125 ms** end-to-end boot on Linux/KVM (NSDI'20, replicated through 2024–2026). That's the bar.

Apple's own **Containerization framework** (WWDC 2025, shipping in macOS 26 Tahoe) claims "sub-second" container start with a VM-per-container model, **but it is built on Virtualization.framework (VZ), not raw HVF**. Their wins come from:
- A custom-tuned 6.14.x Linux kernel with VIRTIO drivers built-in (no module loading).
- A minimal Swift `vminitd` that talks gRPC over vsock — no traditional init system.
- An ASIF (Apple Sparse Image Format) disk backend that's sparse, fast to attach, fast to clone.

Apple does not publish hard numbers below "sub-second"; community measurements with `tart` and `lume` (both VZ) land **400–900 ms** to login prompt, **~150–300 ms** to first userspace instruction on a pre-warmed kernel.

`vfkit` to a Fedora prompt is ~30 s — that's a normal Fedora boot, not a Firecracker-style minimal boot. Not the ceiling.

`libkrun` / `krunkit` (HVF/arm64) advertises "smallest possible footprint" without publishing a number. Reading `src/hvf/src/lib.rs`: simple blocking `hv_vcpu_run` loop, no `hv_vcpu_run_until`, no dirty tracking, no snapshots. Direct boot into a tiny init.

**Realistic squib target: 250–500 ms cold boot on M-series.** Sub-200 ms is achievable with the right kernel (direct boot, `pci=off`, lz4 compression, no ACPI), but you'll spend most of the budget on kernel decompression and userspace init. The HVF entry+exit overhead per se is small relative to that.

### 1.2 Where HVF actually costs you

Documented overhead sources, ranked by what hurts in microVM workloads:

1. **Per-`hv_vcpu_run` syscall round-trip.** Every VM exit returns to userspace. On Apple Silicon, the syscall path is fast but not free; expect ~2–5 µs of pure overhead per exit. KVM handles many light exits in-kernel; HVF has no kernel-side helper for MMIO.
2. **Forced exits via `hv_vcpus_exit` / `hv_vcpu_interrupt`.** No native CPU-kick analogue to KVM's signal-based wakeup. You force exit by IPI, which costs more than KVM signal injection.
3. **System register trap dispatch.** Every guest sysreg access not hardware-virtualized comes back as ESR `EC=0x18`. Exit cost is symmetric with KVM, but the *handling* is in pure userspace — KVM emulates many of these in-kernel.
4. **Interrupt injection.** GIC emulation in userspace until `hv_gic_create()` (macOS 13+) gave us a host-side GICv3. Use it.
5. **vCPU thread pinning.** HVF runs vCPUs on whatever pthread you bind to the vCPU; there is no scheduler hint. P-cores vs E-cores matter — pin to performance cores via `pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE)` or use the work-interval API. M-series chips have 4–6 P-cores; over-subscription is real.

### 1.3 `hv_vcpu_run_until` and what it actually buys

`hv_vcpu_run_until(vcpu, deadline)` (deadline in mach absolute time) is available in modern macOS arm64 SDKs. The `HV_DEADLINE_FOREVER` constant exists for "never time out".

It does **not** batch exits. Each exit still returns to userspace. What it gives you:
- A clean way to bound vCPU run time without firing a separate timer thread.
- Useful for vTimer emulation and to avoid having to sleep+IPI just to deliver a pending virtual timer interrupt.
- libkrun's HVF code doesn't use it — they rely on external `hv_vcpus_exit` kicks. Fine for one-shot microVMs, but for multi-vCPU snapshotting and live introspection, prefer `hv_vcpu_run_until` so you can deterministically punctuate execution.

### 1.4 Apple Silicon hardware features

- **Stage-2 page tables (SLAT)**: present on all M-series, used by HVF transparently. Required for dirty-tracking via write-protect.
- **VHE (Virtualization Host Extensions)**: present, used by the host kernel; the host runs at EL2.
- **ECV (Enhanced Counter Virtualization)**: present on M3+, helps with timer trapping. Not directly exposed in HVF API but reduces sysreg trap rate for `CNTV*`.
- **Nested virtualization**: M3 Pro/Max and later, macOS 15+ only. Not a squib day-1 concern.
- **IPA granularity**: configurable down to 4 KiB on macOS 26 (was 16 KiB minimum). Use this — Linux guests prefer 4 KiB pages.

### 1.5 Profiling HVF

The state of HVF profiling is bad. There is no public dtrace USDT in HVF. Options:

- **kperf / kperfdata** (private framework, reverse-engineered). 2 fixed counters (cycles, instructions) + 8 configurable. Sufficient for boot-path profiling. The `mperf` and `samply` projects (March 2026) wrap this and work on Apple Silicon.
- **Instruments.app** with the System Trace template captures kernel transitions, which lets you see `hv_vcpu_run` enter/exit boundaries.
- **DTrace** for HVF guests is documented as fragile; reverse-engineered work in `tjfontaine/vm_profile_guest` shows guest stack walking is possible after deducing `hv_thread_target` / `hv_task_target` layouts from `AppleHV.kext`. Not a squib priority.

Recommendation: instrument squib's own VMM hot path with `tracing` spans, use samply for sampled profiling, and use Instruments only when you suspect HVF entry/exit cost specifically.

## 2. Dirty Page Tracking on HVF

### 2.1 What HVF gives you (and doesn't)

There is **no** `KVM_GET_DIRTY_LOG` analogue in HVF. There never has been. `hv_vm_protect` is the only documented mechanism, and you build dirty tracking on top of it manually.

```c
hv_return_t hv_vm_protect(hv_ipa_t ipa, size_t size, hv_memory_flags_t flags);
```

Flags: `HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC`. macOS 13+ added `hv_vm_protect_space` for multi-address-space VMs; squib only needs the single-space form. macOS 14+ allows up to 64 GiB of guest RAM; macOS 15+ accepts 4 KiB IPA granularity.

**Trap on data abort is the only signal.** When the guest writes a page you've stripped of `HV_MEMORY_WRITE`, HVF returns from `hv_vcpu_run` with an exception. Decode the syndrome:
- `ESR_EL2[31:26]` = `EC` = `0x24` (data abort, lower EL).
- `ESR_EL2[6]` = `WnR` = 1 means write.
- `HV_VCPU_EXIT_REASON_EXCEPTION` carries the IPA and the FAR_EL2 of the access.

### 2.2 The standard write-protect dirty loop

```rust
const PAGE_SIZE: usize = 4096;

struct DirtyTracker {
    base_ipa: u64,
    len: usize,
    bitmap: Vec<AtomicU64>,
}

impl DirtyTracker {
    fn arm(&self) -> Result<()> {
        // Strip write permission on entire tracked range.
        let flags = HV_MEMORY_READ | HV_MEMORY_EXEC;
        unsafe { hv_vm_protect(self.base_ipa, self.len, flags) }
            .ok_or(Error::HvProtect)
    }

    /// Called from the vCPU exit path when a write data-abort is seen.
    fn on_write_fault(&self, fault_ipa: u64) -> Result<()> {
        let off = fault_ipa.checked_sub(self.base_ipa)
            .ok_or(Error::FaultOutsideRange)?;
        let page = (off as usize) / PAGE_SIZE;
        let word = page / 64;
        let bit = page % 64;
        self.bitmap[word].fetch_or(1u64 << bit, Ordering::Relaxed);

        // Re-grant write so the guest can retry the instruction.
        let page_ipa = self.base_ipa + (page * PAGE_SIZE) as u64;
        let flags = HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC;
        unsafe { hv_vm_protect(page_ipa, PAGE_SIZE, flags) }
            .ok_or(Error::HvProtect)
    }

    /// Drain dirty bits + re-arm for next epoch.
    fn snapshot_and_rearm(&self) -> Vec<u64> {
        let mut dirty = Vec::new();
        for (w, word) in self.bitmap.iter().enumerate() {
            let mut bits = word.swap(0, Ordering::AcqRel);
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                dirty.push((w * 64 + b) as u64);
                bits &= bits - 1;
            }
        }
        let _ = self.arm();
        dirty
    }
}
```

### 2.3 Performance characteristics

Cost of a single write fault:
- VM-exit: ~3–5 µs.
- ESR decode + bitmap update: <100 ns.
- `hv_vm_protect` for 1 page: ~5–15 µs (TLB shootdowns are expensive).
- Re-entry: ~2 µs.

So roughly **10–20 µs per first-touch write per page per epoch**. For a guest doing 1 GB/s of writes (random, page-spread), that's 250 K dirty pages/sec, costing ~2.5–5 seconds of wall clock per second of guest execution — **unworkable**. For workloads with write-locality (re-touching the same pages), cost drops dramatically because each page is faulted once per epoch.

A QEMU-on-HVF arm64 footgun from 2021: stripping `HV_MEMORY_EXEC` along with write led to instruction faults; the standard fix is to keep `EXEC` in the dirty-armed permission set.

**Mitigations for squib:**
- **Larger granularity** when possible. Track 2 MiB blocks for warm-up, drop to 4 KiB only for hot regions. Reduces protect-call count 512×.
- **Batch re-protect**: don't `hv_vm_protect` page-by-page in the exit handler. Promote multiple faulted pages in one call by collecting writes during a small window (care: the guest is paused per-vCPU, not globally).
- **Bound concurrent dirty-tracked range**: only arm tracking on a snapshot/iter checkpoint, not steady-state.
- **Two-pass copy-on-checkpoint**: snapshot epoch N happens by stop-the-world copy of pages dirty in epoch N-1, then arm epoch N. Migration-style incremental.

### 2.4 Can the host process catch writes through its *own* mapping?

This is the userfaultfd-WP analogue question. The setup: the same memory is `mmap`'d into our process AND mapped to the guest via `hv_vm_map`. If we `mprotect(PROT_READ)` the host mapping, host writes fault. Does the guest's stage-2 mapping also become read-only?

**Answer: no, not automatically.** Stage-2 protections are independent of the host's `mprotect` flags on the underlying VM region. `hv_vm_map` snapshots the host VA permissions at map time; subsequent `mprotect` does not propagate to stage-2. You must use `hv_vm_protect` for guest-side protection.

The *converse* is useful: you can make the host mapping `PROT_NONE` and rely on `hv_vm_protect` for guest access while dispatching host accesses via Mach exception ports for postcopy paging (see §3). This is the trick to build a UFFD analogue.

## 3. Postcopy / Lazy Snapshot Restore — The Mach-Exception-Port Path

Linux Firecracker uses `userfaultfd(2)` with a sideband Unix-domain socket protocol; an out-of-process handler `UFFDIO_COPY`s pages on demand. macOS has no userfaultfd. We build our own.

### 3.1 Architecture

1. On snapshot load, allocate guest RAM with `mach_vm_allocate` and immediately `mach_vm_protect(VM_PROT_NONE)`.
2. `hv_vm_map` the region to the guest IPA with full RWX (rely on host-side `PROT_NONE` to fault our own threads).
3. Register a **task-level** Mach exception port for `EXC_MASK_BAD_ACCESS` with behavior `EXCEPTION_DEFAULT_BEHAVIOR | MACH_EXCEPTION_CODES`.
4. A dedicated server thread runs `mach_msg(MACH_RCV_MSG)` in a loop, dispatching on `exception_raise` MIG calls.
5. On fault: the kernel sends a message naming the faulting thread, exception type, and codes (the offending VA is in `code[1]` for `EXC_BAD_ACCESS` / `KERN_PROTECTION_FAILURE`). Compute the page, copy bytes from the snapshot file into that page, `mach_vm_protect(VM_PROT_READ | VM_PROT_WRITE)` the page, reply with `KERN_SUCCESS`. Guest retries.

The trick with HVF: the **guest-side** access through stage-2 also faults in two scenarios:
- The host page-table entry is not present (`PROT_NONE` host mapping). Stage-2 walks reach the host VM layer, kernel signals a fault that surfaces as `hv_vcpu_run` returning with a data-abort exit. Handle in the vCPU thread.
- The guest tries to read a page never touched by host. Same outcome.

So we need **two paths**: vCPU exit handler (for guest faults) and Mach exception handler (for host-side faults from squib's own threads accessing guest RAM, e.g. virtio block writes copying request payloads). Both end up at the same paging-in routine.

### 3.2 Code sketch — Mach exception port setup

Use the `mach2` crate (`mach2 = "0.4"` is current — note: `mach` is unmaintained, `mach2` is the active fork; both leave many APIs missing, so `mach-sys` or hand-`extern "C"` declarations may be needed for `mach_exc.defs` MIG types).

```rust
use std::sync::Arc;
use std::thread;

use mach2::exception_types::{
    EXCEPTION_DEFAULT, EXC_MASK_BAD_ACCESS, MACH_EXCEPTION_CODES,
};
use mach2::kern_return::{kern_return_t, KERN_SUCCESS};
use mach2::mach_port::{mach_port_allocate, mach_port_insert_right};
use mach2::message::{mach_msg, MACH_MSG_TYPE_MAKE_SEND};
use mach2::port::{mach_port_t, MACH_PORT_RIGHT_RECEIVE};
use mach2::task::task_set_exception_ports;
use mach2::thread_status::ARM_THREAD_STATE64;
use mach2::traps::mach_task_self;

use crate::pager::Pager;

pub struct ExceptionServer {
    port: mach_port_t,
    pager: Arc<Pager>,
}

impl ExceptionServer {
    pub fn install(pager: Arc<Pager>) -> Result<Self, SquibError> {
        let mut port: mach_port_t = 0;
        // SAFETY: standard mach API, port is out-param
        let kr = unsafe {
            mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &mut port)
        };
        kr_ok(kr)?;
        let kr = unsafe {
            mach_port_insert_right(mach_task_self(), port, port, MACH_MSG_TYPE_MAKE_SEND)
        };
        kr_ok(kr)?;
        let kr = unsafe {
            task_set_exception_ports(
                mach_task_self(),
                EXC_MASK_BAD_ACCESS,
                port,
                (EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES) as i32,
                ARM_THREAD_STATE64 as i32,
            )
        };
        kr_ok(kr)?;

        let server = ExceptionServer { port, pager };
        thread::Builder::new()
            .name("squib-mach-exc".into())
            .spawn({
                let pager = Arc::clone(&server.pager);
                let port = server.port;
                move || run_exception_loop(port, pager)
            })
            .map_err(SquibError::Spawn)?;
        Ok(server)
    }
}

fn run_exception_loop(port: mach_port_t, pager: Arc<Pager>) {
    let mut buf = [0u8; 1024];
    loop {
        // mach_msg(MACH_RCV_MSG, ...). On EXC_BAD_ACCESS, code[0] = KERN_PROTECTION_FAILURE
        // or KERN_INVALID_ADDRESS, code[1] = faulting VA.
        // Decode the MIG exception_raise body, look up the page in `pager`,
        // copy bytes from the snapshot file, mach_vm_protect to RW, reply KERN_SUCCESS.
    }
}
```

The `crash-handler` crate (currently `0.6.x`) is the closest existing prior art — it sets up a task-level exception port for crash reporting and runs a server thread on `mach_msg`. Crashpad's `ExceptionHandlerServer` (C++) is the canonical reference everyone copies.

**Caveats:**
- **`MACH_EXCEPTION_CODES` is mandatory** on 64-bit — without it, `code[1]` is truncated to 32 bits and you lose the high bits of the faulting VA.
- **Exception ports stack**: previous handlers (LLDB, crash reporters) may be installed. Save/forward to keep them working: read with `task_get_exception_ports`, install your own, forward unhandled exceptions to the saved ports.
- **The reply flavor must match the registration flavor.** ARM_THREAD_STATE64 here.
- **Threading**: when an exception is delivered, the kernel suspends the faulting thread until you reply. If your handler thread itself faults trying to page in (recursive fault on the snapshot file), you deadlock. Lock the snapshot file via `mlock` or `mach_vm_wire` before serving.

### 3.3 Wiring postcopy to HVF

The vCPU exit path catches stage-2 faults via `hv_vcpu_run` returning with a data-abort. The handler reads `HPFAR_EL2` / `FAR_EL2` (via `hv_vcpu_get_sys_reg(HV_SYS_REG_FAR_EL2)` plus the IPA from exit info — the exit struct contains the IPA directly), pages in the requested page from the snapshot file, calls `hv_vm_protect` to grant RWX on the page, and re-enters `hv_vcpu_run`. The guest re-issues the access transparently.

`hv_vm_map` does **not** require all backing memory to be present; it registers a host VA range with the guest IPA. As long as the host VA is reserved (even `PROT_NONE`), the call succeeds. Pages can be filled in lazily.

## 4. Snapshot File Format

### 4.1 versionize is dead — bitcode wins

Firecracker **migrated off `versionize` to `bitcode + serde`** during the v1.10–1.11 cycle. The `firecracker-microvm/versionize` repo was archived February 2026; the last release was v0.1.10 (March 2023). `versionize_derive` is in the same state.

The current Firecracker snapshot format is:

| Field | Size | Purpose |
|-------|------|---------|
| `magic_id` | 8 bytes (u64) | Identifies snapshot + arch |
| `version` | variable | MAJOR.MINOR.PATCH |
| `state` | variable | bitcode-encoded state blob |
| `crc` | 8 bytes (u64) | optional CRC64 |

Bitcode's tradeoff: tiny size, fast, but **not backwards-compatible** at the encoding level. Format-breaking changes bump MAJOR. Cross-version restore is handled via explicit data migrations in the loader.

### 4.2 Recommendation for squib

**Use `bitcode + serde` for state framing; reuse Firecracker's magic IDs and outer container.** Specifically:

- Same outer framing (magic, version, state, crc).
- Same arch magic for aarch64 so a Firecracker snapshot-id check succeeds.
- `state` blob structure is squib-defined (HVF sysreg set differs from KVM — see §4.3).
- Pin to `bitcode = "0.6"` (current major), and gate format-breaking schema changes behind a compile-time const that both writes and validates the inner version.
- For backwards compatibility within a single MAJOR, use additive `Option<T>` fields and `#[serde(default)]`. Bitcode tolerates this within a major.

Avoid rolling your own format; the Firecracker container is well-trodden. Avoid `versionize` even though squib aims for Firecracker compatibility — the format-of-bytes inside the `state` blob is opaque to the outer container, and Firecracker itself no longer uses versionize.

### 4.3 Cross-replay: KVM snapshot → HVF restore?

Memory-only replay is plausible. vCPU state is not.

**Memory portion**: `mmap`-friendly raw memory dump (or Firecracker's "diff" sparse file). HVF maps it via `hv_vm_map` exactly like Firecracker's KVM ioctl. This works as long as IPA ranges and guest RAM size match.

**vCPU portion**: HVF and KVM expose different sysreg subsets, both nominally aarch64-v8.x.

What KVM exposes via `KVM_GET_REG_LIST` and `KVM_GET_ONE_REG`:
- Full ARM64 core state (X0-X30, SP, PC, PSTATE).
- FP-SIMD (V0-V31, FPSR, FPCR).
- ~150–200 sysregs depending on host CPU features (TCR_EL1, SCTLR_EL1, MAIR_EL1, VBAR_EL1, ESR_EL1, CNTV_*, ID_AA64*_EL1, etc.).
- KVM-specific virtual regs (timer offsets, GIC regs, MP state).

What HVF exposes via `hv_vcpu_get_sys_reg` and `hv_vcpu_get_reg`:
- Full ARM64 core + FP-SIMD.
- A documented subset of sysregs accessed via `HV_SYS_REG_*` enum constants — close to 100, missing some KVM entries (notably some MPAM and SVE-related regs that Apple Silicon doesn't implement at host EL2 for guests).
- HVF-specific GIC redistributor regs via `hv_gic_*` (macOS 13+).
- No "MP state" object — you start/stop vCPUs by thread control.

**Practical verdict on cross-replay:**
- ARMv8.0 baseline registers + GICv3 distributor/redistributor → portable byte-for-byte after appropriate name mapping.
- Anything from KVM tied to KVM-internal accounting (vtimer offsets relative to KVM's clock baseline, KVM-PV features, custom SVE state) → not portable.
- ID registers — KVM lets the userspace VMM mask features for migration; HVF has `hv_vcpu_config_get_feature_reg` (newer API) for similar effect, but you cannot expose features the host doesn't have.

**Recommended scope for squib day-1**: same-binary save-restore only. Cross-VMM memory-only restore can be a stretch goal; mark Firecracker-compat as "API-compatible, snapshot-not-binary-compatible" and you're not lying.

## 5. Block IO and Network IO Strategy

### 5.1 Block IO

`io_uring` is Linux-only. The macOS landscape:

- **`aio_read`/`aio_write`**: implemented as a thread pool internally on Darwin. No `kqueue` notification (the man page lies). Effectively `spawn_blocking` with worse ergonomics. **Don't use.**
- **`kqueue` for files**: only signals readiness, regular files are always "ready". Useless for true async file IO. **Don't use for block.**
- **`dispatch_io` (libdispatch / GCD)**: Apple's blessed async IO. Internal thread pool plus event coalescing; decent for high-throughput sequential, respectable for random. Rust bindings via `dispatch2 = "0.3"` (current, maintained) and `dispatchr`. The catch: callback-based, not future-based; bridging into tokio requires care.
- **Tokio + `spawn_blocking`** against a sized thread pool: simple, ~5–10 µs overhead per IO op, scales to a few thousand IOPS per core comfortably. What `monoio` falls back to on macOS, and what virtually every macOS-based microVM project ships with.

**Recommendation for squib day-1**: `tokio` with a dedicated `spawn_blocking` pool sized per CPU count, using `pread`/`pwrite` against `F_NOCACHE` (the macOS equivalent of `O_DIRECT`, set via `fcntl`). Per-virtio-blk-queue dedicated thread to keep cache locality. Delivers ~150–200 K IOPS on Apple SSDs without engineering effort.

**Day-2 stretch**: a `dispatch_io` backend gated behind a feature flag for users who need >300 K IOPS. Complexity is in lifecycle management of `dispatch_data_t` and the channel-vs-future bridge. Don't burn day-1 budget.

### 5.2 Network IO — vmnet

`vmnet.framework` is the canonical macOS path for Apple-Silicon-native networking. Userspace API backed by a kernel extension. Modes:

- **Shared mode**: NAT through host. ~1 Gbit/s observed.
- **Bridge mode**: requires entitlement, near-line-rate, but the entitlement is hard to get for general distribution.
- **Host mode**: host-only network.

Anka's published numbers (2024) report **~2 Gbit/s NAT, 4–8 Gbit/s bridge** on M-series. macOS 15.4 added `virtio_net_hdr` improvements that materially help virtio-net throughput.

**Recommendation**: vmnet shared mode for day-1, with a virtio-net frontend in the guest. Don't ship bridge mode without working through Apple entitlement requests. The `vmnet-helper` project (used by lume) demonstrates the pattern; copy it.

For Rust: there's no maintained `vmnet` crate. You'll write FFI to `vmnet_start_interface`, `vmnet_read`, `vmnet_write` against `dispatch_queue`s. ~300 lines of unsafe wrapper, bounded interface — acceptable inside a `#[forbid(unsafe_code)]` workspace by isolating in a `squib-vmnet-sys` sub-crate.

## 6. Code Signing & Distribution

### 6.1 Required entitlements

For HVF use:
- `com.apple.security.hypervisor` — **required**. Without it, `hv_vm_create` returns `HV_DENIED`.
- `com.apple.security.virtualization` — **only required for VZ.framework**. Squib uses HVF directly, so **not needed**. Don't request it; it adds review friction.
- `com.apple.security.cs.allow-jit` — **not needed**. JIT is for the host process generating executable code at runtime; HVF guests run in stage-2-translated guest memory and don't trigger this.
- `com.apple.vm.networking` — required if you call `vmnet.framework`.

These are "open" entitlements: any Developer ID-signed binary can self-claim them — no Apple approval needed. Exception: bridged vmnet historically required a paperwork process.

### 6.2 Signing flow

For local development:
```
codesign --sign - --entitlements squib.entitlements --force --options runtime ./target/release/squib
```
Ad-hoc (`-`) signing suffices for the current user's machine. The hardened runtime (`--options runtime`) is required if you plan to notarize.

For distribution:
1. Sign with Developer ID Application certificate.
2. Hardened runtime + entitlements file.
3. Submit `.zip` or `.pkg` to `notarytool` (`xcrun notarytool submit ...`), wait for approval (~5–60 minutes).
4. Staple: `xcrun stapler staple squib.pkg`.

### 6.3 Distribution channels

- **Homebrew formula**: viable. `vfkit`, `lume`, `krunkit`, `tart` all in homebrew-core or their own taps. As of Sept 2026, Homebrew **requires** notarization for new casks; formulae (CLI tools) are on the same trajectory.
- **Direct download**: notarized `.pkg` with a postinstall that copies the signed binary into `/usr/local/bin`.
- **Mac App Store**: HVF is **not** allowed in sandboxed App Store apps. Don't pursue.

### 6.4 Practical gotchas

- The `.entitlements` plist must be valid XML or `codesign` silently misses entitlements without erroring.
- Multi-architecture binaries: ship a `lipo`d `arm64` only — squib is Apple-Silicon-only, no x86_64 fat slice.
- Notarytool requires an app-specific password or App Store Connect API key; bake into CI as encrypted secrets.
- `lume` and `vfkit` documentation are the cleanest references for "what does a notarized HVF VMM ship look like." Read their CI pipelines.

## 7. Recommended Minimum macOS Version

**macOS 13 Ventura** is the floor I'd argue for, with **macOS 15 Sequoia** strongly recommended.

- **macOS 11 Big Sur**: first Apple Silicon. HVF arm64 was rough, missing `hv_gic_*`. Skip.
- **macOS 12 Monterey**: stable HVF arm64 baseline. Mostly usable but no host GIC. Skip if you can.
- **macOS 13 Ventura** ✓: `hv_gic_create` and friends — host-side GICv3 dramatically reduces interrupt-injection overhead. **Acceptable floor**.
- **macOS 14 Sonoma**: 64 GiB RAM cap (was 63), various stability fixes.
- **macOS 15 Sequoia**: nested virt for M3+, IPA granularity tunable, `hv_vcpu_config_get_feature_reg` for portable feature masking. **Strong recommendation as floor.**
- **macOS 26 Tahoe**: 4 KiB IPA pages, ASIF disk format, vfkit/Containerization perf wins propagating. **Best target.**

**Recommendation: declare macOS 15 as required for `cargo run`, document macOS 13 as best-effort.** Squib's day-1 user is a developer on a recent MacBook, not someone clinging to a 2021 OS.

## 8. Implications for squib's Day-1 Release

### Solved (lift directly from prior art)
- **Boot loop, ESR decode, GIC use** — copy patterns from libkrun and QEMU's HVF arm64 patches.
- **vmnet shared-mode networking** — copy from lume/vfkit-helper.
- **Block IO via tokio + spawn_blocking + F_NOCACHE** — boring, fast enough.
- **Snapshot framing with bitcode+serde** — Firecracker did the format design; mirror it.
- **Code signing & notarization** — well-trodden path; build into CI from day 0.

### Hard but tractable
- **Dirty page tracking via `hv_vm_protect`** — works, ~10–20 µs/page-fault. Engineer for write-locality and 2 MiB granularity. Doable in 2–3 weeks; the trap is TLB-shootdown cost on `hv_vm_protect`, which forces batching strategy choices. Plan for one round of perf-tuning after the naive loop works.
- **Mach exception ports for postcopy paging** — well-understood pattern (crash-handler, crashpad), but MIG message decoding is finicky. Allocate 2–3 weeks. Critical to get saved-port forwarding right or you'll break LLDB attach.
- **Fast boot path** — `pci=off`, lz4 kernel, custom minimal initramfs, `console=hvc0`. Most of the budget goes to kernel decompression and userspace init, not HVF entry/exit. Half-engineer-week to hand-tune a reference Linux kernel config.
- **Snapshot save** — straightforward once dirty tracking works. Stop vCPUs, freeze devices, walk dirty bitmap, write changed pages + sysreg state. ~1–2 weeks.

### Hard, defer past day-1
- **Postcopy lazy snapshot restore over network** — Mach-exception infra is on day-1, but using it for *remote* snapshot streaming is integration work. Day-1 ships local file restore.
- **`dispatch_io` block backend** — defer; tokio + spawn_blocking covers most workloads.
- **Cross-VMM Firecracker memory-only restore** — defer; document as not supported.
- **Bridged vmnet** — defer; requires Apple entitlement coordination.
- **DTrace USDT for VMM internals** — defer; rely on `tracing` + samply for now.
- **Nested virt (M3+ on macOS 15+)** — defer; not a core squib value prop.

### What can wreck the schedule
1. **Underestimating the TLB cost of `hv_vm_protect`.** If your dirty rate is high and granularity is 4 KiB, you'll see >100% slowdown. Have a 2 MiB fallback.
2. **Mach exception port edge cases** with debuggers attached. Test with LLDB attached during snapshot restore.
3. **Notarization on a mature CI**. Apple's notarytool can hang for 1–2 hours occasionally. Don't gate releases on synchronous notarization; do it post-merge.
4. **HVF macOS-version skew**. APIs are weakly versioned; behavior subtly differs between macOS 13 / 14 / 15 / 26. CI matrix should cover at least two.

## Sources

- [hv_vcpu_run_until — Apple](https://developer.apple.com/documentation/hypervisor/hv_vcpu_run_until(_:_:))
- [hv_vm_protect — Apple](https://developer.apple.com/documentation/hypervisor/hv_vm_protect(_:_:_:))
- [hv_vm_protect_space — Apple](https://developer.apple.com/documentation/hypervisor/hv_vm_protect_space(_:_:_:_:))
- [hv_vm_map — Apple](https://developer.apple.com/documentation/hypervisor/hv_vm_map(_:_:_:_:))
- [Hypervisor framework — Apple](https://developer.apple.com/documentation/hypervisor)
- [Firecracker snapshot versioning](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/versioning.md)
- [Firecracker handling page faults on snapshot resume](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/handling-page-faults-on-snapshot-resume.md)
- [Firecracker CHANGELOG](https://github.com/firecracker-microvm/firecracker/blob/main/CHANGELOG.md)
- [versionize crate (archived)](https://github.com/firecracker-microvm/versionize)
- [bitcode crate](https://crates.io/crates/bitcode)
- [containers/libkrun](https://github.com/containers/libkrun)
- [crc-org/vfkit](https://github.com/crc-org/vfkit)
- [Apple containerization framework](https://github.com/apple/containerization)
- [The State of MicroVM Isolation in 2026](https://emirb.github.io/blog/microvm-2026/)
- [ARM VMM with Apple's Hypervisor Framework — Whexy](https://www.whexy.com/posts/simpple_01)
- [QEMU HVF Apple Silicon support patches](https://lore.kernel.org/all/20210916155404.86958-2-agraf@csgraf.de/T/)
- [QEMU HVF allow > 63 GB RAM on macOS 15+](https://lore.kernel.org/all/CA7E2403-A9F6-4B29-B640-13E41D530744@apple.com/T/)
- [crash-handler crate](https://crates.io/crates/crash-handler)
- [Mach Exception Handlers — Mike Ash / Landon Fuller](https://www.mikeash.com/pyblog/friday-qa-2013-01-11-mach-exception-handlers.html)
- [Defeating Anti-Debug: macOS Mach exception ports](https://alexomara.com/blog/defeating-anti-debug-techniques-macos-mach-exception-ports/)
- [task_set_exception_ports — Mach man page](https://web.mit.edu/darwin/src/modules/xnu/osfmk/man/task_set_exception_ports.html)
- [dispatch2 crate](https://lib.rs/crates/dispatch2)
- [tjfontaine/vm_profile_guest](https://github.com/tjfontaine/vm_profile_guest)
- [macOS Tahoe 26 release notes](https://developer.apple.com/documentation/macos-release-notes/macos-26-release-notes)
- [Apple Containerization Framework deep dive — Anil Madhavapeddy](https://anil.recoil.org/notes/apple-containerisation)
- [Anka — Apple Silicon VM networking modes](https://veertu.com/unlocking-superior-macos-vm-network-performance/)
- [Firecracker MicroVMs on MacBook Pro M3](https://u3n.medium.com/the-future-of-development-is-here-running-firecracker-microvms-on-your-macbook-pro-m3-ad6fd3e5092c) (note: nested-Lima, not native)
