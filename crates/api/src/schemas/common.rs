//! Boundary newtypes shared across endpoint schemas.
//!
//! Per [70-security.md § 4](../../../specs/70-security.md#4-input-validation), every
//! domain primitive that crosses the HTTP boundary is wrapped in a newtype with private
//! fields and a fallible constructor. Validation runs once in `try_from`; every
//! downstream use is provably safe by construction.
//!
//! Per [10-data-model.md § 2.3](../../../specs/10-data-model.md#23-schema-layer):
//!
//! - identifier regex allowlist: `^[A-Za-z0-9_]{1,64}$`
//! - default string cap 256 **bytes** (not chars; multi-byte exhaustion is an attack)
//! - path cap 1024 bytes (Darwin `PATH_MAX`)
//! - UDS path cap 103 bytes (Darwin `sun_path` minus the NUL terminator)
//! - per-class collection caps calibrated against the 32-slot virtio-MMIO budget

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Maximum number of `/drives` rows in a single configuration.
pub const MAX_DRIVES: usize = 8;

/// Maximum number of `/network-interfaces` rows in a single configuration.
pub const MAX_NICS: usize = 8;

/// Maximum number of `/pmem` rows in a single configuration.
pub const MAX_PMEM: usize = 4;

/// Maximum number of `virtio-mem` instances in a single configuration.
pub const MAX_VIRTIO_MEM: usize = 1;

/// Default per-string byte cap for fields without an explicit override.
pub const DEFAULT_STRING_CAP: usize = 256;

/// Path-shaped string cap (Darwin `PATH_MAX`).
pub const PATH_MAX: usize = 1024;

/// Unix-domain-socket path cap (Darwin `sun_path` size minus the NUL terminator).
pub const UDS_PATH_MAX: usize = 103;

/// Maximum vCPU count, matching upstream `MAX_SUPPORTED_VCPUS` and D19.
pub const MAX_VCPU_COUNT: u32 = 32;

/// Identifier regex per upstream Firecracker (`^[A-Za-z0-9_]{1,64}$`).
fn is_valid_identifier(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn ensure_no_nul(value: &str, field: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!("{field} must not contain NUL bytes"));
    }
    Ok(())
}

/// Validated microVM instance ID (`--id` / `InstanceInfo.id`). Regex
/// `^[A-Za-z0-9_]{1,64}$`; empty rejected.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
#[serde(transparent)]
pub struct InstanceId(String);

