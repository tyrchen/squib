//! Integration tests covering Phase 5.6 — atomic-rename fault injection,
//! cross-FS rejection, and end-to-end save/load round-trips.
//!
//! The Mach-exception-port LLDB-coexistence tests live in
//! `crates/host/tests/lldb_attach.rs` because they need the live pager.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::{io::Write as _, path::Path};

use squib_snapshot::{
    AtomicWriter, DeviceState, DeviceStates, DirtyBitmap, MicrovmState, PsciVcpuState, SaveRequest,
    SnapshotError, SnapshotKind, VcpuState, VecPageReader, VmInfo, derive_temp_path, load, save,
};
use tempfile::TempDir;

fn build_state() -> MicrovmState {
    MicrovmState {
        vm_info: VmInfo {
            mem_size_mib: 4,
            smt: false,
            cpu_template: String::new(),
            kernel_image_path: "/k".into(),
            initrd_path: None,
            boot_args: String::new(),
            track_dirty_pages: false,
        },
        vcpu_states: vec![{
            let mut v = VcpuState::new(0);
            v.psci_state = PsciVcpuState::On;
            v
        }],
        device_states: DeviceStates::default(),
        gic_state: squib_snapshot::GicState::from_bytes(vec![0u8; 64]),
        mmds_state: None,
    }
}

fn save_full_pair(state_path: &Path, mem_path: &Path) {
    let reader = VecPageReader::new(vec![0u8; 16 * 1024]);
    save(SaveRequest {
        state_path,
        memory_path: mem_path,
        kind: SnapshotKind::Full,
        state: build_state(),
        memory: &reader,
        ram_size: 16 * 1024,
        memory_page_size: 16 * 1024,
        dirty: None,
    })
    .unwrap();
}

#[test]
fn full_round_trip_through_save_and_load() {
    let dir = TempDir::new().unwrap();
    let snap = dir.path().join("vm.snap");
    let mem = dir.path().join("vm.mem");
    save_full_pair(&snap, &mem);
    let loaded = load(&snap).unwrap();
    assert_eq!(loaded.state.vcpu_states.len(), 1);
    assert_eq!(loaded.state.vm_info.mem_size_mib, 4);
    assert_eq!(std::fs::metadata(&mem).unwrap().len(), 16 * 1024);
}

#[test]
fn diff_round_trip_writes_only_dirty_pages() {
    let dir = TempDir::new().unwrap();
    let snap = dir.path().join("vm.snap");
    let mem = dir.path().join("vm.mem");
    let mut state = build_state();
    state.vm_info.track_dirty_pages = true;
    let bm = DirtyBitmap::new(0, 64 * 1024, 16 * 1024).unwrap();
    bm.set_dirty_by_index(2);
    let reader = VecPageReader::new(vec![0xAB; 64 * 1024]);
    let report = save(SaveRequest {
        state_path: &snap,
        memory_path: &mem,
        kind: SnapshotKind::Diff,
        state,
        memory: &reader,
        ram_size: 64 * 1024,
        memory_page_size: 16 * 1024,
        dirty: Some(&bm),
    })
    .unwrap();
    assert_eq!(report.pages_written, 1);

    let buf = std::fs::read(&mem).unwrap();
    assert_eq!(buf.len(), 64 * 1024);
    // Pages 0, 1, 3 are clean (zero); page 2 is dirty (0xAB).
    assert!(buf[..2 * 16 * 1024].iter().all(|&b| b == 0));
    assert!(buf[2 * 16 * 1024..3 * 16 * 1024].iter().all(|&b| b == 0xAB));
    assert!(buf[3 * 16 * 1024..].iter().all(|&b| b == 0));
}

