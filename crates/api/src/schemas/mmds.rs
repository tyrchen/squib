//! `/mmds` and `/mmds/config` bodies.
//!
//! Per [21-api-compat-matrix.md `/mmds/config`
//! PUT](../../../specs/21-api-compat-matrix.md#mmdsconfig-put):
//!
//! - `version` — `V1 | V2`.
//! - `network_interfaces` — list of `iface_id`s, max 8 (matches `MAX_NICS`).
//! - `ipv4_address` — link-local in `169.254.0.0/16`; default `169.254.169.254`.
//! - `imds_compat` — bool default false.
//! - `token_ttl_seconds` (V2 only) — `1..=21600`.

use std::net::{IpAddr, Ipv4Addr};

use serde::{Deserialize, Serialize};

use super::common::{IfaceId, MAX_NICS};

/// Maximum value for `token_ttl_seconds`, matching upstream's `MAX_TOKEN_TTL_SECONDS`.
pub const MAX_TOKEN_TTL_SECONDS: u64 = 21_600;

/// MMDS protocol version.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
pub enum MmdsVersion {
    /// `IMDSv1` — no token negotiation.
    V1,
    /// `IMDSv2` — token-based, default for new instances.
    #[default]
    V2,
}

/// Raw `/mmds/config` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMmdsConfig {
    /// MMDS protocol version.
    #[serde(default)]
    pub version: MmdsVersion,
    /// IDs of network interfaces the dumbo intercept binds to.
    pub network_interfaces: Vec<String>,
    /// Optional IMDS link-local address; default `169.254.169.254`.
    #[serde(default)]
    pub ipv4_address: Option<Ipv4Addr>,
    /// `imds_compat` overrides the `Accept` header to plain text.
    #[serde(default)]
    pub imds_compat: bool,
    /// V2 token TTL in seconds (1..=21600); ignored for V1.
    #[serde(default)]
    pub token_ttl_seconds: Option<u64>,
}

/// Validated `/mmds/config` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct MmdsConfig {
    /// MMDS protocol version.
    pub version: MmdsVersion,
    /// Validated `iface_id`s.
    pub network_interfaces: Vec<IfaceId>,
    /// Resolved IMDS IPv4 (always link-local).
    pub ipv4_address: Ipv4Addr,
    /// IMDS-compat flag.
    pub imds_compat: bool,
    /// Validated V2 token TTL (`1..=21600`).
    pub token_ttl_seconds: u64,
}

fn is_link_local(addr: Ipv4Addr) -> bool {
    let octets = addr.octets();
    octets[0] == 169 && octets[1] == 254
}

impl TryFrom<RawMmdsConfig> for MmdsConfig {
    type Error = String;

    fn try_from(raw: RawMmdsConfig) -> Result<Self, Self::Error> {
        if raw.network_interfaces.is_empty() {
            return Err("Invalid mmds-config: network_interfaces must not be empty".into());
        }
        if raw.network_interfaces.len() > MAX_NICS {
            return Err(format!(
                "Invalid mmds-config: network_interfaces exceeds {MAX_NICS} entries"
            ));
        }
        let mut ifaces = Vec::with_capacity(raw.network_interfaces.len());
        for id in raw.network_interfaces {
            ifaces.push(IfaceId::new(id)?);
        }
        let ipv4_address = raw
            .ipv4_address
            .unwrap_or_else(|| Ipv4Addr::new(169, 254, 169, 254));
        if !is_link_local(ipv4_address) {
            return Err(format!(
                "Invalid ipv4_address: {ipv4_address} is not in 169.254.0.0/16"
            ));
        }
        let token_ttl_seconds = raw.token_ttl_seconds.unwrap_or(MAX_TOKEN_TTL_SECONDS);
        if token_ttl_seconds == 0 || token_ttl_seconds > MAX_TOKEN_TTL_SECONDS {
            return Err(format!(
                "Invalid token_ttl_seconds: must be 1..={MAX_TOKEN_TTL_SECONDS}"
            ));
        }
        Ok(Self {
            version: raw.version,
            network_interfaces: ifaces,
            ipv4_address,
            imds_compat: raw.imds_compat,
            token_ttl_seconds,
        })
    }
}

/// `PUT /mmds` body — arbitrary user JSON tree, capped by `--mmds-size-limit`.
///
/// We serialize the tree back through `serde_json::Value` so unknown structure is
/// preserved (the MMDS data store *is* dynamic; unlike every other endpoint here, it
/// has no fixed schema). The size cap is enforced by the `RequestBodyLimitLayer`
/// middleware against `--mmds-size-limit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MmdsContents(pub serde_json::Value);

impl MmdsContents {
    /// Wrap a serde tree.
    pub fn new(tree: serde_json::Value) -> Self {
        Self(tree)
    }
}

/// Helper: returns `Some(addr)` if the address falls in the link-local range.
#[must_use]
pub fn link_local_or_default(addr: Option<IpAddr>) -> Option<Ipv4Addr> {
    match addr {
        Some(IpAddr::V4(v4)) if is_link_local(v4) => Some(v4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> RawMmdsConfig {
        RawMmdsConfig {
            version: MmdsVersion::V2,
            network_interfaces: vec!["eth0".into()],
            ipv4_address: None,
            imds_compat: false,
            token_ttl_seconds: None,
        }
    }

    #[test]
    fn test_should_accept_minimal_mmds_config() {
        let cfg = MmdsConfig::try_from(raw()).unwrap();
        assert_eq!(cfg.ipv4_address, Ipv4Addr::new(169, 254, 169, 254));
        assert_eq!(cfg.token_ttl_seconds, MAX_TOKEN_TTL_SECONDS);
    }

    #[test]
    fn test_should_reject_empty_network_interfaces() {
        let mut r = raw();
        r.network_interfaces.clear();
        assert!(MmdsConfig::try_from(r).is_err());
    }

    #[test]
    fn test_should_reject_too_many_network_interfaces() {
        let mut r = raw();
        r.network_interfaces = (0..=MAX_NICS).map(|i| format!("eth{i}")).collect();
        assert!(MmdsConfig::try_from(r).is_err());
    }

    #[test]
    fn test_should_reject_non_link_local_ipv4() {
        let mut r = raw();
        r.ipv4_address = Some(Ipv4Addr::new(10, 0, 0, 1));
        assert!(MmdsConfig::try_from(r).is_err());
    }

    #[test]
    fn test_should_reject_token_ttl_at_zero() {
        let mut r = raw();
        r.token_ttl_seconds = Some(0);
        assert!(MmdsConfig::try_from(r).is_err());
    }

    #[test]
    fn test_should_reject_token_ttl_above_cap() {
        let mut r = raw();
        r.token_ttl_seconds = Some(MAX_TOKEN_TTL_SECONDS + 1);
        assert!(MmdsConfig::try_from(r).is_err());
    }
}