impl InstanceId {
    /// Wrap an instance ID after validating the upstream identifier rules.
    pub fn new(id: impl Into<String>) -> Result<Self, String> {
        let id = id.into();
        if !is_valid_identifier(&id) {
            return Err(format!(
                "Invalid id: must match ^[A-Za-z0-9_]{{1,64}}$ (got {} bytes)",
                id.len()
            ));
        }
        Ok(Self(id))
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for InstanceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for InstanceId {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// Validated `drive_id`. Regex `^[A-Za-z0-9_]{1,64}$`.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
#[serde(transparent)]
pub struct DriveId(String);

impl DriveId {
    /// Wrap a drive ID after running the identifier allowlist.
    pub fn new(id: impl Into<String>) -> Result<Self, String> {
        let id = id.into();
        if !is_valid_identifier(&id) {
            return Err("Invalid drive_id: must match ^[A-Za-z0-9_]{1,64}$".to_string());
        }
        Ok(Self(id))
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for DriveId {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// Validated `iface_id`. Regex `^[A-Za-z0-9_]{1,64}$`.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
#[serde(transparent)]
pub struct IfaceId(String);

impl IfaceId {
    /// Wrap a network-interface ID after running the identifier allowlist.
    pub fn new(id: impl Into<String>) -> Result<Self, String> {
        let id = id.into();
        if !is_valid_identifier(&id) {
            return Err("Invalid iface_id: must match ^[A-Za-z0-9_]{1,64}$".to_string());
        }
        Ok(Self(id))
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for IfaceId {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// Validated vsock ID (the upstream `vsock_id` field). Regex `^[A-Za-z0-9_]{1,64}$`.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
#[serde(transparent)]
pub struct VsockId(String);

impl VsockId {
    /// Wrap a vsock ID after running the identifier allowlist.
    pub fn new(id: impl Into<String>) -> Result<Self, String> {
        let id = id.into();
        if !is_valid_identifier(&id) {
            return Err("Invalid vsock_id: must match ^[A-Za-z0-9_]{1,64}$".to_string());
        }
        Ok(Self(id))
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for VsockId {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// Validated host-filesystem path (`path_on_host`, `kernel_image_path`, etc.).
///
/// Performs three checks: byte length cap (`PATH_MAX = 1024`), no NUL bytes, and (for
/// fields where it applies) parent-traversal rejection. Canonicalization happens at
/// the consumer (the loader / drive opener), not here — Phase 2 cannot canonicalize
/// because the file may not exist yet at config-load time.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SafePath(PathBuf);

impl SafePath {
    /// Wrap a host filesystem path after running the boundary checks.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let s = path
            .to_str()
            .ok_or_else(|| "path is not valid UTF-8".to_string())?;
        if s.is_empty() {
            return Err("path must not be empty".into());
        }
        if s.len() > PATH_MAX {
            return Err(format!(
                "path exceeds {PATH_MAX} bytes (got {} bytes)",
                s.len()
            ));
        }
        ensure_no_nul(s, "path")?;
        Ok(Self(path))
    }

    /// Borrow the inner [`std::path::Path`].
    #[must_use]
    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for SafePath {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl From<SafePath> for PathBuf {
    fn from(p: SafePath) -> Self {
        p.0
    }
}

impl<'de> Deserialize<'de> for SafePath {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// Validated UDS path (`api_sock`, `vsock.uds_path`, `mem_backend.backend_path`).
///
/// Capped at 103 bytes to fit Darwin's `sockaddr_un.sun_path` minus the NUL terminator;
/// the kernel silently truncates beyond this, so a hard cap surfaces the misconfiguration
/// as a 4xx instead of a "connection refused" debugging session ([70 §
/// 5](../../../specs/70-security.md#5-path-inputs)).
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct UdsPath(PathBuf);

impl UdsPath {
    /// Wrap a UDS path, enforcing the Darwin `sun_path` cap.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let s = path
            .to_str()
            .ok_or_else(|| "uds path is not valid UTF-8".to_string())?;
        if s.is_empty() {
            return Err("uds path must not be empty".into());
        }
        if s.len() > UDS_PATH_MAX {
            return Err(format!(
                "uds path exceeds {UDS_PATH_MAX} bytes on Darwin (got {} bytes)",
                s.len()
            ));
        }
        ensure_no_nul(s, "uds path")?;
        Ok(Self(path))
    }

    /// Borrow the inner [`std::path::Path`].
    #[must_use]
    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

impl<'de> Deserialize<'de> for UdsPath {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// Validated memory size in MiB. The upper bound is host-RAM-dependent and validated
/// at the controller against `BackendCapabilities`; the type-level constraint is
/// `mem_size_mib >= 1`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MemSizeMib(u64);

impl MemSizeMib {
    /// Wrap a memory-size value in MiB, enforcing the lower bound.
    pub fn new(value: u64) -> Result<Self, String> {
        if value == 0 {
            return Err("mem_size_mib must be >= 1".into());
        }
        Ok(Self(value))
    }

    /// The raw MiB count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for MemSizeMib {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let v = u64::deserialize(de)?;
        Self::new(v).map_err(serde::de::Error::custom)
    }
}

/// Validated 48-bit MAC address. Accepts the canonical `aa:bb:cc:dd:ee:ff` shape
/// (lowercase or uppercase), rejects anything else.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize)]
#[serde(transparent)]
pub struct MacAddr {
    #[serde(with = "mac_str")]
    bytes: [u8; 6],
}

impl MacAddr {
    /// Wrap raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 6]) -> Self {
        Self { bytes }
    }

    /// Borrow the underlying 6-byte representation. The VMM consumes this
    /// to seed the virtio-net `guest_mac`; upstream keeps the getter narrow
    /// so the raw bytes don't leak into arbitrary code.
    #[must_use]
    pub const fn bytes(&self) -> [u8; 6] {
        self.bytes
    }

    /// The wire-shape `aa:bb:cc:dd:ee:ff` representation.
    #[must_use]
    pub fn to_canonical_string(self) -> String {
        format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.bytes[0],
            self.bytes[1],
            self.bytes[2],
            self.bytes[3],
            self.bytes[4],
            self.bytes[5]
        )
    }

    /// Parse from the canonical `aa:bb:cc:dd:ee:ff` form.
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut bytes = [0u8; 6];
        let mut count = 0usize;
        for (i, part) in s.split(':').enumerate() {
            if i >= 6 {
                return Err(format!(
                    "Invalid MAC: expected 6 octets (saw at least {})",
                    i + 1
                ));
            }
            if part.len() != 2 {
                return Err(format!(
                    "Invalid MAC: octet {i} must be exactly 2 hex digits"
                ));
            }
            let octet = u8::from_str_radix(part, 16)
                .map_err(|_| format!("Invalid MAC: octet {i} ({part}) is not valid hexadecimal"))?;
            // SAFETY-EQUIVALENT: `i < 6` guarded above; `bytes.get_mut` would also work
            // but `[i]` keeps the array-typed assignment explicit.
            if let Some(slot) = bytes.get_mut(i) {
                *slot = octet;
            }
            count = i + 1;
        }
        if count != 6 {
            return Err(format!(
                "Invalid MAC: expected 6 colon-separated octets (got {count})"
            ));
        }
        Ok(Self { bytes })
    }
}

impl<'de> Deserialize<'de> for MacAddr {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

mod mac_str {
    use serde::Serializer;

    #[allow(clippy::trivially_copy_pass_by_ref)] // signature dictated by `#[serde(with)]`
    pub(super) fn serialize<S: Serializer>(bytes: &[u8; 6], s: S) -> Result<S::Ok, S::Error> {
        let formatted = format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
        );
        s.serialize_str(&formatted)
    }
}

/// Length-cap any free-form string field to `DEFAULT_STRING_CAP` bytes (256), unless
/// a per-field override is documented in the field's spec entry.
pub fn check_string_cap(value: &str, field: &str, max: usize) -> Result<(), String> {
    if value.len() > max {
        return Err(format!(
            "{field} exceeds {max} bytes (got {} bytes)",
            value.len()
        ));
    }
    ensure_no_nul(value, field)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_valid_drive_id() {
        let id = DriveId::new("rootfs").unwrap();
        assert_eq!(id.as_str(), "rootfs");
    }

    #[test]
    fn test_should_reject_drive_id_with_dash() {
        let err = DriveId::new("root-fs").unwrap_err();
        assert!(err.contains("Invalid drive_id"));
    }

    #[test]
    fn test_should_reject_empty_drive_id() {
        assert!(DriveId::new("").is_err());
    }

    #[test]
    fn test_should_reject_drive_id_over_64_bytes() {
        let id = "a".repeat(65);
        assert!(DriveId::new(id).is_err());
    }

    #[test]
    fn test_should_reject_drive_id_with_nul_byte() {
        assert!(DriveId::new("root\0fs").is_err());
    }

    #[test]
    fn test_should_accept_path_under_path_max() {
        let p = SafePath::new("/tmp/kernel.bin").unwrap();
        assert_eq!(p.as_path().as_os_str(), "/tmp/kernel.bin");
    }

    #[test]
    fn test_should_reject_path_over_path_max() {
        let s = format!("/tmp/{}", "a".repeat(PATH_MAX));
        assert!(SafePath::new(s).is_err());
    }

    #[test]
    fn test_should_reject_path_with_nul_byte() {
        assert!(SafePath::new("/tmp/k\0ernel").is_err());
    }

    #[test]
    fn test_should_reject_uds_path_over_103_bytes() {
        let s = format!("/tmp/{}", "a".repeat(UDS_PATH_MAX));
        assert!(UdsPath::new(s).is_err());
    }

    #[test]
    fn test_should_accept_uds_path_under_cap() {
        let p = UdsPath::new("/tmp/squib.sock").unwrap();
        assert_eq!(p.as_path().as_os_str(), "/tmp/squib.sock");
    }

    #[test]
    fn test_should_round_trip_mac_through_canonical_string() {
        let m = MacAddr::parse("AA:bb:CC:dd:EE:ff").unwrap();
        assert_eq!(m.to_canonical_string(), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn test_should_reject_mac_with_invalid_octet_count() {
        assert!(MacAddr::parse("aa:bb:cc:dd:ee").is_err());
    }

    #[test]
    fn test_should_reject_mac_with_non_hex_octet() {
        assert!(MacAddr::parse("zz:bb:cc:dd:ee:ff").is_err());
    }

    #[test]
    fn test_should_reject_zero_mem_size_mib() {
        assert!(MemSizeMib::new(0).is_err());
    }

    #[test]
    fn test_should_accept_one_mib_and_above() {
        assert_eq!(MemSizeMib::new(256).unwrap().get(), 256);
    }
}