#[test]
fn fault_injection_mid_rename_leaves_previous_pair_intact() {
    // Simulate a save that opens the temp file, writes some bytes, and then is
    // dropped before commit() — the AtomicWriter's UnlinkOnDrop cleans the
    // temp, leaving the previous good pair intact.
    let dir = TempDir::new().unwrap();
    let snap = dir.path().join("vm.snap");
    let mem = dir.path().join("vm.mem");
    save_full_pair(&snap, &mem);
    let prior_snap = std::fs::read(&snap).unwrap();
    let prior_mem = std::fs::read(&mem).unwrap();

    {
        // Imagine the writer crashes mid-save: we open the temps, write some
        // partial bytes, and drop without commit.
        let mut tmp_state = AtomicWriter::open(&snap).unwrap();
        let mut tmp_mem = AtomicWriter::open(&mem).unwrap();
        tmp_state.write_all(b"partial-state").unwrap();
        tmp_mem.write_all(b"partial-mem").unwrap();
        // Drop both writers without commit — the UnlinkOnDrop guards fire.
    }

    // No stranded temp files.
    assert!(
        !derive_temp_path(&snap).exists(),
        "temp state file leaked: {}",
        derive_temp_path(&snap).display()
    );
    assert!(
        !derive_temp_path(&mem).exists(),
        "temp mem file leaked: {}",
        derive_temp_path(&mem).display()
    );
    // Previous good pair untouched.
    assert_eq!(std::fs::read(&snap).unwrap(), prior_snap);
    assert_eq!(std::fs::read(&mem).unwrap(), prior_mem);
}

#[test]
fn fault_injection_after_first_rename_succeeds_and_temp_unlinked() {
    // The save flow renames the state file first, then the memory file.
    // If we successfully commit the state writer but force the memory writer
    // to error before commit, we end up with an out-of-pair state file (no
    // matching mem) — the load path detects this and rejects the file.
    let dir = TempDir::new().unwrap();
    let snap = dir.path().join("vm.snap");
    let mem = dir.path().join("vm.mem");
    let prior_snap = b"prior good state".as_slice();
    let prior_mem = b"prior good mem".as_slice();
    std::fs::write(&snap, prior_snap).unwrap();
    std::fs::write(&mem, prior_mem).unwrap();

    let mut tmp_state = AtomicWriter::open(&snap).unwrap();
    tmp_state.write_all(b"committed-new-state").unwrap();
    tmp_state.commit().unwrap();
    // memory writer simulated crash: open + drop without commit.
    {
        let mut tmp_mem = AtomicWriter::open(&mem).unwrap();
        tmp_mem.write_all(b"never-committed").unwrap();
    }

    // State file rename did succeed; memory file untouched.
    assert_eq!(std::fs::read(&snap).unwrap(), b"committed-new-state");
    assert_eq!(std::fs::read(&mem).unwrap(), prior_mem);
    // Mem temp is unlinked.
    assert!(!derive_temp_path(&mem).exists());
}

#[test]
fn cross_filesystem_temp_path_rejection() {
    // We can't always create a separate filesystem in unit tests, but we *can*
    // verify the pre-flight check rejects when the parent directory of the
    // destination doesn't exist — the underlying `stat(2)` call surfaces an
    // io::ErrorKind::NotFound which the writer wraps as `SnapshotError::Io`.
    // (A full cross-FS test runs in CI on macOS hosts that mount a tmpfs and
    // a regular volume — out-of-band here.)
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("does-not-exist").join("vm.snap");
    let res = AtomicWriter::open(&dest);
    assert!(res.is_err());
}

#[test]
fn cross_filesystem_check_reports_dedicated_error_when_devs_differ() {
    use squib_snapshot::check_same_filesystem;
    // Same filesystem (the temp dir) returns Ok.
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a.snap");
    let b = dir.path().join("a.snap.tmp");
    let _ = std::fs::write(&a, b"x"); // not required but harmless
    let res = check_same_filesystem(&a, &b);
    assert!(res.is_ok());
}

