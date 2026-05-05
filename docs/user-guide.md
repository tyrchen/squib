# User guide

Squib is a Firecracker-compatible microVM monitor for Apple Silicon. It speaks
the same HTTP API, accepts the same JSON config, and produces the same snapshot
envelope — but runs on top of HVF + `vmnet.framework` instead of KVM + Linux
TAP. This guide is for operators: install it, boot a guest, drive it from an
SDK, snapshot it, snapshot-restore it, and triage when it misbehaves.

If you are looking for the wire-by-wire compatibility table, see
[`api-deviations.md`](./api-deviations.md) and
[`specs/21-api-compat-matrix.md`](../specs/21-api-compat-matrix.md). If you
want to hack on squib, see [`dev-guide.md`](./dev-guide.md).

中文版：[user-guide.zh-CN.md](./user-guide.zh-CN.md)。

## 1. Requirements

| Item | Minimum | Recommended |
|------|---------|-------------|
| Hardware | Apple Silicon (M1/M2/M3/M4) | M2 Pro or later |
| OS | macOS 15 Sequoia | macOS 26 Tahoe |
| Architecture | `aarch64-apple-darwin` only | — |
| Disk | 200 MiB free for the binary + reference VM | 1 GiB if you keep snapshots |
| Network | Outbound HTTPS to fetch the reference kernel | — |

Intel Macs are not supported. There is no `x86_64-apple-darwin` build target,
and there will not be one. Guests are aarch64 Linux only.

## 2. Install

### Option A — `.pkg` installer

Download the signed and notarized installer from the release page, double-click,
and follow the prompts. The installer drops `/usr/local/bin/squib`,
`/usr/local/bin/squib-jail`, and (when bundled) `/usr/local/libexec/squib/gvproxy`.

### Option B — Homebrew

```bash
brew install --HEAD --build-from-source ./dist/homebrew/squib.rb
```

The formula is HEAD-only until the first tagged release; after tagging it
points at the notarized `.pkg`.

### Option C — Build from source

```bash
git clone https://github.com/tyrchen/squib && cd squib
make sign                # build + ad-hoc-sign with com.apple.security.hypervisor
./target/aarch64-apple-darwin/release/squib --version
```

Local development requires the codesignature because HVF refuses to initialise
without `com.apple.security.hypervisor`. `make sign` handles that with an
ad-hoc identity (`-`); releases override `SIGN_ID` with a Developer ID hash.

For deeper detail on entitlements and signing failures see
[`macos-setup.md`](./macos-setup.md).

## 3. First boot — the reference VM

The reference VM is a 256 MiB busybox-on-initramfs guest that exercises every
piece of the boot path: HVF vCPU thread, GIC, virtio-mmio bus, PL011 console,
MMDS round-trip, PSCI shutdown.

```bash
make build-reference-vm   # downloads vmlinux + busybox into examples/reference-vm/build/
make demo                 # codesigns + boots + asserts the MMDS handshake
```

You should see a serial transcript ending in:

```
[init] hitting MMDS at http://169.254.169.254/latest/meta-data/instance-id
===SQUIB-DEMO===
"i-squibdemo"
===END===
[init] OK
```

