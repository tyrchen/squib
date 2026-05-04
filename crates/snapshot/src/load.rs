//! High-level load orchestrator.
//!
//! Symmetric to [`crate::save::save`]. Validates magic, version, and CRC; loads the
//! state blob; verifies structural compatibility; surfaces the result to the caller
//! who then drives `hv_gic_state_set_data` and the per-vCPU register restore.

use std::{fs::File, io::BufReader, path::Path};

use crate::{
    envelope::{SNAPSHOT_VERSION, Snapshot},
    error::{Result, SnapshotError},
    state::MicrovmState,
};

/// What `load` returns: the state blob plus the version embedded in the file.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedSnapshot {
    /// State blob.
    pub state: MicrovmState,
    /// Version this state file was produced by.
    pub source_version: semver::Version,
}

/// Load and validate a state file.
///
/// # Errors
/// Any [`SnapshotError`] from envelope decode + structural verification.
pub fn load(state_path: &Path) -> Result<LoadedSnapshot> {
    let file = File::open(state_path)?;
    let mut reader = BufReader::new(file);
    let snapshot = Snapshot::<MicrovmState>::load(&mut reader)?;
    snapshot.data.verify_compatible()?;
    Ok(LoadedSnapshot {
        state: snapshot.data,
        source_version: snapshot.header.version,
    })
}

/// What `--describe-snapshot` prints. Captures the same fields a Firecracker user
/// would expect:
///
/// - file path
/// - magic + version
/// - vCPU count
/// - mem_size_mib
/// - dirty-page tracking flag
/// - whether the matching memory file looks present
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // each bool reports an independent condition
pub struct SnapshotDescription {
    /// Path to the state file.
    pub state_path: std::path::PathBuf,
    /// Magic value embedded in the file.
    pub magic: u64,
    /// Version embedded in the file.
    pub version: semver::Version,
    /// Squib's expected version (for the cross-version note).
    pub expected_version: semver::Version,
    /// Number of vCPUs the snapshot saved.
    pub vcpu_count: usize,
    /// Configured guest RAM size in MiB.
    pub mem_size_mib: u64,
    /// Whether dirty-page tracking was enabled at save time.
    pub track_dirty_pages: bool,
    /// Per-device summary: `(class, id, mmio_slot)`.
    pub devices: Vec<(String, String, u32)>,
    /// Whether MMDS state is present.
    pub mmds_present: bool,
    /// Best-effort memory-file path inferred by swapping `.snap`→`.mem`.
    pub inferred_memory_path: Option<std::path::PathBuf>,
    /// Whether that inferred memory file exists.
    pub inferred_memory_exists: bool,
    /// Inferred memory file's size in bytes (if present).
    pub inferred_memory_size: Option<u64>,
    /// CRC verified on load.
    pub crc_ok: bool,
}

impl SnapshotDescription {
    /// Convert to a `serde_json::Value` for machine consumption.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "state_path": self.state_path.display().to_string(),
            "magic": format!("{:#018x}", self.magic),
            "version": self.version.to_string(),
            "expected_version": self.expected_version.to_string(),
            "vcpu_count": self.vcpu_count,
            "mem_size_mib": self.mem_size_mib,
            "track_dirty_pages": self.track_dirty_pages,
            "devices": self
                .devices
                .iter()
                .map(|(k, i, s)| serde_json::json!({"kind": k, "id": i, "mmio_slot": s}))
                .collect::<Vec<_>>(),
            "mmds_present": self.mmds_present,
            "inferred_memory_path": self
                .inferred_memory_path
                .as_ref()
                .map(|p| p.display().to_string()),
            "inferred_memory_exists": self.inferred_memory_exists,
            "inferred_memory_size": self.inferred_memory_size,
            "crc_ok": self.crc_ok,
        })
    }

    /// Format a human-readable summary suitable for `--describe-snapshot` stdout.
    pub fn human(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "snapshot: {}", self.state_path.display());
        let _ = writeln!(s, "  magic:               {:#018x}", self.magic);
        let _ = writeln!(s, "  version:             {}", self.version);
        if self.version != self.expected_version {
            let _ = writeln!(s, "  expected (squib):    {}", self.expected_version);
        }
        let _ = writeln!(s, "  vcpu_count:          {}", self.vcpu_count);
        let _ = writeln!(s, "  mem_size_mib:        {}", self.mem_size_mib);
        let _ = writeln!(s, "  track_dirty_pages:   {}", self.track_dirty_pages);
        let _ = writeln!(s, "  devices:             {}", self.devices.len());
        for (kind, id, slot) in &self.devices {
            let _ = writeln!(s, "    - {kind}: {id} (slot {slot})");
        }
        let _ = writeln!(s, "  mmds_present:        {}", self.mmds_present);
        if let Some(p) = &self.inferred_memory_path {
            let _ = writeln!(
                s,
                "  memory file:         {} ({})",
                p.display(),
                if self.inferred_memory_exists {
                    self.inferred_memory_size
                        .map_or_else(|| "present".into(), |b| format!("{b} bytes"))
                } else {
                    "MISSING".into()
                }
            );
        }
        let _ = writeln!(
            s,
            "  crc_ok:              {}",
            if self.crc_ok { "yes" } else { "NO" }
        );
        s
    }
}