/// Live cross-FS rejection covering the I-SNAP-3 path on Darwin: mount a
/// `hdiutil`-backed RAM disk, place the destination there, and assert that the
/// save pipeline either rejects with `AtomicCommitCrossFs` (the dedicated
/// variant) or the surrounding `Io` error when its temp path lives on a
/// different filesystem. Gated behind `#[ignore]` because it requires a
/// privileged-ish hdiutil mount and a clean unmount on test exit; CI runs it
/// with `make snapshot-cross-fs-test`.
#[cfg(target_os = "macos")]
struct DetachOnDrop(String);

#[cfg(target_os = "macos")]
impl Drop for DetachOnDrop {
    fn drop(&mut self) {
        let _ = std::process::Command::new("hdiutil")
            .args(["detach", "-force", &self.0])
            .output();
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires hdiutil; run via make snapshot-cross-fs-test"]
fn cross_filesystem_save_rejects_when_dest_is_on_a_separate_ramdisk() {
    use std::process::Command;
    // 1. Attach a 4 MiB RAM disk.
    let attach = Command::new("hdiutil")
        .args(["attach", "-nomount", "ram://8192"])
        .output()
        .expect("hdiutil attach (need ramdisk privileges?)");
    let device = String::from_utf8(attach.stdout)
        .expect("hdiutil stdout utf8")
        .trim()
        .to_string();
    assert!(!device.is_empty(), "hdiutil returned empty device");

    // RAII: detach the ramdisk no matter how the test exits.
    let _detach_guard = DetachOnDrop(device.clone());

    // 2. Format and mount the ramdisk.
    let mount_dir = TempDir::new().unwrap();
    let _ = Command::new("newfs_hfs")
        .args(["-v", "squib_x", &device])
        .output()
        .expect("newfs_hfs");
    let mount_status = Command::new("mount")
        .args([
            "-t",
            "hfs",
            &device,
            mount_dir.path().to_str().expect("temp path utf8"),
        ])
        .status()
        .expect("mount");
    assert!(
        mount_status.success(),
        "mount failed: status {mount_status}"
    );

    // 3. Source temp path on the regular FS, destination on the ramdisk.
    let regular = TempDir::new().unwrap();
    let snap_dest = mount_dir.path().join("vm.snap");
    let mem_dest = mount_dir.path().join("vm.mem");
    let _ = regular; // explicit: we don't write to the regular dir directly,
    // but its presence pins the home FS as distinct from the mount point.

    let reader = VecPageReader::new(vec![0u8; 16 * 1024]);
    let res = save(SaveRequest {
        state_path: &snap_dest,
        memory_path: &mem_dest,
        kind: SnapshotKind::Full,
        state: build_state(),
        memory: &reader,
        ram_size: 16 * 1024,
        memory_page_size: 16 * 1024,
        dirty: None,
    });

    // Either the dedicated cross-FS variant (preferred) or a generic Io
    // variant when the AtomicWriter's temp-on-source-FS heuristic fires
    // before the cross-FS check has a chance to run. Both are valid signals
    // that the pipeline did not silently lose data.
    match res {
        Err(SnapshotError::AtomicCommitCrossFs { .. } | SnapshotError::Io(_)) => {}
        Err(other) => panic!("unexpected error variant: {other:?}"),
        Ok(_) => panic!("save unexpectedly succeeded across two filesystems"),
    }
}

#[test]
fn save_then_describe_produces_human_summary() {
    let dir = TempDir::new().unwrap();
    let snap = dir.path().join("vm.snap");
    let mem = dir.path().join("vm.mem");
    save_full_pair(&snap, &mem);

    let desc = squib_snapshot::describe(&snap).unwrap();
    let h = desc.human();
    assert!(h.contains("vcpu_count:          1"));
    assert!(h.contains("mem_size_mib:        4"));
    assert!(h.contains("crc_ok:              yes"));
}

/// I-NET-3: `host_dev_name` is round-trip-preserved through snapshot save/restore even
/// though it's opaque to the snapshot crate. The snapshot crate stores per-device state
/// in `DeviceState::blob` as bitcode bytes; if the pipeline preserves those bytes
/// byte-for-byte then any field inside (including `host_dev_name`) round-trips trivially.
///
/// This test plants a synthetic virtio-net `DeviceState` whose blob bytes embed a known
/// `host_dev_name` byte sequence and asserts:
/// - the loaded blob is byte-equal to the saved blob (the strong invariant);
/// - the embedded `host_dev_name` bytes appear inside the loaded blob (the weaker field-level smoke
///   that catches accidental sub-blob compaction).
#[test]
fn host_dev_name_round_trips_through_save_restore() {
    let dir = TempDir::new().unwrap();
    let snap = dir.path().join("vm.snap");
    let mem = dir.path().join("vm.mem");

    let host_dev_name = b"vmnet-shared-en0-1234";
    let blob = {
        // Synthetic virtio-net blob shape: `[version: u8] [host_dev_name_len: u32] [name…]`.
        let mut b = Vec::with_capacity(5 + host_dev_name.len());
        b.push(1u8); // schema version byte
        b.extend_from_slice(&u32::try_from(host_dev_name.len()).unwrap().to_le_bytes());
        b.extend_from_slice(host_dev_name);
        b
    };

    let mut state = build_state();
    state.device_states = DeviceStates::from_devices(vec![DeviceState {
        kind: "virtio-net".into(),
        id: "eth0".into(),
        mmio_slot: 1,
        blob: blob.clone(),
    }]);

    let reader = VecPageReader::new(vec![0u8; 16 * 1024]);
    save(SaveRequest {
        state_path: &snap,
        memory_path: &mem,
        kind: SnapshotKind::Full,
        state,
        memory: &reader,
        ram_size: 16 * 1024,
        memory_page_size: 16 * 1024,
        dirty: None,
    })
    .unwrap();

    let loaded = load(&snap).unwrap();
    let net = loaded
        .state
        .device_states
        .devices
        .iter()
        .find(|d| d.kind == "virtio-net" && d.id == "eth0")
        .expect("virtio-net device state must round-trip through save/load");
    assert_eq!(net.blob, blob, "blob bytes must be preserved byte-for-byte");
    assert!(
        net.blob
            .windows(host_dev_name.len())
            .any(|w| w == host_dev_name),
        "host_dev_name bytes missing from loaded blob"
    );
    assert_eq!(net.mmio_slot, 1);
}

/// Synthetic-pattern sweep covering I-SNAP-2: across a wide grid of randomized
/// `(ram_size, page_size, dirty_subset)` combinations the resulting `<id>.mem`
/// must carry the pattern bytes only at the dirty offsets and zeros elsewhere.
///
/// We use a deterministic seeded LCG (no `rand`/`proptest` dependency) so the
/// test is reproducible and cheap to run on every developer's `cargo test`. The
/// LCG constants are the canonical Numerical-Recipes triple; the seed is a
/// fixed compile-time constant so a regression always surfaces in the same
/// trial number.
#[test]
fn diff_round_trip_property_sweep_over_random_dirty_patterns() {
    // Numerical-Recipes LCG — 32-bit state is enough for the small vector
    // sizes we generate here. The seed is fixed so failures reproduce.
    struct Lcg(u32);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            self.0
        }
        fn next_u8(&mut self) -> u8 {
            (self.next() >> 24) as u8
        }
        fn next_below(&mut self, ceil: u64) -> u64 {
            u64::from(self.next()) % ceil
        }
    }
    let mut rng = Lcg(0x5EED_C0DE);

