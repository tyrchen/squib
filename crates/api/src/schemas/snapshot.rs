//! `/snapshot/create` and `/snapshot/load` bodies.

use serde::{Deserialize, Serialize};

use super::common::{SafePath, UdsPath};

/// Snapshot type for `/snapshot/create`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
pub enum SnapshotType {
    /// Full snapshot — the entire memory file is dumped.
    #[default]
    Full,
    /// Diff snapshot — only dirty pages (requires `track_dirty_pages`).
    Diff,
}

/// Raw `/snapshot/create` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSnapshotCreateConfig {
    /// Output path for the state file (`<id>.snap`).
    pub snapshot_path: String,
    /// Output path for the memory file (`<id>.mem`).
    pub mem_file_path: String,
    /// Snapshot type (Full or Diff).
    #[serde(default)]
    pub snapshot_type: SnapshotType,
    /// Optional version override (currently ignored; we follow upstream version).
    #[serde(default)]
    pub version: Option<String>,
}

/// Validated `/snapshot/create` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct SnapshotCreateConfig {
    /// Validated state-file output path.
    pub snapshot_path: SafePath,
    /// Validated memory-file output path.
    pub mem_file_path: SafePath,
    /// Snapshot type.
    pub snapshot_type: SnapshotType,
    /// Optional version override.
    pub version: Option<String>,
}

impl TryFrom<RawSnapshotCreateConfig> for SnapshotCreateConfig {
    type Error = String;

    fn try_from(raw: RawSnapshotCreateConfig) -> Result<Self, Self::Error> {
        let snapshot_path =
            SafePath::new(raw.snapshot_path).map_err(|e| format!("Invalid snapshot_path: {e}"))?;
        let mem_file_path =
            SafePath::new(raw.mem_file_path).map_err(|e| format!("Invalid mem_file_path: {e}"))?;
        if let Some(v) = raw.version.as_deref()
            && (v.is_empty() || v.len() > 32)
        {
            return Err("Invalid version: must be 1..=32 bytes".into());
        }
        Ok(Self {
            snapshot_path,
            mem_file_path,
            snapshot_type: raw.snapshot_type,
            version: raw.version,
        })
    }
}

/// Memory backend type for `/snapshot/load`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
pub enum MemBackendType {
    /// Memory file is loaded eagerly from disk.
    #[default]
    File,
    /// Memory is faulted in via a userfaultfd-style backend (Mach exception ports on
    /// Darwin); `backend_path` is a UDS the page server connects to.
    Uffd,
}

/// `mem_backend` sub-object on `/snapshot/load`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMemBackend {
    /// Backend kind.
    pub backend_type: MemBackendType,
    /// Path to the memory file (`File`) or to the page-server UDS (`Uffd`).
    pub backend_path: String,
}

/// Validated `mem_backend`.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct MemBackend {
    /// Backend kind.
    pub backend_type: MemBackendType,
    /// Validated backend path. The cap depends on the backend type — UDS for `Uffd`,
    /// regular file path for `File`.
    pub backend_path_file: Option<SafePath>,
    /// UDS path when `backend_type` is Uffd.
    pub backend_path_uds: Option<UdsPath>,
}

impl TryFrom<RawMemBackend> for MemBackend {
    type Error = String;

    fn try_from(raw: RawMemBackend) -> Result<Self, Self::Error> {
        match raw.backend_type {
            MemBackendType::File => {
                let p = SafePath::new(raw.backend_path)
                    .map_err(|e| format!("Invalid mem_backend.backend_path: {e}"))?;
                Ok(Self {
                    backend_type: raw.backend_type,
                    backend_path_file: Some(p),
                    backend_path_uds: None,
                })
            }
            MemBackendType::Uffd => {
                let p = UdsPath::new(raw.backend_path)
                    .map_err(|e| format!("Invalid mem_backend.backend_path: {e}"))?;
                Ok(Self {
                    backend_type: raw.backend_type,
                    backend_path_file: None,
                    backend_path_uds: Some(p),
                })
            }
        }
    }
}

/// Raw `/snapshot/load` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSnapshotLoadConfig {
    /// Path to the state file.
    pub snapshot_path: String,
    /// Memory backend descriptor (post-1.5 upstream API).
    #[serde(default)]
    pub mem_backend: Option<RawMemBackend>,
    /// Deprecated single-string memory file path (pre-1.5 upstream API; squib accepts
    /// for back-compat).
    #[serde(default)]
    pub mem_file_path: Option<String>,
    /// Whether to track dirty pages after restore (for subsequent diff snapshots).
    #[serde(default)]
    pub track_dirty_pages: bool,
    /// Whether to resume vCPUs immediately after restore.
    #[serde(default)]
    pub resume_vm: bool,
    /// `clock_realtime` — x86-only field; squib accept-and-ignore (A row).
    #[serde(default)]
    pub clock_realtime: Option<u64>,
    /// Per-NIC overrides (passthrough).
    #[serde(default)]
    pub network_overrides: Option<Vec<serde_json::Value>>,
    /// Vsock override (passthrough).
    #[serde(default)]
    pub vsock_override: Option<serde_json::Value>,
}

