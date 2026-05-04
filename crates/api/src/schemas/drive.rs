//! `/drives/{id}` PUT and PATCH bodies.
//!
//! Per [21-api-compat-matrix.md `/drives/{id}`
//! PUT](../../../specs/21-api-compat-matrix.md#drivesid-put):
//!
//! - `drive_id` — `^[A-Za-z0-9_]{1,64}$` (`DriveId` newtype).
//! - `path_on_host` — `SafePath` (1024-byte cap, no NUL).
//! - `cache_type` — `Unsafe | Writeback`.
//! - `io_engine` — `Sync | Async`.
//! - `partuuid` — optional; when `is_root_device` true, threaded into `root=PARTUUID=`.
//! - `socket` (vhost-user) — `A`: accept-and-warn at config-load.
//!
//! Diffs from upstream are intentionally absent at this layer; the deviations are
//! enforced at the controller / VMM stage (e.g. vhost-user `socket` warning).

use serde::{Deserialize, Serialize};

use super::common::{DriveId, SafePath};

/// Caching policy for the block device. Mirrors upstream verbatim.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
pub enum CacheType {
    /// Pass writes straight to the backing file (no host page cache).
    #[default]
    Unsafe,
    /// Buffer writes through the host page cache and flush on guest fsync.
    Writeback,
}

/// Block-device IO engine. Sync = blocking; Async = `tokio::task::spawn_blocking`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
pub enum IoEngine {
    /// Blocking IO on the device thread.
    #[default]
    Sync,
    /// Async IO offloaded to the tokio blocking pool.
    Async,
}

/// Raw `/drives/{id}` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDriveConfig {
    /// Caller-supplied unique identifier (`^[A-Za-z0-9_]{1,64}$`).
    pub drive_id: String,
    /// Host filesystem path to the backing image.
    pub path_on_host: String,
    /// Whether the guest should mount this as the root block device.
    #[serde(default)]
    pub is_root_device: bool,
    /// Whether the guest sees the device read-only.
    #[serde(default)]
    pub is_read_only: bool,
    /// Caching policy.
    #[serde(default)]
    pub cache_type: CacheType,
    /// IO engine.
    #[serde(default)]
    pub io_engine: IoEngine,
    /// Root partition UUID for `root=PARTUUID=...`. Only honored when `is_root_device`.
    #[serde(default)]
    pub partuuid: Option<String>,
    /// Optional rate limiter (passed through verbatim for now).
    #[serde(default)]
    pub rate_limiter: Option<serde_json::Value>,
    /// vhost-user socket path. Accept-and-warn (vhost-user is Linux-only).
    #[serde(default)]
    pub socket: Option<String>,
}

/// Validated `/drives/{id}` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct DriveConfig {
    /// Validated identifier.
    pub drive_id: DriveId,
    /// Validated host path.
    pub path_on_host: SafePath,
    /// `is_root_device` is honored exactly once per VM; the controller enforces uniqueness.
    pub is_root_device: bool,
    /// `is_read_only`.
    pub is_read_only: bool,
    /// Caching policy.
    pub cache_type: CacheType,
    /// IO engine.
    pub io_engine: IoEngine,
    /// Validated root partition UUID, if present.
    pub partuuid: Option<String>,
    /// Rate limiter passthrough (validated structurally by the device layer).
    pub rate_limiter: Option<serde_json::Value>,
    /// Whether a vhost-user socket was supplied (accept-and-warn marker).
    pub vhost_user_socket: Option<String>,
}

fn validate_partuuid(uuid: &str) -> Result<(), String> {
    if uuid.is_empty() || uuid.len() > 64 {
        return Err(format!(
            "Invalid partuuid: must be 1..=64 bytes (got {} bytes)",
            uuid.len()
        ));
    }
    if !uuid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err("Invalid partuuid: only [A-Za-z0-9-] permitted".into());
    }
    Ok(())
}

impl TryFrom<RawDriveConfig> for DriveConfig {
    type Error = String;

