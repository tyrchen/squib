//! High-level save / load orchestrator.
//!
//! Implements the producer flow from
//! [16-snapshots.md § 2](../../../specs/16-snapshots.md#2-state-file):
//!
//! ```text
//! 1. Open <id>.snap.tmp and <id>.mem.tmp (sibling of the destinations).
//! 2. Encode MicrovmState with bitcode + CRC64 trailer (envelope::Snapshot::save).
//! 3. Write the memory file (Full or sparse-of-dirty).
//! 4. fsync(3) both temp files.
//! 5. rename(2) <id>.snap.tmp → <id>.snap and <id>.mem.tmp → <id>.mem.
//! 6. If either rename fails, unlink any partially-renamed file.
//! ```
//!
//! This module owns the high-level transaction; the building blocks (envelope,
//! atomic-writer, memory writer, dirty bitmap) are independently testable.

use std::path::Path;

use crate::{
    atomic::AtomicWriter,
    dirty::DirtyBitmap,
    envelope::Snapshot,
    error::{Result, SnapshotError},
    memory::{MemoryWriter, PageReader},
    state::MicrovmState,
};

/// Snapshot kind requested by the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    /// Full snapshot — entire memory dumped.
    Full,
    /// Diff snapshot — only dirty pages since the last clean checkpoint.
    Diff,
}

/// Inputs to a save operation.
#[derive(Debug)]
pub struct SaveRequest<'a, R: PageReader> {
    /// Destination path for the state file (`<id>.snap`).
    pub state_path: &'a Path,
    /// Destination path for the memory file (`<id>.mem`).
    pub memory_path: &'a Path,
    /// Snapshot type.
    pub kind: SnapshotKind,
    /// State blob (vCPUs, GIC, devices, MMDS).
    pub state: MicrovmState,
    /// Source for memory bytes.
    pub memory: &'a R,
    /// Logical RAM size.
    pub ram_size: u64,
    /// Memory-file page size (host page; 16 KiB on Apple Silicon).
    pub memory_page_size: u64,
    /// Dirty bitmap — required for `Diff`, ignored for `Full`.
    pub dirty: Option<&'a DirtyBitmap>,
}

/// Save report — emitted after a successful save for tracing / metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveReport {
    /// Snapshot kind that produced this save.
    pub kind: SnapshotKind,
    /// Number of pages written for a Diff snapshot (= 0 for Full, where every page
    /// is written via the dense write path).
    pub pages_written: u64,
}

/// Save the state + memory pair to disk atomically (D25).
///
/// The two-temp-file + two-rename pattern is sequential, not transactional across
/// the pair: a crash between the two renames leaves the operator with the previous
/// good pair plus one stranded temp file. The load path validates both files'
/// magic+CRC and refuses to use a mismatched pair, so this remains safe per § 2.
///
/// # Errors
/// [`SnapshotError`] for any step in the pipeline.
pub fn save<R: PageReader>(req: SaveRequest<'_, R>) -> Result<SaveReport> {
    let SaveRequest {
        state_path,
        memory_path,
        kind,
        state,
        memory,
        ram_size,
        memory_page_size,
        dirty,
    } = req;

    if matches!(kind, SnapshotKind::Diff) && dirty.is_none() {
        return Err(SnapshotError::InvalidPath(
            "Diff snapshot requires track_dirty_pages=true and a dirty bitmap".into(),
        ));
    }
    if matches!(kind, SnapshotKind::Diff) && !state.vm_info.track_dirty_pages {
        return Err(SnapshotError::InvalidPath(
            "Diff snapshot requested but vm_info.track_dirty_pages is false".into(),
        ));
    }
    state.verify_compatible()?;

    // Step 1 — write the state file (small; do this first so the memory write
    // doesn't fight for fsync bandwidth before we know the state encodes).
    let mut state_writer = AtomicWriter::open(state_path)?;
    let envelope = Snapshot::new(state);
    envelope.save(state_writer.file_mut())?;

    // Step 2 — write the memory file.
    let mut mem_writer = MemoryWriter::open(memory_path, ram_size, memory_page_size)?;
    let pages_written = match kind {
        SnapshotKind::Full => {
            mem_writer.write_full(memory)?;
            0
        }
        SnapshotKind::Diff => {
            let bitmap = dirty.expect("checked above");
            mem_writer.write_diff(memory, bitmap)?
        }
    };

    // Step 3 — commit both atomically.
    //
    // Order: state first, memory second. If the memory rename fails after the
    // state rename succeeded, the operator sees a state file pointing at a
    // memory file that doesn't exist — they re-take the snapshot. The previous
    // good *memory* pair is still there; only the *state* pair was overwritten,
    // and a state file without its memory peer is detectable on load (see
    // `load::verify_pair`).
    state_writer.commit()?;
    mem_writer.commit()?;

    Ok(SaveReport {
        kind,
        pages_written,
    })
}