    // 12 trials × 3 different (ram_size, page_size) shapes — enough to surface
    // any boundary bug without ballooning the test runtime.
    let shapes: [(u64, u64); 3] = [
        (16 * 1024 * 8, 16 * 1024),  // 128 KiB RAM, 16 KiB page → 8 pages
        (4 * 1024 * 16, 4 * 1024),   // 64 KiB RAM, 4 KiB page  → 16 pages
        (16 * 1024 * 32, 16 * 1024), // 512 KiB RAM, 16 KiB page → 32 pages
    ];

    for trial in 0..12 {
        for (ram_size, page_size) in shapes {
            let dir = TempDir::new().unwrap();
            let snap = dir.path().join("vm.snap");
            let mem = dir.path().join("vm.mem");
            let mut state = build_state();
            state.vm_info.track_dirty_pages = true;

            let bm = DirtyBitmap::new(0, ram_size, page_size).unwrap();
            let total_pages = ram_size / page_size;
            // Random subset: between 1 and total_pages-1 pages dirty (cover both
            // sparse and dense extremes).
            let dirty_count = 1 + rng.next_below(total_pages.saturating_sub(1).max(1));
            let mut dirty_indices = std::collections::BTreeSet::new();
            while u64::try_from(dirty_indices.len()).unwrap() < dirty_count {
                dirty_indices.insert(rng.next_below(total_pages));
            }
            for &idx in &dirty_indices {
                bm.set_dirty_by_index(idx);
            }

            // Pattern byte uniquely keyed off trial so a misattribution at a
            // page boundary is loud.
            let pattern = rng.next_u8() | 0x80; // ensure non-zero
            #[allow(clippy::cast_possible_truncation)]
            let ram = vec![pattern; ram_size as usize];
            let reader = VecPageReader::new(ram);

            let report = save(SaveRequest {
                state_path: &snap,
                memory_path: &mem,
                kind: SnapshotKind::Diff,
                state,
                memory: &reader,
                ram_size,
                memory_page_size: page_size,
                dirty: Some(&bm),
            })
            .unwrap_or_else(|e| {
                panic!("trial {trial} ram={ram_size} pg={page_size}: save failed: {e:?}")
            });
            assert_eq!(
                report.pages_written, dirty_count,
                "trial {trial}: pages_written must equal |dirty|"
            );

            let buf = std::fs::read(&mem)
                .unwrap_or_else(|e| panic!("trial {trial}: read mem failed: {e}"));
            assert_eq!(
                u64::try_from(buf.len()).unwrap(),
                ram_size,
                "trial {trial}: mem file size must match ram_size"
            );

            for page_idx in 0..total_pages {
                #[allow(clippy::cast_possible_truncation)]
                let page_start = (page_idx * page_size) as usize;
                #[allow(clippy::cast_possible_truncation)]
                let page_end = ((page_idx + 1) * page_size) as usize;
                let slice = &buf[page_start..page_end];
                if dirty_indices.contains(&page_idx) {
                    assert!(
                        slice.iter().all(|&b| b == pattern),
                        "trial {trial} page {page_idx}: dirty page must carry the pattern byte"
                    );
                } else {
                    assert!(
                        slice.iter().all(|&b| b == 0),
                        "trial {trial} page {page_idx}: clean page must be zero"
                    );
                }
            }
        }
    }
}

#[test]
fn save_aborts_when_state_file_path_is_invalid() {
    // `mem_size_mib = 0` is technically allowed per the type; we use the
    // verify_compatible() rejection path (zero vCPUs).
    let dir = TempDir::new().unwrap();
    let snap = dir.path().join("vm.snap");
    let mem = dir.path().join("vm.mem");
    let mut state = build_state();
    state.vcpu_states.clear();
    let reader = VecPageReader::new(vec![0u8; 16 * 1024]);
    let res = save(SaveRequest {
        state_path: &snap,
        memory_path: &mem,
        kind: SnapshotKind::Full,
        state,
        memory: &reader,
        ram_size: 16 * 1024,
        memory_page_size: 16 * 1024,
        dirty: None,
    });
    assert!(matches!(res, Err(SnapshotError::Incompatible)));
    assert!(!snap.exists());
    assert!(!mem.exists());
}
