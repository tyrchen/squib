//! `/pmem/{id}` PUT and PATCH bodies — virtio-pmem on a memory-mapped file.

use serde::{Deserialize, Serialize};

use super::common::{DriveId, SafePath};

/// Raw `/pmem/{id}` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPmemConfig {
    /// Identifier (we re-use the drive identifier rules).
    pub pmem_id: String,
    /// Backing-file path.
    pub path_on_host: String,
    /// Whether the device is exposed read-only.
    #[serde(default)]
    pub is_read_only: bool,
    /// Whether the guest mounts this as the root device (replaces virtio-block root).
    #[serde(default)]
    pub is_root_device: bool,
    /// Optional partition UUID for `root=PARTUUID=...`.
    #[serde(default)]
    pub partuuid: Option<String>,
    /// Optional rate limiter passthrough.
    #[serde(default)]
    pub rate_limiter: Option<serde_json::Value>,
}

/// Validated `/pmem/{id}` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct PmemConfig {
    /// Validated identifier.
    pub pmem_id: DriveId,
    /// Validated host path.
    pub path_on_host: SafePath,
    /// Read-only flag.
    pub is_read_only: bool,
    /// Root-device flag.
    pub is_root_device: bool,
    /// Validated partition UUID.
    pub partuuid: Option<String>,
    /// Optional rate limiter passthrough.
    pub rate_limiter: Option<serde_json::Value>,
}

fn validate_partuuid(uuid: &str) -> Result<(), String> {
    if uuid.is_empty() || uuid.len() > 64 {
        return Err("Invalid partuuid: must be 1..=64 bytes".into());
    }
    if !uuid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err("Invalid partuuid: only [A-Za-z0-9-] permitted".into());
    }
    Ok(())
}

impl TryFrom<RawPmemConfig> for PmemConfig {
    type Error = String;

    fn try_from(raw: RawPmemConfig) -> Result<Self, Self::Error> {
        let pmem_id = DriveId::new(raw.pmem_id)?;
        let path_on_host =
            SafePath::new(raw.path_on_host).map_err(|e| format!("Invalid path_on_host: {e}"))?;
        if let Some(p) = raw.partuuid.as_deref() {
            validate_partuuid(p)?;
        }
        Ok(Self {
            pmem_id,
            path_on_host,
            is_read_only: raw.is_read_only,
            is_root_device: raw.is_root_device,
            partuuid: raw.partuuid,
            rate_limiter: raw.rate_limiter,
        })
    }
}

/// Raw `/pmem/{id}` PATCH body. Mirrors upstream's `PmemDeviceUpdateConfig`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPmemPatch {
    /// Pmem device ID being patched (must match the URL `{id}`). Upstream calls this
    /// `id`; we accept both `pmem_id` and `id` for forward compat with squib's other
    /// patch bodies.
    #[serde(alias = "id")]
    pub pmem_id: String,
    /// Replacement rate limiter.
    #[serde(default)]
    pub rate_limiter: Option<serde_json::Value>,
}

/// Validated `/pmem/{id}` PATCH body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct PmemPatch {
    /// Validated pmem device ID.
    pub pmem_id: DriveId,
    /// Replacement rate limiter passthrough.
    pub rate_limiter: Option<serde_json::Value>,
}

impl TryFrom<RawPmemPatch> for PmemPatch {
    type Error = String;

    fn try_from(raw: RawPmemPatch) -> Result<Self, Self::Error> {
        Ok(Self {
            pmem_id: DriveId::new(raw.pmem_id)?,
            rate_limiter: raw.rate_limiter,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_minimal_pmem() {
        let cfg = PmemConfig::try_from(RawPmemConfig {
            pmem_id: "pmem0".into(),
            path_on_host: "/tmp/p.bin".into(),
            is_read_only: false,
            is_root_device: false,
            partuuid: None,
            rate_limiter: None,
        })
        .unwrap();
        assert_eq!(cfg.pmem_id.as_str(), "pmem0");
    }

    #[test]
    fn test_should_reject_invalid_pmem_id() {
        assert!(
            PmemConfig::try_from(RawPmemConfig {
                pmem_id: "pmem-0".into(),
                path_on_host: "/tmp/p.bin".into(),
                is_read_only: false,
                is_root_device: false,
                partuuid: None,
                rate_limiter: None,
            })
            .is_err()
        );
    }

    #[test]
    fn test_should_round_trip_pmem_patch() {
        let raw: RawPmemPatch = serde_json::from_str(r#"{"pmem_id":"pmem0"}"#).unwrap();
        let patch = PmemPatch::try_from(raw).unwrap();
        assert_eq!(patch.pmem_id.as_str(), "pmem0");
    }

    #[test]
    fn test_should_accept_id_alias_in_pmem_patch() {
        let raw: RawPmemPatch = serde_json::from_str(r#"{"id":"pmem0"}"#).unwrap();
        let patch = PmemPatch::try_from(raw).unwrap();
        assert_eq!(patch.pmem_id.as_str(), "pmem0");
    }
}
