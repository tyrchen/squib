---
title: 16-snapshots — bitcode state, sparse memory, dirty tracking, postcopy
type: design
status: draft
last_updated: 2026-05-03
depends_on: 10-data-model.md, 12-hvf-backend.md
---

# 16 · Snapshots — bitcode state, sparse memory, dirty tracking, postcopy

Status: draft · Owner: squib-snapshot · Depends on: [10-data-model.md](./10-data-model.md), [12-hvf-backend.md](./12-hvf-backend.md)

## 1. Purpose

Save and restore a running microVM. Same-host save / restore is a hard requirement (R4). Cross-host (KVM↔HVF) is explicitly **not** supported; HVF and KVM expose different sysreg subsets and different timer / GIC state shapes — see [99-key-decisions.md § D10](./99-key-decisions.md#d10-cross-host-snapshot-not-supported).

The four sub-features:

1. **Full snapshots** — write the entire `MicrovmState` blob and a dense memory dump.
2. **Diff snapshots** — write only changed pages since the last snapshot, gated by `track_dirty_pages: true` in machine-config.
3. **Dirty page tracking** — `hv_vm_protect`-and-fault scheme to identify modified pages between snapshots.
4. **Postcopy / lazy restore** — Mach-exception-port-based on-demand paging at restore time.

## 2. State file

The wire shape is fixed in [10-data-model.md § 6.1](./10-data-model.md#61-state-file-idsnap). The producer:

```text
Save:
  1. Pause every vCPU (VcpuCommand::Pause; wait for ack).
  2. For each vCPU: applevisor::Vcpu::sys_reg_get over the curated sysreg list (see 13-arch-and-boot.md § 3).
  3. Capture GP regs + FP/SIMD regs.
  4. hv_gic_state_create + get_size + get_data → opaque GIC blob.
  5. Snapshot every device's config + virtqueue cursors.
  6. Snapshot MMDS tree + V2 token store.
  7. Encode MicrovmState with bitcode.
  8. Write magic | version | bitcode_blob | crc64 to the state file path.
```

Restore is the symmetric inverse, with one wrinkle: the PSCI state machine is reset to BSP-running / secondaries-Off independent of the saved value, because vCPU thread identity differs between save and restore (HVF affinity contract). The guest sees a `PSCI_FEATURES`-equivalent VM and re-issues `CPU_ON` via its scheduler.

## 3. Memory file

`<id>.mem`. Two flavours per [10-data-model.md § 6.2](./10-data-model.md#62-memory-file-idmem):

- **Full** — `pwrite` every page in `[ram_start, ram_end)` to the dense file.
- **Sparse-of-dirty** — open the file, `lseek` to `(page * PAGE_SIZE)`, `pwrite` only dirty pages. Filesystem `SEEK_HOLE` reads back natural holes.

Memory is **not** part of the state file. They are paired (state references mem) but separate so `firecracker --describe-snapshot` against a squib-produced file can stat the memory file lazily.

## 4. Dirty page tracking

Implemented in `squib-snapshot::dirty`. The mechanism per [docs/research/hvf-performance-and-snapshots.md § 2.2](../docs/research/hvf-performance-and-snapshots.md):

```text
After a clean checkpoint:
  1. Strip HV_MEMORY_WRITE from the entire tracked range
     (single Vm::protect_memory call per region).
  2. Guest writes generate ESR EC=0x24, WnR=1 exits.
     The vCPU exit handler:
       a. computes the page index (bit_idx = (far - ram_start) >> page_shift),
       b. sets bit_idx in a shadow Vec<AtomicU64>,
       c. re-grants HV_MEMORY_WRITE on that page (or 2 MiB block),
       d. resumes the vCPU.
  3. On snapshot:
       a. drain the bitmap atomically (swap with zeros),
       b. write only dirty pages via pwrite at page-aligned offsets.
```

**Granularity is 2 MiB by default**, dropping to 4 KiB only when the dirty-rate heuristic in the tracker says so. This bounds the TLB-shootdown cost — the real performance limiter. Recorded as [99-key-decisions.md § D11](./99-key-decisions.md#d11-dirty-tracking-2mib-default-with-4kib-fallback).

The shadow bitmap is per-RAM-region. For a 4 GiB region at 2 MiB granularity that's 256 bytes; at 4 KiB granularity it's 128 KiB. Either fits comfortably.

The handler re-grants on the **2 MiB block** containing the faulting page (default), so a single MiB-granular write triggers one fault and tolerates 511 subsequent writes within the block at full speed.

## 5. Postcopy / lazy restore

Optional infrastructure ships in 1.0; it powers the `mem_backend.backend_type=Uffd` path in `PUT /snapshot/load`. The implementation per [docs/research/hvf-performance-and-snapshots.md § 3](../docs/research/hvf-performance-and-snapshots.md):

```text
1. Allocate guest RAM with mach_vm_allocate, immediately mach_vm_protect(VM_PROT_NONE).
2. hv_vm_map the region with full RWX
   (host-side PROT_NONE is what causes faults to surface).
3. Register a task-level Mach exception port for EXC_MASK_BAD_ACCESS
   with MACH_EXCEPTION_CODES.
4. A dedicated server thread (squib-host::pager) runs mach_msg(MACH_RCV_MSG)
   and dispatches MIG exception_raise calls.
5. On fault:
     a. copy bytes from snapshot file at the matching offset,
     b. mach_vm_protect(VM_PROT_READ | VM_PROT_WRITE),
     c. reply KERN_SUCCESS.
6. The vCPU exit handler does the same for guest-side stage-2 faults.
```

**Save and forward to prior exception ports** — LLDB attach must keep working. The pager thread, when it does not own the exception type, forwards via `mach_exception_raise` to the prior handler's port. Tested in CI with an `lldb` attach to a running squib.

The page-server protocol mirrors the upstream Firecracker `Uffd` shape: `mem_backend.backend_path` is a UDS the page-server connects to; the server receives page-fault notifications and serves pages. Squib's pager talks to that server only when the snapshot file does not contain the page (i.e. for hot-restore scenarios).

## 6. API surface (mapping to wire endpoints)

| Endpoint | Purpose | State precondition |
|----------|---------|--------------------|
| `PUT /snapshot/create` (Full) | Snapshot a Paused or Running VM, full memory dump | Running or Paused |
| `PUT /snapshot/create` (Diff) | Snapshot dirty pages only since the last clean checkpoint | Running or Paused; `track_dirty_pages: true` |
| `PUT /snapshot/load` (File) | Restore a VM from a state + memory file pair | NotStarted |
| `PUT /snapshot/load` (Uffd) | Restore with a page-server providing pages on demand | NotStarted |
| `--describe-snapshot <path>` | Read state file, print summary | n/a (CLI tool, not API) |

`clock_realtime` is x86_64-only and accepted-and-ignored — see [21-api-compat-matrix.md § 2](./21-api-compat-matrix.md#snapshotload-put).

## 7. Behaviour edges

- **Concurrent dirty-tracking vs running**: the `protect_memory` call to strip writes must serialize with vCPU `run` calls only at the page granularity HVF guarantees (TLB invalidation is per-VA). We allow live dirty tracking; the cost is one fault per first-write per page.
- **vCPU thread identity across restore**: see § 2. PSCI state is normalized; the guest's PSCI driver re-issues `CPU_ON`.
- **GIC state restore**: `hv_gic_state_set_data` must happen before any vCPU runs. The boot orchestrator gates on this.
- **Sparse file cross-FS**: `SEEK_HOLE` is supported on APFS, HFS+, and exFAT, all common Mac filesystems. NFS-mounted snapshots fall back to dense.
- **Postcopy + LLDB**: the pager forwards unhandled exception codes; verified by a CI test that does `lldb -p $(pgrep squib)` and asserts attach succeeds.

## 8. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-SNAP-1 | A Full snapshot, kill, restore round-trip produces a workload-equivalent VM (guest counter continues, file descriptors valid, network reconnects). | CI integration test |
| I-SNAP-2 | A Diff snapshot contains exactly the pages whose dirty bit was set since the last checkpoint, no more, no less. | Property test with synthetic write patterns |
| I-SNAP-3 | Cross-VMM (KVM ↔ HVF) snapshot replay returns `Error::SnapshotIncompatible` cleanly, never UB. | Integration test loading a KVM-produced state file |
| I-SNAP-4 | The pager forwards Mach exceptions it does not own to the prior handler. | LLDB-attach CI test |
| I-SNAP-5 | The state file's CRC64 matches the encoded `magic..state` on every successful save and is verified on every load. | Unit test with bit-flipped state file |
| I-SNAP-6 | A `track_dirty_pages: false` Diff request is rejected with the documented `fault_message`. | Per-endpoint compat test |

## 9. Cross-references

- ← Depends on: [10-data-model.md § 5–6](./10-data-model.md#5-microvmstate--the-snapshot-state-blob), [12-hvf-backend.md § 7](./12-hvf-backend.md#7-memory-mapping)
- → Consumed by: [20-firecracker-api.md](./20-firecracker-api.md) (`/snapshot/create`, `/snapshot/load`, `--describe-snapshot`), [71-performance-budgets.md](./71-performance-budgets.md) (dirty-tracking overhead budget)
- ↔ Related research: [docs/research/hvf-performance-and-snapshots.md](../docs/research/hvf-performance-and-snapshots.md) (every section)
