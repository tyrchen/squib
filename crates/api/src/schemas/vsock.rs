//! `/vsock` PUT body.
//!
//! Per [21-api-compat-matrix.md `/vsock` PUT](../../../specs/21-api-compat-matrix.md#vsock-put):
//!
//! - `guest_cid` — minimum 3 (CIDs 0–2 reserved by the spec).
//! - `uds_path` — `UdsPath` (103-byte cap on Darwin).
//! - `tsi` — squib extension; opt-in TSI mode (default false).
//! - `vsock_id` — Firecracker still accepts this for back-compat though it's ignored at runtime; we
//!   accept and validate via `VsockId`.

use serde::{Deserialize, Serialize};

use super::common::{UdsPath, VsockId};

/// Lowest legal `guest_cid` (0 = HV, 1 = local, 2 = host — all reserved).
pub const MIN_GUEST_CID: u32 = 3;

/// Raw `/vsock` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawVsockConfig {
    /// Caller-supplied vsock ID (back-compat field; runtime-ignored upstream).
    #[serde(default)]
    pub vsock_id: Option<String>,
    /// Guest-side CID. Must be `>= 3`.
    pub guest_cid: u32,
    /// UDS path the host multiplex listener binds.
    pub uds_path: String,
    /// Squib-only opt-in: enable the TSI mode. Default `false`.
    #[serde(default)]
    pub tsi: bool,
}

/// Validated `/vsock` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct VsockConfig {
    /// Optional validated vsock ID.
    pub vsock_id: Option<VsockId>,
    /// Validated guest CID.
    pub guest_cid: u32,
    /// Validated UDS path.
    pub uds_path: UdsPath,
    /// TSI extension flag.
    pub tsi: bool,
}

impl TryFrom<RawVsockConfig> for VsockConfig {
    type Error = String;

    fn try_from(raw: RawVsockConfig) -> Result<Self, Self::Error> {
        if raw.guest_cid < MIN_GUEST_CID {
            return Err(format!(
                "Invalid guest_cid: must be >= {MIN_GUEST_CID}, got {}",
                raw.guest_cid
            ));
        }
        let vsock_id = match raw.vsock_id {
            Some(s) => Some(VsockId::new(s)?),
            None => None,
        };
        let uds_path = UdsPath::new(raw.uds_path).map_err(|e| format!("Invalid uds_path: {e}"))?;
        Ok(Self {
            vsock_id,
            guest_cid: raw.guest_cid,
            uds_path,
            tsi: raw.tsi,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_minimal_vsock() {
        let raw = RawVsockConfig {
            vsock_id: None,
            guest_cid: 3,
            uds_path: "/tmp/vsock.sock".into(),
            tsi: false,
        };
        let cfg = VsockConfig::try_from(raw).unwrap();
        assert_eq!(cfg.guest_cid, 3);
    }

    #[test]
    fn test_should_reject_guest_cid_below_3() {
        let raw = RawVsockConfig {
            vsock_id: None,
            guest_cid: 2,
            uds_path: "/tmp/vsock.sock".into(),
            tsi: false,
        };
        assert!(VsockConfig::try_from(raw).is_err());
    }

    #[test]
    fn test_should_reject_uds_path_above_darwin_cap() {
        let raw = RawVsockConfig {
            vsock_id: None,
            guest_cid: 3,
            uds_path: format!("/tmp/{}", "a".repeat(110)),
            tsi: false,
        };
        let err = VsockConfig::try_from(raw).unwrap_err();
        assert!(err.contains("uds_path"));
    }

    #[test]
    fn test_should_default_tsi_to_false() {
        let json = r#"{"guest_cid":3,"uds_path":"/tmp/v"}"#;
        let raw: RawVsockConfig = serde_json::from_str(json).unwrap();
        assert!(!raw.tsi);
    }
}
