//! `squib-snapshot` — bitcode state file, sparse memory file, dirty-page tracking.
//!
//! Implements Phase 5 of the squib roadmap. The crate is split into:
//!
//! - [`error`] — wire-stable [`SnapshotError`] (I-RC-8 in 11 § 7).
//! - [`state`] — `MicrovmState` and child structs (vCPU, GIC, MMDS, devices).
//! - [`envelope`] — `Snapshot<T>` outer container with bitcode + CRC64 trailer.
//! - [`atomic`] — D25 temp-file + fsync + rename writer with cross-FS pre-flight check.
//! - [`memory`] — Full / sparse-of-dirty memory-file writer.
//! - [`dirty`] — `Box<[AtomicU64]>` dirty bitmap + adaptive heuristic (D11 + D21).
//! - [`mod@save`] / [`mod@load`] — high-level orchestrators that tie the pieces together.
//!
//! Cross-references: [16-snapshots.md](../../../specs/16-snapshots.md),
//! [10-data-model.md §
//! 5–6](../../../specs/10-data-model.md#5-microvmstate--the-snapshot-state-blob), [11-runtime-core.
//! md § 6](../../../specs/11-runtime-core.md#6-error-types), [99-key-decisions.md § D5, D11, D21,
//! D25](../../../specs/99-key-decisions.md).
//!
//! # Quality bar
//!
//! `#![forbid(unsafe_code)]` — every public surface is safe Rust. The Mach-exception
//! pager that backs `--mem-backend=Uffd` lives in the sibling `squib-host` crate
//! where the `unsafe` is bounded and reviewed.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Snapshot prose talks about hardware names (CRC64, ECMA-182, GICR, CPU_ON, ...) and
// path types; backticking each adds noise.
#![allow(clippy::doc_markdown)]
// `unwrap_or` on infallible conversions in test fixtures keeps the test surface
// readable without injecting explicit error paths the test does not exercise.
#![allow(clippy::cast_possible_truncation)]
// The snapshot subsystem is synchronous by design: a save quiesces vCPUs, encodes
// state, fsyncs, renames, resumes. Going through `tokio::fs` here would force
// async-fn-coloring on a producer that holds the vCPU pause — and the spec's
// quiesce timeout (1 s) bounds the blocking time. `clippy::disallowed_methods` is
// a runtime-code lint per the project's policy (see `crates/vmm/src/lib.rs`).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod atomic;
pub mod dirty;
pub mod envelope;
pub mod error;
pub mod load;
pub mod memory;
pub mod save;
pub mod state;
pub mod vcpu_save;

pub use atomic::{
    AtomicWriter, TEMP_SUFFIX, UnlinkOnDrop, check_same_filesystem, derive_temp_path,
};
pub use dirty::{
    ADAPTIVE_WINDOW, AdaptiveController, DEFAULT_STEP_DOWN_THRESHOLD, DirtyBitmap, TrackedRegion,
    TrackingGranule,
};
pub use envelope::{
    Crc64Writer, SNAPSHOT_DESERIALIZATION_BYTES_LIMIT, SNAPSHOT_MAGIC_AARCH64, SNAPSHOT_VERSION,
    Snapshot, SnapshotHdr, arch_magic,
};
pub use error::{Result, SnapshotError};
pub use load::{LoadedSnapshot, SnapshotDescription, describe, load};
pub use memory::{MemorySnapshotKind, MemoryWriter, PageReader, VecPageReader};
pub use save::{SaveReport, SaveRequest, SnapshotKind, save};
pub use state::{
    DeviceState, DeviceStates, FpSimdRegs, GicState, GpRegs, MicrovmState, MmdsState,
    PsciVcpuState, VcpuState, VmInfo,
};
pub use vcpu_save::{
    GicRestoreTarget, GicSnapshotSource, MmdsRestoreTarget, MmdsSnapshotSource, VcpuRestoreTarget,
    VcpuSnapshotSource, capture_vcpu_state, normalized_psci_state, restore_vcpu_state,
};
