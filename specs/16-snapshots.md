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
Save (atomic-or-rollback):
  1. Quiesce: send VcpuCommand::Pause to every vCPU; wait for ack.
     If any vCPU fails to ack within 1 s, abort the save with
     SnapshotError::QuiesceTimeout, resume the others, return 4xx.
  2. Open <id>.snap.tmp and <id>.mem.tmp (sibling of the destinations,
     same filesystem so the final rename is atomic).
  3. For each vCPU: applevisor::Vcpu::sys_reg_get over the curated sysreg list
     (see 13-arch-and-boot.md § 3).
  4. Capture GP regs + FP/SIMD regs.
  5. hv_gic_state_create + get_size + get_data → opaque GIC blob.
  6. Snapshot every device's config + virtqueue cursors.
  7. Snapshot MMDS tree + V2 token store.
  8. Encode MicrovmState with bitcode.
  9. Write the bitcode blob to <id>.snap.tmp via a CRC64Writer wrapper;
     the writer appends the trailing 8-byte CRC.
 10. Write the memory image (Full or sparse-of-dirty) to <id>.mem.tmp.
 11. fsync(3) both temp files.
 12. rename(2) <id>.snap.tmp → <id>.snap and <id>.mem.tmp → <id>.mem.
     If either rename fails, unlink any partially-renamed file and surface
     SnapshotError::AtomicCommitFailed.
 13. Resume vCPUs (or leave Paused if resume_vm = false).

Failure / cancellation:
- If the API client drops the connection mid-save, the controller still
  drives steps 1–13 to completion (a snapshot is a single transaction,
  not a stream). The 204 just gets discarded.
- If steps 3–11 fail, both temp files are unlinked; the destination
  files are untouched.
```

Restore is the symmetric inverse, with one wrinkle: the PSCI state machine is reset to BSP-running / secondaries-Off independent of the saved value, because vCPU thread identity differs between save and restore (HVF affinity contract). The guest sees a `PSCI_FEATURES`-equivalent VM and re-issues `CPU_ON` via its scheduler.

The "atomic-or-rollback" rule above (steps 11–12) is what makes a half-disk-full host safe: a previous snapshot pair is never observably corrupted by a failed new snapshot. Recorded as [99-key-decisions.md § D25](./99-key-decisions.md#d25-snapshot-save-is-atomic-via-temp-file-and-rename).

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
       a. computes the page index (bit_idx = (far - ram_start) >> tracking_shift),
       b. sets bit_idx in a shadow Box<[AtomicU64]> bitset (fetch_or, Relaxed),
       c. re-grants HV_MEMORY_WRITE on that page (or 2 MiB block),
       d. resumes the vCPU.
  3. On snapshot:
       a. drain the bitmap atomically (per-word swap(0, Acquire)),
       b. write only dirty pages via pwrite at page-aligned offsets.
```

### 4.0 TLB cost on live guests

`hv_vm_protect` invalidates stage-2 TLB entries for the affected IPA range across all vCPUs. On Apple Silicon, this is a system-wide DSB + TLBI sequence; for a 4 GiB range stripped in one call, observed cost on M2 Pro is ~120 µs of vCPU stall per affected vCPU thread (measured in week 1 of Phase 5). That stall is paid **once per checkpoint**, not per page — the page-granularity work is the per-fault re-grant in step 2c, which only stalls the vCPU that took the fault. The choice to strip at region granularity (not per-page) is what makes this affordable; per-page strip would TLB-shoot once per page and is unworkable.

### 4.1 Granularity — host page vs tracking page vs HVF stage-2 granule

Three sizes interact, all expressed in `squib-arch::layout::PageGeometry`:

| Size | Source | Value on Apple Silicon |
|------|--------|------------------------|
| `HOST_PAGE_SIZE` | `getconf PAGE_SIZE` / `sysconf(_SC_PAGESIZE)` | **16 KiB** (Apple Silicon native; we do not assume 4 KiB) |
| `HVF_STAGE2_GRANULE` | smallest range `hv_vm_protect` will actually act on | 16 KiB (matches the host granule on Apple Silicon; `hv_vm_protect` rounds up to this) |
| `TRACKING_PAGE_SIZE` | squib's chosen dirty-tracking unit | **2 MiB default**, may step down to 16 KiB for hot regions (per the heuristic in § 4.2) |

The tracking unit is *not* "4 KiB" as some Linux-derived prior art suggests — Apple Silicon hosts use 16 KiB pages, and any granule strictly smaller than the host page is a fiction (`hv_vm_protect` will silently round up). Callers reading `page_shift` in code should use `TRACKING_PAGE_SIZE.trailing_zeros() as u8`; the constant is centralized so the bitmap math, the FAR-to-bit-index calculation, and the snapshot writer share one source of truth.