/// Validated `/snapshot/load` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct SnapshotLoadConfig {
    /// Validated state-file path.
    pub snapshot_path: SafePath,
    /// Validated memory backend (or back-compat `mem_file_path`).
    pub mem_backend: MemBackend,
    /// Whether to track dirty pages after restore.
    pub track_dirty_pages: bool,
    /// Whether to resume vCPUs immediately after restore.
    pub resume_vm: bool,
    /// `clock_realtime` — recorded but ignored (warning emitted).
    pub clock_realtime: Option<u64>,
    /// Per-NIC overrides (passthrough).
    pub network_overrides: Vec<serde_json::Value>,
    /// Vsock override (passthrough).
    pub vsock_override: Option<serde_json::Value>,
}

impl TryFrom<RawSnapshotLoadConfig> for SnapshotLoadConfig {
    type Error = String;

    fn try_from(raw: RawSnapshotLoadConfig) -> Result<Self, Self::Error> {
        let snapshot_path =
            SafePath::new(raw.snapshot_path).map_err(|e| format!("Invalid snapshot_path: {e}"))?;
        let mem_backend = match (raw.mem_backend, raw.mem_file_path) {
            (Some(mb), None) => MemBackend::try_from(mb)?,
            (None, Some(p)) => {
                // Back-compat single-string form maps to backend_type=File.
                let p = SafePath::new(p).map_err(|e| format!("Invalid mem_file_path: {e}"))?;
                MemBackend {
                    backend_type: MemBackendType::File,
                    backend_path_file: Some(p),
                    backend_path_uds: None,
                }
            }
            (Some(_), Some(_)) => {
                return Err(
                    "Invalid snapshot/load: provide exactly one of mem_backend or mem_file_path"
                        .into(),
                );
            }
            (None, None) => {
                return Err(
                    "Invalid snapshot/load: must provide mem_backend or mem_file_path".into(),
                );
            }
        };
        Ok(Self {
            snapshot_path,
            mem_backend,
            track_dirty_pages: raw.track_dirty_pages,
            resume_vm: raw.resume_vm,
            clock_realtime: raw.clock_realtime,
            network_overrides: raw.network_overrides.unwrap_or_default(),
            vsock_override: raw.vsock_override,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_validate_snapshot_create_paths() {
        let cfg = SnapshotCreateConfig::try_from(RawSnapshotCreateConfig {
            snapshot_path: "/tmp/x.snap".into(),
            mem_file_path: "/tmp/x.mem".into(),
            snapshot_type: SnapshotType::Full,
            version: None,
        })
        .unwrap();
        assert_eq!(cfg.snapshot_path.as_path().as_os_str(), "/tmp/x.snap");
    }

    #[test]
    fn test_should_default_snapshot_type_to_full() {
        let json = r#"{"snapshot_path":"/tmp/x.snap","mem_file_path":"/tmp/x.mem"}"#;
        let raw: RawSnapshotCreateConfig = serde_json::from_str(json).unwrap();
        assert_eq!(raw.snapshot_type, SnapshotType::Full);
    }

    #[test]
    fn test_should_accept_back_compat_mem_file_path() {
        let cfg = SnapshotLoadConfig::try_from(RawSnapshotLoadConfig {
            snapshot_path: "/tmp/x.snap".into(),
            mem_backend: None,
            mem_file_path: Some("/tmp/x.mem".into()),
            track_dirty_pages: false,
            resume_vm: false,
            clock_realtime: None,
            network_overrides: None,
            vsock_override: None,
        })
        .unwrap();
        assert_eq!(cfg.mem_backend.backend_type, MemBackendType::File);
    }

    #[test]
    fn test_should_reject_both_mem_backend_and_mem_file_path() {
        let mb = RawMemBackend {
            backend_type: MemBackendType::File,
            backend_path: "/tmp/x.mem".into(),
        };
        let res = SnapshotLoadConfig::try_from(RawSnapshotLoadConfig {
            snapshot_path: "/tmp/x.snap".into(),
            mem_backend: Some(mb),
            mem_file_path: Some("/tmp/y.mem".into()),
            track_dirty_pages: false,
            resume_vm: false,
            clock_realtime: None,
            network_overrides: None,
            vsock_override: None,
        });
        assert!(res.is_err());
    }

    #[test]
    fn test_should_validate_uffd_backend_uds_path() {
        let mb = RawMemBackend {
            backend_type: MemBackendType::Uffd,
            backend_path: "/tmp/pager.sock".into(),
        };
        let cfg = SnapshotLoadConfig::try_from(RawSnapshotLoadConfig {
            snapshot_path: "/tmp/x.snap".into(),
            mem_backend: Some(mb),
            mem_file_path: None,
            track_dirty_pages: true,
            resume_vm: true,
            clock_realtime: None,
            network_overrides: None,
            vsock_override: None,
        })
        .unwrap();
        assert_eq!(cfg.mem_backend.backend_type, MemBackendType::Uffd);
        assert!(cfg.mem_backend.backend_path_uds.is_some());
    }
}