    fn try_from(raw: RawDriveConfig) -> Result<Self, Self::Error> {
        let drive_id = DriveId::new(raw.drive_id)?;
        let path_on_host =
            SafePath::new(raw.path_on_host).map_err(|e| format!("Invalid path_on_host: {e}"))?;
        if let Some(p) = raw.partuuid.as_deref() {
            validate_partuuid(p)?;
        }
        let socket = match raw.socket {
            Some(s) if s.is_empty() => return Err("Invalid socket: must not be empty".into()),
            Some(s) if s.len() > 1024 => {
                return Err("Invalid socket: exceeds 1024 bytes".into());
            }
            other => other,
        };
        Ok(Self {
            drive_id,
            path_on_host,
            is_root_device: raw.is_root_device,
            is_read_only: raw.is_read_only,
            cache_type: raw.cache_type,
            io_engine: raw.io_engine,
            partuuid: raw.partuuid,
            rate_limiter: raw.rate_limiter,
            vhost_user_socket: socket,
        })
    }
}

/// Raw `/drives/{id}` PATCH body. Every mutable field optional.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDrivePatch {
    /// Drive ID being patched (must match the URL `{id}`).
    pub drive_id: String,
    /// Replacement backing path.
    #[serde(default)]
    pub path_on_host: Option<String>,
    /// Replacement rate limiter.
    #[serde(default)]
    pub rate_limiter: Option<serde_json::Value>,
}

/// Validated `/drives/{id}` PATCH body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct DrivePatch {
    /// Validated drive ID.
    pub drive_id: DriveId,
    /// Replacement path, if any.
    pub path_on_host: Option<SafePath>,
    /// Replacement rate limiter.
    pub rate_limiter: Option<serde_json::Value>,
}

impl TryFrom<RawDrivePatch> for DrivePatch {
    type Error = String;

    fn try_from(raw: RawDrivePatch) -> Result<Self, Self::Error> {
        let drive_id = DriveId::new(raw.drive_id)?;
        let path_on_host = match raw.path_on_host {
            Some(p) => Some(SafePath::new(p).map_err(|e| format!("Invalid path_on_host: {e}"))?),
            None => None,
        };
        Ok(Self {
            drive_id,
            path_on_host,
            rate_limiter: raw.rate_limiter,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(id: &str) -> RawDriveConfig {
        RawDriveConfig {
            drive_id: id.into(),
            path_on_host: "/tmp/img.bin".into(),
            is_root_device: false,
            is_read_only: false,
            cache_type: CacheType::Unsafe,
            io_engine: IoEngine::Sync,
            partuuid: None,
            rate_limiter: None,
            socket: None,
        }
    }

    #[test]
    fn test_should_accept_minimal_drive() {
        let cfg = DriveConfig::try_from(raw("rootfs")).unwrap();
        assert_eq!(cfg.drive_id.as_str(), "rootfs");
    }

    #[test]
    fn test_should_reject_invalid_drive_id() {
        assert!(DriveConfig::try_from(raw("root-fs")).is_err());
    }

    #[test]
    fn test_should_validate_partuuid() {
        let mut r = raw("rootfs");
        r.partuuid = Some("ABC123-def456".into());
        assert!(DriveConfig::try_from(r).is_ok());
    }

    #[test]
    fn test_should_reject_partuuid_with_special_chars() {
        let mut r = raw("rootfs");
        r.partuuid = Some("ABC*1".into());
        assert!(DriveConfig::try_from(r).is_err());
    }

    #[test]
    fn test_should_default_io_engine_to_sync() {
        let json = r#"{"drive_id":"rootfs","path_on_host":"/tmp/img.bin"}"#;
        let raw: RawDriveConfig = serde_json::from_str(json).unwrap();
        assert_eq!(raw.io_engine, IoEngine::Sync);
    }

    #[test]
    fn test_should_round_trip_drive_patch() {
        let json = r#"{"drive_id":"rootfs","path_on_host":"/tmp/x.bin"}"#;
        let raw: RawDrivePatch = serde_json::from_str(json).unwrap();
        let p = DrivePatch::try_from(raw).unwrap();
        assert_eq!(p.drive_id.as_str(), "rootfs");
        assert!(p.path_on_host.is_some());
    }
}