Granularity choice is recorded as [99-key-decisions.md § D11](./99-key-decisions.md#d11-dirty-tracking-2-mib-default-with-host-page-fallback) (D11 supersedes the "4 KiB" wording with "minimum-host-page = 16 KiB" — the spirit of D11 was "trade tracking precision against TLB cost," not the literal page sizes).

### 4.2 Bitmap sizing and adaptive heuristic

Shadow bitmap is per-RAM-region. At 2 MiB tracking pages: 4 GiB region → 2048 bits = 256 bytes. At 16 KiB tracking pages: 4 GiB region → 256 K bits = 32 KiB. Either fits comfortably in cache.

The handler re-grants on the **whole 2 MiB tracking block** containing the faulting page (default), so a single 16 KiB-granular write triggers one fault and the next 127 writes in the same block proceed at full speed. The adaptive step-down to 16 KiB tracking happens per-RAM-region when, in a 100 ms sliding window, the per-block fault rate exceeds 32 — i.e. the workload is touching most of a 2 MiB block per second and the over-counting in the Diff snapshot is paying for itself. The threshold (32 faults / 100 ms / 2 MiB block) is configurable via `[squib].snapshot.dirty_step_down_threshold` for tuning, with the default frozen as the first ship value so future contributors do not silently change perf characteristics.

## 5. Postcopy / lazy restore

Optional infrastructure ships in 1.0. It powers `PUT /snapshot/load` with **two distinct** `mem_backend.backend_type` values — `File` and `Uffd` — selected unambiguously by the request, never by a runtime heuristic.

The implementation per [docs/research/hvf-performance-and-snapshots.md § 3](../docs/research/hvf-performance-and-snapshots.md):

```text
1. Allocate guest RAM with mach_vm_allocate, immediately mach_vm_protect(VM_PROT_NONE).
2. hv_vm_map the region with full RWX
   (host-side PROT_NONE is what causes faults to surface).
3. Register a task-level Mach exception port for EXC_MASK_BAD_ACCESS
   with MACH_EXCEPTION_CODES.
4. A dedicated server thread (squib-host::pager) runs mach_msg(MACH_RCV_MSG)
   and dispatches MIG exception_raise calls.
5. On fault:
     a. resolve the page from the configured page source (see § 5.1),
     b. mach_vm_protect(VM_PROT_READ | VM_PROT_WRITE),
     c. reply KERN_SUCCESS.
6. The vCPU exit handler does the same for guest-side stage-2 faults.
```

**Save and forward to prior exception ports** — LLDB attach must keep working in **both directions** (squib registers first then lldb attaches; lldb attaches first then squib registers). The pager:

1. At `task_set_exception_ports(EXC_MASK_BAD_ACCESS, our_port, ...)` time, calls `task_swap_exception_ports` (not `task_set_exception_ports`) so the previous handler (kernel default, or LLDB's port) is captured.
2. On every received message, attempts to resolve as a postcopy fault. If the IPA does **not** fall in any registered postcopy region, the pager forwards the exception via `mach_exception_raise_state_identity` (the MIG-generated forwarding shim) to the saved prior port and returns whatever `KERN_*` value the prior handler produced.
3. If LLDB attaches **after** squib has called `task_swap_exception_ports`, LLDB's attach overwrites our handler. We catch this by re-reading the current ports on every `mach_msg` timeout (1 s default) and re-installing if they have drifted; this preserves both squib's faulting semantics and LLDB's debug session, at the cost of a one-second window where postcopy faults panic the guest. Documented in `docs/macos-setup.md` as "attach LLDB before booting the postcopy guest, not during."

Tested in CI with two scenarios: (a) `lldb -p $(pgrep squib)` *during* postcopy load — must preserve both; (b) `lldb -p $(pgrep squib)` *before* `PUT /snapshot/load` — must preserve both.

### 5.1 Page source — `File` vs `Uffd` are different backends, not modes of one

Both backends use the same Mach-exception fault path (§ 5 step 5). They differ only in *where the page bytes come from*:

- **`backend_type = "File"`** — `backend_path` is the path to the memory file from the snapshot pair. The pager `pread`s the page at `(ipa - ram_start)` and serves it directly. This is the simple "fast restore from local files" case.
- **`backend_type = "Uffd"`** — `backend_path` is a UDS that an external page-server is listening on. The pager opens the UDS, sends a fault message in the upstream-Firecracker `Uffd` wire shape (page index, count), receives the page bytes back, then services the fault. The page-server is the *sole* source of pages; squib never reads its own snapshot file in this mode. This is the "page from a remote / lazy-loading store" case (e.g. live-migration receivers, page warmers).

Picking the right backend is the operator's call at `PUT /snapshot/load` time. Squib never falls back from one to the other — that ambiguity is exactly what an operator does not want when their page-server has a bug.

### 5.2 Pre-warming

Before vCPU 0 runs, the pager pre-faults a small set of "boot-critical" pages so the first guest cycles are not all blocking on the pager:

- The kernel's `_text` and `_stext` neighbourhoods (resolved from the FDT-recorded kernel load address + a fixed 2 MiB window).
- The guest's stack page for vCPU 0 (resolved from the saved `SP_EL1` in the restored vCPU state).
- The page containing the FDT.

The boot orchestrator passes these three IPAs to the pager via the same `Vm::map_memory` call sequence that establishes the postcopy region; no separate side-channel.

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