/// Build a description from a state file.
///
/// Always tries CRC-checked load first; on CRC failure, falls back to the
/// no-CRC-check load so the operator can still see the version + structure of a
/// corrupt file. Surfaces the CRC outcome via `crc_ok`.
///
/// # Errors
/// [`SnapshotError`] for any decode failure that even the no-CRC path can't recover.
pub fn describe(state_path: &Path) -> Result<SnapshotDescription> {
    let bytes = std::fs::read(state_path)?;
    let (snapshot, crc_ok) = match Snapshot::<MicrovmState>::load_from_slice(&bytes) {
        Ok(snap) => (snap, true),
        Err(SnapshotError::CrcMismatch) => {
            // The body excluding the CRC trailer; subscribed to the same size limit.
            if bytes.len() < 8 {
                return Err(SnapshotError::TooShort);
            }
            let body = &bytes[..bytes.len() - 8];
            (
                Snapshot::<MicrovmState>::load_without_crc_check(body)?,
                false,
            )
        }
        Err(e) => return Err(e),
    };

    let inferred_memory_path = infer_memory_path(state_path);
    let (inferred_memory_exists, inferred_memory_size) = match &inferred_memory_path {
        Some(p) => match std::fs::metadata(p) {
            Ok(m) => (true, Some(m.len())),
            Err(_) => (false, None),
        },
        None => (false, None),
    };

    let devices = snapshot
        .data
        .device_states
        .devices
        .iter()
        .map(|d| (d.kind.clone(), d.id.clone(), d.mmio_slot))
        .collect();

    Ok(SnapshotDescription {
        state_path: state_path.to_path_buf(),
        magic: snapshot.header.magic,
        version: snapshot.header.version,
        expected_version: SNAPSHOT_VERSION,
        vcpu_count: snapshot.data.vcpu_states.len(),
        mem_size_mib: snapshot.data.vm_info.mem_size_mib,
        track_dirty_pages: snapshot.data.vm_info.track_dirty_pages,
        devices,
        mmds_present: snapshot.data.mmds_state.is_some(),
        inferred_memory_path,
        inferred_memory_exists,
        inferred_memory_size,
        crc_ok,
    })
}