If `make demo` fails before the kernel banner, you almost certainly hit a
codesigning issue — see [§ 9 Troubleshooting](#9-troubleshooting).

## 4. Driving squib from a client

There are three equivalent ways to drive squib: a static config file, raw
`curl` against the UDS, or a Firecracker SDK.

### Static config file

```bash
squib --config-file examples/reference-vm/config.json
```

Squib replays the config in upstream Firecracker order (boot-source → drives →
network-interfaces → mmds-config → machine-config) and then calls
`InstanceStart` on your behalf. Add `--no-api` to skip binding the UDS once
replay finishes.

### Raw HTTP over a UDS

```bash
squib --api-sock /tmp/squib.sock &

curl --unix-socket /tmp/squib.sock http://localhost/version
# {"firecracker_version":"1.16.0","squib_version":"0.1.0"}

curl --unix-socket /tmp/squib.sock -X PUT http://localhost/boot-source \
  -H 'Content-Type: application/json' \
  -d '{"kernel_image_path":"/path/to/Image","boot_args":"console=ttyAMA0"}'

curl --unix-socket /tmp/squib.sock -X PUT http://localhost/actions \
  -H 'Content-Type: application/json' \
  -d '{"action_type":"InstanceStart"}'
```

Every request shape, status code, and `{"fault_message": "..."}` body matches
upstream. The `Server: Firecracker API` header is preserved verbatim so SDKs
that sniff it accept squib.

### SDKs

`firectl`, `firecracker-go-sdk`, `firecracker-containerd`, and
`weaveworks/ignite` drive squib unmodified. Point them at the squib binary
or socket and they cannot tell the difference, except for the documented
[deviations](./api-deviations.md).

```bash
firectl --firecracker-binary "$(which squib)" \
  --kernel-image "$PWD/examples/reference-vm/build/Image" \
  --kernel-opts "console=ttyAMA0 reboot=k panic=1"
```

## 5. Networking

Pick the mode that matches the entitlement you have and the network shape you
need:

| `--network=` | Entitlement needed | What you get | Throughput |
|--------------|--------------------|--------------|------------|
| `shared` (default) | `com.apple.security.hypervisor` (self-claimable) | NAT'd guests with internet access | ≥ 1 Gbit/s |
| `host` | same | Host-only network, no internet | n/a |
| `bridged` | `com.apple.vm.networking` (restricted) | Guest gets an L2 address on your physical network | ≥ 1 Gbit/s |
| `userspace` | none | gvproxy as a child process | ~300–400 Mbit/s |

`shared` is what you want unless you know otherwise. `bridged` requires Apple
DTS approval for the restricted entitlement and ships disabled — flip the
`bridged` cargo feature and re-sign with the entitlement-bearing identity.
`userspace` is the escape hatch for locked-down corporate machines.

The `host_dev_name` field on `PUT /network-interfaces/{id}` is preserved on
the wire (so existing configs round-trip) but mapped internally to a vmnet
handle named `squib-tap-<iface_id>`. Linux TAP names have no host effect.

## 6. MMDS

The microVM metadata service lives at `169.254.169.254` exactly as it does in
upstream Firecracker. Squib accepts both V1 (open `GET`) and V2 (IMDSv2 with a
session token):

```bash
# Seed the data store
curl --unix-socket /tmp/squib.sock -X PUT http://localhost/mmds \
  -H 'Content-Type: application/json' \
  -d '{"latest":{"meta-data":{"instance-id":"i-squibdemo"}}}'

# Bind it to a network interface
curl --unix-socket /tmp/squib.sock -X PUT http://localhost/mmds/config \
  -H 'Content-Type: application/json' \
  -d '{"version":"V2","network_interfaces":["eth0"]}'
```

Inside the guest:

```sh
TOKEN=$(wget -qO- --method=PUT --header='X-metadata-token-ttl-seconds: 60' \
  http://169.254.169.254/latest/api/token)
wget -qO- --header="X-metadata-token: $TOKEN" \
  http://169.254.169.254/latest/meta-data/instance-id
```

JSON Pointer traversal works the same way it does upstream.

## 7. Snapshots

Squib's snapshot envelope is byte-identical to upstream Firecracker
(`bitcode::serialize(Snapshot { header, data: MicrovmState })` + 8-byte CRC-64
trailer). The *contents* of `MicrovmState` are HVF-shaped — different sysreg
set, different GIC blob — and are not portable to or from KVM.

### Save

```bash
curl --unix-socket /tmp/squib.sock -X PATCH http://localhost/vm \
  -H 'Content-Type: application/json' -d '{"state":"Paused"}'

curl --unix-socket /tmp/squib.sock -X PUT http://localhost/snapshot/create \
  -H 'Content-Type: application/json' \
  -d '{"snapshot_path":"/tmp/vm.snap","mem_file_path":"/tmp/vm.mem","snapshot_type":"Full"}'
```

`Diff` snapshots use HVF's `hv_vm_protect` to track dirty pages between saves.

### Inspect

```bash
squib --describe-snapshot /tmp/vm.snap
# magic: 0x07101984_AAAA_0000
# version: 1.0.0
# crc_ok: YES
```

If the CRC check fails the same command prints the metadata, reports
`crc_ok: NO`, and exits with code 2.

### Restore

```bash
squib --api-sock /tmp/squib2.sock &

curl --unix-socket /tmp/squib2.sock -X PUT http://localhost/snapshot/load \
  -H 'Content-Type: application/json' \
  -d '{"snapshot_path":"/tmp/vm.snap","mem_backend":{"backend_type":"File","backend_path":"/tmp/vm.mem"}}'
```

Postcopy via Mach exception ports (`pager-live-mach`) is the default —
unfaulted pages stream in lazily on first access, so restore returns control
to the API in a few hundred milliseconds even for multi-GiB VMs.

## 8. CLI quick reference

The full CLI surface lives in [`specs/50-cli.md`](../specs/50-cli.md). The
day-to-day knobs:

| Flag | Default | Purpose |
|------|---------|---------|
| `--api-sock <path>` | `/run/firecracker.socket` | UDS the API server binds |
| `--id <id>` | `anonymous` | microVM instance id |
| `--config-file <path>` | — | Replay a static config file |
| `--no-api` | — | Replay only, do not bind the UDS |
| `--metadata <path>` | — | Seed MMDS at startup |
| `--log-path <path>` | stderr | File or FIFO target for tracing |
| `--level <lvl>` | `Info` | `Off` / `Error` / `Warning` / `Info` / `Debug` / `Trace` |
| `--network <mode>` | `shared` | `shared` / `host` / `bridged` / `userspace` |
| `--gvproxy-path <path>` | `$SQUIB_GVPROXY_PATH` | Override the bundled gvproxy |
| `--snapshot-version` | — | Print the supported snapshot format version, exit |
| `--describe-snapshot <path>` | — | Inspect a snapshot file, exit |

Linux-only flags (`--seccomp-filter`, `--no-seccomp`, `--enable-pci`, the x86
`cpu_template` values, `huge_pages: "2M"`) are accepted and warned exactly
once per startup so existing launchers do not break. See
[`api-deviations.md`](./api-deviations.md) for the full list.

## 9. Troubleshooting

**`HV_ERROR` / `EX_NOPERM` immediately after start.** The binary is missing
the hypervisor entitlement. Check:

```bash
codesign --display --entitlements - "$(which squib)"
```

If `com.apple.security.hypervisor` isn't bound, re-run `make sign`.

**`make demo` boots but the MMDS assertion never fires.** The reference kernel
has no PL011 console driver — the kernel boots but earlycon never binds, so
serial output is silent. Use `make hvf-test` for a more diagnostic path; the
in-DRAM ringbuffer is dumped on test failure.

**`PUT /actions {InstanceStart}` returns `400 fault_message: "VMM not yet wired"`.**
You are running a Phase < 1.6 build where the API surface is up but the HVF
backend has not been linked yet. Pull `master` or wait for the next tag.

**`504 Gateway Timeout` on a long-running call.** Squib emits 504 when an
ApiAction exceeds its per-class timeout — see
[`api-deviations.md § Squib-only response codes`](./api-deviations.md#squib-only-response-codes).
The action stays pending; clients should retry with the same idempotency key.

**`--network=bridged` fails with "requires `--features bridged`".** You are on
the default build. Bridged mode needs the restricted
`com.apple.vm.networking` entitlement and a re-signed binary; until then use
`--network=shared` (NAT) or `--network=userspace` (gvproxy).

**Quarantine attribute on a downloaded binary.** macOS may refuse to launch
a binary downloaded over the browser. Strip the attribute:

```bash
xattr -d com.apple.quarantine /path/to/squib
```

For anything else, run with `--level Debug --show-log-origin` and grep
`tracing` output for the failing subsystem (`squib_hv`, `squib_vmm`,
`squib_api`, `squib_net`, `squib_snapshot`).

## 10. Where to go next

- [`api-deviations.md`](./api-deviations.md) — every behavior that differs
  from upstream Firecracker, with curl reproductions.
- [`macos-setup.md`](./macos-setup.md) — entitlements, codesigning failures,
  network-mode tradeoffs in depth.
- [`perf/index.md`](./perf/index.md) — published per-revision boot, snapshot,
  and memory numbers.
- [`dev-guide.md`](./dev-guide.md) — building, testing, and contributing to
  squib.
- [`specs/`](../specs/index.md) — the design corpus.
