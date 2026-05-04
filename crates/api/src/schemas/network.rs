//! `/network-interfaces/{id}` PUT and PATCH bodies.
//!
//! Per [21-api-compat-matrix.md `/network-interfaces/{id}`
//! PUT](../../../specs/21-api-compat-matrix.md#network-interfacesid-put):
//!
//! - `iface_id` — `^[A-Za-z0-9_]{1,64}$`.
//! - `host_dev_name` — `P` deviation: mapped to a vmnet handle name `squib-tap-<iface_id>`; literal
//!   Linux TAP names are not honored.
//! - `guest_mac` — auto-generated if missing.
//! - `rx_rate_limiter`, `tx_rate_limiter` — passthrough validated structurally.

use serde::{Deserialize, Serialize};

use super::common::{IfaceId, MacAddr};

/// Raw `/network-interfaces/{id}` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawNetworkInterfaceConfig {
    /// Interface identifier.
    pub iface_id: String,
    /// Caller-supplied host device name. Squib maps it to a vmnet handle name.
    pub host_dev_name: String,
    /// Optional guest MAC address; auto-generated when absent.
    #[serde(default)]
    pub guest_mac: Option<String>,
    /// RX rate limiter passthrough.
    #[serde(default)]
    pub rx_rate_limiter: Option<serde_json::Value>,
    /// TX rate limiter passthrough.
    #[serde(default)]
    pub tx_rate_limiter: Option<serde_json::Value>,
}

/// Validated `/network-interfaces/{id}` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct NetworkInterfaceConfig {
    /// Validated interface ID.
    pub iface_id: IfaceId,
    /// Caller-requested host device name (informational; the vmnet handle is derived).
    pub host_dev_name: String,
    /// Validated guest MAC; `None` requests auto-generation downstream.
    pub guest_mac: Option<MacAddr>,
    /// RX rate limiter passthrough.
    pub rx_rate_limiter: Option<serde_json::Value>,
    /// TX rate limiter passthrough.
    pub tx_rate_limiter: Option<serde_json::Value>,
}

fn validate_host_dev_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Invalid host_dev_name: must not be empty".into());
    }
    if name.len() > 64 {
        return Err(format!(
            "Invalid host_dev_name: exceeds 64 bytes (got {} bytes)",
            name.len()
        ));
    }
    if name.contains('\0') {
        return Err("Invalid host_dev_name: must not contain NUL bytes".into());
    }
    Ok(())
}

impl TryFrom<RawNetworkInterfaceConfig> for NetworkInterfaceConfig {
    type Error = String;

    fn try_from(raw: RawNetworkInterfaceConfig) -> Result<Self, Self::Error> {
        let iface_id = IfaceId::new(raw.iface_id)?;
        validate_host_dev_name(&raw.host_dev_name)?;
        let guest_mac = match raw.guest_mac {
            Some(s) => Some(MacAddr::parse(&s)?),
            None => None,
        };
        Ok(Self {
            iface_id,
            host_dev_name: raw.host_dev_name,
            guest_mac,
            rx_rate_limiter: raw.rx_rate_limiter,
            tx_rate_limiter: raw.tx_rate_limiter,
        })
    }
}

/// Raw `/network-interfaces/{id}` PATCH body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawNetworkPatch {
    /// Interface ID being patched (must match the URL `{id}`).
    pub iface_id: String,
    /// Replacement RX rate limiter.
    #[serde(default)]
    pub rx_rate_limiter: Option<serde_json::Value>,
    /// Replacement TX rate limiter.
    #[serde(default)]
    pub tx_rate_limiter: Option<serde_json::Value>,
}

/// Validated `/network-interfaces/{id}` PATCH body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct NetworkPatch {
    /// Validated interface ID.
    pub iface_id: IfaceId,
    /// Replacement RX rate limiter.
    pub rx_rate_limiter: Option<serde_json::Value>,
    /// Replacement TX rate limiter.
    pub tx_rate_limiter: Option<serde_json::Value>,
}

impl TryFrom<RawNetworkPatch> for NetworkPatch {
    type Error = String;

    fn try_from(raw: RawNetworkPatch) -> Result<Self, Self::Error> {
        Ok(Self {
            iface_id: IfaceId::new(raw.iface_id)?,
            rx_rate_limiter: raw.rx_rate_limiter,
            tx_rate_limiter: raw.tx_rate_limiter,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_minimal_network_interface() {
        let raw = RawNetworkInterfaceConfig {
            iface_id: "eth0".into(),
            host_dev_name: "tap0".into(),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
        };
        let cfg = NetworkInterfaceConfig::try_from(raw).unwrap();
        assert_eq!(cfg.iface_id.as_str(), "eth0");
        assert!(cfg.guest_mac.is_none());
    }

    #[test]
    fn test_should_validate_guest_mac() {
        let raw = RawNetworkInterfaceConfig {
            iface_id: "eth0".into(),
            host_dev_name: "tap0".into(),
            guest_mac: Some("aa:bb:cc:dd:ee:ff".into()),
            rx_rate_limiter: None,
            tx_rate_limiter: None,
        };
        let cfg = NetworkInterfaceConfig::try_from(raw).unwrap();
        assert!(cfg.guest_mac.is_some());
    }

    #[test]
    fn test_should_reject_empty_host_dev_name() {
        let raw = RawNetworkInterfaceConfig {
            iface_id: "eth0".into(),
            host_dev_name: String::new(),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
        };
        assert!(NetworkInterfaceConfig::try_from(raw).is_err());
    }

    #[test]
    fn test_should_reject_unknown_fields() {
        let json = r#"{"iface_id":"eth0","host_dev_name":"tap0","unexpected":1}"#;
        assert!(serde_json::from_str::<RawNetworkInterfaceConfig>(json).is_err());
    }
}