/// Best-effort `<id>.snap → <id>.mem` translation.
///
/// Only triggers when the file extension is exactly `.snap`. We do **not** try
/// `.snap.gz`, `.snapshot`, or anything else; the operator can pass `--memory-path`
/// explicitly when their naming convention diverges.
fn infer_memory_path(state_path: &Path) -> Option<std::path::PathBuf> {
    let ext = state_path.extension()?.to_str()?;
    if ext != "snap" {
        return None;
    }
    let mut p = state_path.to_path_buf();
    p.set_extension("mem");
    Some(p)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::{
        envelope::Snapshot,
        memory::VecPageReader,
        save::{SaveRequest, SnapshotKind, save},
        state::{DeviceState, DeviceStates, GicState, MicrovmState, VcpuState, VmInfo},
    };

    fn build_state() -> MicrovmState {
        MicrovmState {
            vm_info: VmInfo {
                mem_size_mib: 256,
                smt: false,
                cpu_template: String::new(),
                kernel_image_path: "/k".into(),
                initrd_path: None,
                boot_args: String::new(),
                track_dirty_pages: false,
            },
            vcpu_states: vec![VcpuState::new(0), VcpuState::new(1)],
            device_states: DeviceStates::from_devices(vec![DeviceState {
                kind: "virtio-block".into(),
                id: "rootfs".into(),
                mmio_slot: 0,
                blob: vec![],
            }]),
            gic_state: GicState::from_bytes(vec![0xAA; 32]),
            mmds_state: None,
        }
    }

    fn dest_in(dir: &Path, name: &str) -> std::path::PathBuf {
        dir.join(name)
    }

    fn save_pair(dir: &Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let snap = dest_in(dir, &format!("{name}.snap"));
        let mem = dest_in(dir, &format!("{name}.mem"));
        let reader = VecPageReader::new(vec![0u8; 16 * 1024]);
        save(SaveRequest {
            state_path: &snap,
            memory_path: &mem,
            kind: SnapshotKind::Full,
            state: build_state(),
            memory: &reader,
            ram_size: 16 * 1024,
            memory_page_size: 16 * 1024,
            dirty: None,
        })
        .unwrap();
        (snap, mem)
    }

    #[test]
    fn test_should_round_trip_save_then_load() {
        let dir = TempDir::new().unwrap();
        let (snap, _mem) = save_pair(dir.path(), "x");
        let loaded = load(&snap).unwrap();
        assert_eq!(loaded.state.vcpu_states.len(), 2);
        assert_eq!(loaded.source_version, SNAPSHOT_VERSION);
    }

    #[test]
    fn test_should_describe_state_file_in_human_form() {
        let dir = TempDir::new().unwrap();
        let (snap, _mem) = save_pair(dir.path(), "x");
        let desc = describe(&snap).unwrap();
        let h = desc.human();
        assert!(h.contains("vcpu_count:          2"));
        assert!(h.contains("mem_size_mib:        256"));
        assert!(h.contains("crc_ok:              yes"));
    }

    #[test]
    fn test_should_describe_state_file_in_json_form() {
        let dir = TempDir::new().unwrap();
        let (snap, _mem) = save_pair(dir.path(), "x");
        let desc = describe(&snap).unwrap();
        let j = desc.to_json();
        assert_eq!(j["vcpu_count"], 2);
        assert_eq!(j["mem_size_mib"], 256);
        assert_eq!(j["crc_ok"], true);
    }

    #[test]
    fn test_should_describe_corrupt_file_with_crc_warning() {
        // Flip a byte in the trailing CRC so the body's magic + version still
        // decode but the checksum fails — this is the case `--describe-snapshot`
        // is designed to surface (operator gets the metadata, knows the file is
        // damaged).
        let dir = TempDir::new().unwrap();
        let (snap, _mem) = save_pair(dir.path(), "x");
        let mut bytes = std::fs::read(&snap).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        std::fs::write(&snap, &bytes).unwrap();
        let desc = describe(&snap).unwrap();
        assert!(!desc.crc_ok);
        assert!(desc.human().contains("crc_ok:              NO"));
    }

    #[test]
    fn test_should_infer_memory_path_from_state_path() {
        assert_eq!(
            infer_memory_path(Path::new("/tmp/x.snap")),
            Some(std::path::PathBuf::from("/tmp/x.mem"))
        );
        assert_eq!(infer_memory_path(Path::new("/tmp/x.bin")), None);
        assert_eq!(infer_memory_path(Path::new("/tmp/x")), None);
    }

    #[test]
    fn test_should_report_missing_memory_file() {
        let dir = TempDir::new().unwrap();
        let (snap, mem) = save_pair(dir.path(), "x");
        std::fs::remove_file(&mem).unwrap();
        let desc = describe(&snap).unwrap();
        assert!(!desc.inferred_memory_exists);
        assert!(desc.human().contains("MISSING"));
    }

    #[test]
    fn test_should_reject_load_on_incompatible_state() {
        let dir = TempDir::new().unwrap();
        let snap = dest_in(dir.path(), "x.snap");
        // Hand-craft an envelope with 0 vCPUs; will pass envelope decode but fail
        // verify_compatible.
        let mut bad = build_state();
        bad.vcpu_states.clear();
        let envelope = Snapshot::new(bad);
        let mut buf = Vec::new();
        envelope.save(&mut buf).unwrap();
        std::fs::write(&snap, &buf).unwrap();
        let res = load(&snap);
        assert!(matches!(res, Err(SnapshotError::Incompatible)));
    }

    #[test]
    fn test_should_surface_truncated_file_as_too_short() {
        let dir = TempDir::new().unwrap();
        let snap = dest_in(dir.path(), "x.snap");
        std::fs::write(&snap, b"abc").unwrap();
        let res = describe(&snap);
        assert!(matches!(
            res,
            Err(SnapshotError::TooShort | SnapshotError::CrcMismatch)
        ));
    }
}
