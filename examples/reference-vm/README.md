# Reference VM — squib end-to-end demo

A minimal aarch64 Linux microVM (kernel + initramfs + busybox userspace)
that exercises every part of squib's boot path:

- HVF vCPU thread + run loop
- MMIO bus dispatching to PL011 (console), virtio-rng, virtio-net
- GIC IRQ delivery from devices to the guest
- FDT consumed by the kernel for memory layout, devices, command line
- MMDS HTTP round-trip via the guest's BusyBox `wget` against the
  link-local interceptor

The VM boots, brings up `eth0`, hits `http://169.254.169.254/...`, prints
the response between `===SQUIB-DEMO===` / `===END===` markers on PL011,
and powers off via PSCI.

## One-time build

```sh
./build.sh
```

This produces `build/Image` (the kernel) and `build/initramfs.cpio.gz`
(busybox userspace + the demo `init`). Both are gitignored.

`build.sh` downloads two artifacts:

- **Kernel** (~30 MB): a known-good aarch64 vmlinux from Firecracker's
  reference S3 bucket. Linux 5.10 with virtio-MMIO, ext4, 9p, console,
  net, rng — the standard microvm config. Override with
  `KERNEL_URL=...` or `KERNEL_VERSION=...`.
- **Static aarch64 BusyBox** (~1 MB): extracted from the Docker Library's
  `busybox:1.36.1-musl` image. Requires Docker. Skip the download by
  placing your own static binary at `build/busybox-aarch64`.

## Run

After building:

```sh
make demo
```

You should see (truncated):

```
[init] mounting pseudofs
[init] waiting for eth0
[init] configuring eth0 -> 169.254.169.50
[init] hitting MMDS at http://169.254.169.254/latest/meta-data/instance-id
===SQUIB-DEMO===
"i-squibdemo"
===END===
[init] OK
```

## Customising

The init script reads two kernel cmdline knobs:

| key | default | meaning |
|---|---|---|
| `squib_demo_ip=` | `169.254.169.50` | guest IPv4 to assign to eth0 |
| `squib_demo_path=` | `/latest/meta-data/instance-id` | URL path to fetch from MMDS |

Add them via boot-args in your VM config:

```json
{
    "boot_args": "squib_demo_ip=10.0.0.5 squib_demo_path=/foo/bar"
}
```

## What's not in this VM

- No real disk — initramfs lives in RAM. virtio-blk is a separate path
  (used by `--rootfs`); see `make demo-rootfs` once landed.
- No DHCP — IP is hard-coded via cmdline.
- No multi-vCPU — `make demo` boots with `vcpu_count=1`.
