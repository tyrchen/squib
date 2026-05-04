# Boot-time tuning levers

Per [`specs/71-performance-budgets.md § 2`](../../specs/71-performance-budgets.md#2-targets-10),
P1 targets **p50 boot to `/sbin/init` ≤ 400 ms** on M2 Pro / M3. This document
enumerates every tuning lever known to move the number plus their measured (or
estimated) impact, so the next round of optimisation can pick the right knob
first.

The numbers below are *estimates* until the live boot loop lands and
`crates/vmm/benches/boot.rs` is upgraded to record real boot-to-init time via
the boot-timer device (`14 § 4.8`). Estimates come from libkrun / Firecracker
literature and the squib substrate measurements under `docs/perf/<sha>/`.

## Lever 1 — Kernel image

| Lever | Estimated saving | Where |
|-------|-----------------:|-------|
| Strip `CONFIG_DEBUG_*` / lockdep / KASAN | 50–80 ms | Kernel build |
| Drop `CONFIG_PCI` (squib uses virtio-MMIO regardless) | 10–30 ms | Kernel build |
| `CONFIG_BLOCK=y CONFIG_BLK_DEV_INITRD=y` only — no `MD`, `DM`, etc. | 5–10 ms | Kernel build |
| Drop unused console drivers; keep PL011 + `earlycon` only | 5–15 ms | Kernel build |
| `CONFIG_RANDOMIZE_BASE=n` (KASLR adds early init) | 5–10 ms | Kernel build |

Squib's reference VM (`examples/reference-vm/`) downloads the
Firecracker-published aarch64 vmlinux. That kernel is already heavily trimmed —
the levers above are next-step territory if we ship our own kernel image.

## Lever 2 — Initramfs

| Lever | Estimated saving | Where |
|-------|-----------------:|-------|
| Replace gzipped initramfs with uncompressed `cpio` | 10–20 ms (decompression saved; counter-balanced by larger memory copy) | `examples/reference-vm/build.sh` |
| Strip the initramfs to busybox + minimal `/init` | 5–10 ms | `examples/reference-vm/build.sh` |
| Drop dynamic linker (statically link busybox) | 5–10 ms | Use `busybox-static` | `examples/reference-vm/build.sh` |

The current reference VM uses Alpine's musl-built busybox + `ld-musl-aarch64`.
A statically-linked busybox would shave the dynamic-linker resolve cost.

## Lever 3 — Kernel command line

Squib already passes the minimum-viable cmdline via `examples/reference-vm/init`.
Per [`specs/13-arch-and-boot.md § 6.1`](../../specs/13-arch-and-boot.md#61-boot-args-composition):

- `console=ttyAMA0` and `panic=1` are appended automatically only if the user
  doesn't set them.
- `quiet` / `loglevel=4` removes printk noise; saves ~5–15 ms.
- `init=/sbin/init` skips `/init` → `/sbin/init` lookup; saves ~1 ms.

Recommended baseline cmdline for boot-time-sensitive guests:

```text
console=ttyAMA0 panic=1 quiet loglevel=4 init=/sbin/init reboot=t
```

## Lever 4 — Memory layout

Per [`specs/13-arch-and-boot.md § 2`](../../specs/13-arch-and-boot.md#2-memory-layout-concrete)
and the const-evaluated D22 overlap-check in `crates/arch/src/layout.rs`, the
memory layout is fixed. The only relevant boot-time lever here is RAM size —
small VMs cold-boot faster (less to zero).

| Lever | Estimated saving | Where |
|-------|-----------------:|-------|
| 128 MiB → 256 MiB stays sub-linear (kernel zeroes only what it touches) | < 5 ms | API config |

The layout itself is not a tuning knob.

## Lever 5 — Device probe ordering

The FDT lists virtio-MMIO slots in a fixed order (`14 § 4`). The kernel probes
serially. Drivers we don't need can be omitted from the FDT (saves ~1–3 ms per
unused slot).

| Lever | Estimated saving | Where |
|-------|-----------------:|-------|
| Skip emitting unused MMIO slots | 1–3 ms / slot | `crates/fdt/src/lib.rs::FdtBuilder` |

## Lever 6 — Boot-timer device

The boot-timer (`14 § 4.8`) is a pure measurement device — exposing it in the
FDT adds nothing to the boot envelope. Its job is to record `monotonic_now() -
InstanceStart` from inside the guest, which is the canonical P1 number.

When the live boot path lands, `crates/vmm/benches/boot.rs` wires the boot-timer
device into the criterion harness so each bench iteration drives a real cold
boot and records the timer's first read.

## What we'll measure once Phase 1 tail lands

- p50 / p99 boot-to-`/sbin/init` for the reference VM.
- Per-stage breakdown: HVF init / kernel decompress / kernel execute /
  initramfs unpack / userspace start.
- Effect of each lever above; either confirm the estimate or amend this file
  with the real number.

Until then, the only honest claim is the **substrate** — every isolated
component is well within budget. See [`index.md`](./index.md) for the
substrate measurements at `a559e57`.

## Cross-references

- ← Targets: [`specs/71-performance-budgets.md`](../../specs/71-performance-budgets.md).
- ← Boot orchestration: [`specs/13-arch-and-boot.md`](../../specs/13-arch-and-boot.md).
- ← Boot-tail tracking: [`specs/93-improvements-review.md`](../../specs/93-improvements-review.md) under "Phase 1 (lands at end of Phase 1.6)".
- → Reference VM build: [`examples/reference-vm/build.sh`](../../examples/reference-vm/build.sh).
