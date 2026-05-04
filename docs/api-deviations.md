# API deviations from upstream Firecracker

This document is the operator-facing companion to
[`specs/21-api-compat-matrix.md`](../specs/21-api-compat-matrix.md). Every P / A
/ R row from the compat matrix appears here with a curl-shaped reproduction so
SDKs and orchestrators can sniff-test the deviation independently.

For the test surface, see
[`tests/firecracker-compat/`](../tests/firecracker-compat) — every entry below
has a corresponding test.

> Status legend: **F** full parity (omitted from this doc), **P** parity in
> shape with documented deviation, **A** accept-and-warn no-op, **R** rejected
> with a stable `fault_message`, **squib-only** new endpoint or status code.

## Squib-only response codes

### `504 Gateway Timeout` (D26)

Squib emits 504 when an `ApiAction` exceeds its per-action-class timeout (see
[`specs/70-security.md § 6`](../specs/70-security.md#6-resource-limits)).
Upstream Firecracker has no equivalent because KVM ioctls either complete or
hard-fault — squib needs the 504 to surface "the VMM is wedged" without leaving
the client hanging.

```text
HTTP/1.1 504 Gateway Timeout
Server: Firecracker API
Content-Type: application/json

{"fault_message": "VMM action timed out: PUT /actions {InstanceStart}"}
```

Action remains pending at the VMM (cancelling would leave undefined state).
Orchestrators should retry with the same idempotency key.

## P rows (parity in shape; semantics differ)

### `host_dev_name` on `/network-interfaces/{id}` (P)

Squib accepts any literal-looking TAP name and maps it internally to a vmnet
handle named `squib-tap-<iface_id>`. Linux TAP semantics are not honoured.

```text
PUT /network-interfaces/eth0 HTTP/1.1
Content-Type: application/json

{"iface_id":"eth0","host_dev_name":"tap0"}
```

→ `204 No Content`. Internally squib opens `vmnet` with the operating mode set
by `--network=...`; the host-visible interface name is squib's own.

Reproduction: `tests/firecracker-compat/tests/p_rows.rs::test_p_should_accept_host_dev_name_with_linux_style_tap_value`.

### `cpu-config` aarch64 best-effort (P)

aarch64 `reg_modifiers` and `vcpu_features` are applied via
`hv_vcpu_set_sys_reg` for registers squib owns; unsupported registers warn but
the request still returns 200. x86 fields (`cpuid_modifiers`, `msr_modifiers`,
`kvm_capabilities`) accept-and-warn — see the A-row table below.

## A rows (accept-and-warn no-ops on macOS)

### `/drives/{id}.socket` — vhost-user (A)

```text
PUT /drives/rootfs HTTP/1.1
Content-Type: application/json

{"drive_id":"rootfs","path_on_host":"/tmp/rootfs.img","is_root_device":true,"socket":"/tmp/vhost.sock"}
```

→ `204 No Content` + a one-time log warning:

```text
WARN squib_vmm: vhost-user socket is Linux-only; falling back to in-process block engine
```

Reproduction: `tests/firecracker-compat/tests/a_rows.rs::test_a_should_accept_vhost_user_socket_field`.

### `/machine-config.huge_pages = "2M"` (A)

```text
PUT /machine-config HTTP/1.1

{"vcpu_count":1,"mem_size_mib":256,"huge_pages":"2M"}
```

→ `204 No Content` + `WARN squib_vmm: huge_pages is managed by macOS; flag is no-op`.

### `/snapshot/load.clock_realtime` (A)

x86-only kvmclock setting. Squib accepts it without error (so x86-derived SDK
configs replay verbatim) and ignores the value.

### `/cpu-config` x86 fields (A)

`cpuid_modifiers`, `msr_modifiers`, and `kvm_capabilities` accept-and-warn.

### `--seccomp-filter` / `--no-seccomp` CLI flags (A)

```bash
squib --no-seccomp --api-sock /tmp/squib.sock
```

Logs `info: seccomp options are accepted for Firecracker compatibility but no-op on macOS` once at startup.

### `--enable-pci` CLI flag (A)

Logged as `info: --enable-pci is accepted for compatibility; squib uses virtio-MMIO transport`.

## R rows (rejected with fault_message)

### `/machine-config.smt = true` (R)

```text
PUT /machine-config HTTP/1.1

{"vcpu_count":1,"mem_size_mib":256,"smt":true}
```

→ `400 Bad Request`:

```json
{"fault_message": "Invalid arch field for SMT: SMT not supported on Apple Silicon"}
```

Reproduction: `tests/firecracker-compat/tests/r_rows.rs::test_r_should_reject_smt_true_with_apple_silicon_message`.

### `/actions {action_type:SendCtrlAltDel}` (R)

→ `400 Bad Request`:

```json
{"fault_message": "Invalid action: SendCtrlAltDel is x86-only and not supported on aarch64"}
```

Reproduction: `tests/firecracker-compat/tests/r_rows.rs::test_r_should_reject_send_ctrl_alt_del_with_aarch64_message`.

### Snapshot file format mismatches (R)

Squib's snapshot envelope is bit-identical with upstream's
`Snapshot<MicrovmState>` shape (see `specs/16-snapshots.md` and `specs/10-data-model.md § 6`),
but the embedded vCPU + GIC state is HVF-shaped. Cross-VMM (KVM ↔ HVF) replay
returns:

```json
{"fault_message": "Snapshot rejected: Incompatible (cross-VMM replay not supported; see 99-key-decisions.md § D10)"}
```

`firecracker --describe-snapshot <squib-file>` deserialises the envelope and
prints header / version / CRC; the inner state is opaque.

### Unknown route (R)

Upstream Firecracker collapses unknown URIs to `400` (no 404). Squib does the
same:

```text
GET /no-such-thing HTTP/1.1
```

→ `400 Bad Request`:

```json
{"fault_message": "No such resource: /no-such-thing"}
```

## squib-only CLI flags

### `--network={shared|bridged|host|userspace}` (squib-only)

Selects the host-side networking backend. `shared` is the default; `bridged`
needs the restricted `com.apple.vm.networking` entitlement and is gated behind
the `bridged` cargo feature; `userspace` runs gvproxy as a child process.

See [`specs/30-networking.md`](../specs/30-networking.md) for the host-side
mapping.

### `--snapshot-version`, `--describe-snapshot <path>` (F via squib extensions)

Both flags exist upstream; squib's `--describe-snapshot` reads upstream-format
files where structurally compatible (header / version / CRC verify even when
the inner state is opaque). On CRC failure squib prints the metadata, reports
`crc_ok: NO`, and exits with code 2 — pre-emptively documenting that the file
is corrupt.

## Cross-references

- ← Machine-readable matrix: [`specs/21-api-compat-matrix.md`](../specs/21-api-compat-matrix.md).
- ← Compat suite: [`tests/firecracker-compat/`](../tests/firecracker-compat).
- → Setup notes: [`docs/macos-setup.md`](./macos-setup.md).