/// Re-export of the on-disk header struct for `--describe-snapshot`.
pub use crate::envelope::SnapshotHdr as Header;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::{
        memory::VecPageReader,
        state::{GicState, MicrovmState, VcpuState, VmInfo},
    };

    fn build_state() -> MicrovmState {
        MicrovmState {
            vm_info: VmInfo {
                mem_size_mib: 256,
                smt: false,
                cpu_template: "V1N1".into(),
                kernel_image_path: "/tmp/vmlinux".into(),
                initrd_path: None,
                boot_args: "console=ttyAMA0 panic=1".into(),
                track_dirty_pages: false,
            },
            vcpu_states: vec![VcpuState::new(0)],
            device_states: crate::state::DeviceStates::default(),
            gic_state: GicState::from_bytes(vec![1, 2, 3, 4, 5, 6, 7, 8]),
            mmds_state: None,
        }
    }

    fn dest_in(dir: &Path, name: &str) -> std::path::PathBuf {
        dir.join(name)
    }

    #[test]
    fn test_should_save_full_snapshot_pair_atomically() {
        let dir = TempDir::new().unwrap();
        let snap = dest_in(dir.path(), "x.snap");
        let mem = dest_in(dir.path(), "x.mem");
        let ram_size: u64 = 32 * 1024;
        let reader = VecPageReader::new(vec![7u8; ram_size as usize]);
        let report = save(SaveRequest {
            state_path: &snap,
            memory_path: &mem,
            kind: SnapshotKind::Full,
            state: build_state(),
            memory: &reader,
            ram_size,
            memory_page_size: 16 * 1024,
            dirty: None,
        })
        .unwrap();
        assert_eq!(report.kind, SnapshotKind::Full);
        assert!(snap.exists());
        assert!(mem.exists());
        assert_eq!(std::fs::metadata(&mem).unwrap().len(), ram_size);
    }

    #[test]
    fn test_should_reject_diff_without_dirty_bitmap() {
        let dir = TempDir::new().unwrap();
        let snap = dest_in(dir.path(), "x.snap");
        let mem = dest_in(dir.path(), "x.mem");
        let mut state = build_state();
        state.vm_info.track_dirty_pages = true;
        let reader = VecPageReader::new(vec![0u8; 32 * 1024]);
        let res = save(SaveRequest {
            state_path: &snap,
            memory_path: &mem,
            kind: SnapshotKind::Diff,
            state,
            memory: &reader,
            ram_size: 32 * 1024,
            memory_page_size: 16 * 1024,
            dirty: None,
        });
        assert!(matches!(res, Err(SnapshotError::InvalidPath(_))));
    }

    #[test]
    fn test_should_reject_diff_when_track_dirty_is_false() {
        let dir = TempDir::new().unwrap();
        let snap = dest_in(dir.path(), "x.snap");
        let mem = dest_in(dir.path(), "x.mem");
        let bm = DirtyBitmap::new(0, 32 * 1024, 16 * 1024).unwrap();
        let reader = VecPageReader::new(vec![0u8; 32 * 1024]);
        let res = save(SaveRequest {
            state_path: &snap,
            memory_path: &mem,
            kind: SnapshotKind::Diff,
            state: build_state(),
            memory: &reader,
            ram_size: 32 * 1024,
            memory_page_size: 16 * 1024,
            dirty: Some(&bm),
        });
        assert!(matches!(res, Err(SnapshotError::InvalidPath(_))));
    }

    #[test]
    fn test_should_save_diff_only_dirty_pages() {
        let dir = TempDir::new().unwrap();
        let snap = dest_in(dir.path(), "x.snap");
        let mem = dest_in(dir.path(), "x.mem");
        let mut state = build_state();
        state.vm_info.track_dirty_pages = true;
        let bm = DirtyBitmap::new(0, 32 * 1024, 16 * 1024).unwrap();
        bm.set_dirty_by_index(1);
        let reader = VecPageReader::new(vec![9u8; 32 * 1024]);
        let report = save(SaveRequest {
            state_path: &snap,
            memory_path: &mem,
            kind: SnapshotKind::Diff,
            state,
            memory: &reader,
            ram_size: 32 * 1024,
            memory_page_size: 16 * 1024,
            dirty: Some(&bm),
        })
        .unwrap();
        assert_eq!(report.pages_written, 1);
        let buf = std::fs::read(&mem).unwrap();
        assert!(buf[..16 * 1024].iter().all(|&b| b == 0));
        assert!(buf[16 * 1024..32 * 1024].iter().all(|&b| b == 9));
    }

    #[test]
    fn test_should_reject_save_when_state_is_incompatible() {
        let dir = TempDir::new().unwrap();
        let snap = dest_in(dir.path(), "x.snap");
        let mem = dest_in(dir.path(), "x.mem");
        let mut state = build_state();
        state.vcpu_states.clear(); // 0-vCPU state is "Incompatible"
        let reader = VecPageReader::new(vec![0u8; 32 * 1024]);
        let res = save(SaveRequest {
            state_path: &snap,
            memory_path: &mem,
            kind: SnapshotKind::Full,
            state,
            memory: &reader,
            ram_size: 32 * 1024,
            memory_page_size: 16 * 1024,
            dirty: None,
        });
        assert!(matches!(res, Err(SnapshotError::Incompatible)));
        assert!(
            !snap.exists(),
            "incompatible state must not stage temp file"
        );
    }

    #[test]
    fn test_should_keep_existing_pair_when_save_fails_during_state_validation() {
        let dir = TempDir::new().unwrap();
        let snap = dest_in(dir.path(), "x.snap");
        let mem = dest_in(dir.path(), "x.mem");
        std::fs::write(&snap, b"prior good state").unwrap();
        std::fs::write(&mem, b"prior good mem").unwrap();
        let mut state = build_state();
        state.vm_info.smt = true; // Incompatible.
        let reader = VecPageReader::new(vec![0u8; 32 * 1024]);
        let _ = save(SaveRequest {
            state_path: &snap,
            memory_path: &mem,
            kind: SnapshotKind::Full,
            state,
            memory: &reader,
            ram_size: 32 * 1024,
            memory_page_size: 16 * 1024,
            dirty: None,
        });
        // Original files untouched.
        assert_eq!(std::fs::read_to_string(&snap).unwrap(), "prior good state");
        assert_eq!(std::fs::read_to_string(&mem).unwrap(), "prior good mem");
    }
}
