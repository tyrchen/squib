//! `--config-file` static-config envelope ([20-firecracker-api.md §
//! 6](../../../specs/20-firecracker-api.md#6-static-config-file---config-file)).
//!
//! Kebab-case top-level keys per upstream. The `"squib": {...}` extension is
//! `#[serde(default)]` so files remain portable in the squib→firecracker direction;
//! upstream Firecracker silently ignores unknown top-level keys. The `"squib"`
//! sub-object itself is `deny_unknown_fields` so typos inside it fail loudly.
//!
//! Top-level `deny_unknown_fields` is **not** applied to this envelope: per the spec
//! we tolerate unknown keys for forward-compat with future squib extensions.

use serde::Deserialize;

use super::{
    balloon::RawBalloonConfig, boot_source::RawBootSourceConfig, cpu_config::RawCpuConfig,
    drive::RawDriveConfig, entropy::RawEntropyConfig, logger::RawLoggerConfig,
    machine_config::RawMachineConfig, metrics::RawMetricsConfig, mmds::RawMmdsConfig,
    network::RawNetworkInterfaceConfig, pmem::RawPmemConfig, serial::RawSerialConfig,
    vsock::RawVsockConfig,
};

/// Networking mode squib understands. Mirrors `apps/squib::cli::NetworkMode`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SquibNetworkMode {
    /// vmnet-shared (NAT). Default.
    #[default]
    Shared,
    /// vmnet-bridged.
    Bridged,
    /// vmnet-host (host-only network).
    Host,
    /// Embedded gvproxy userspace stack.
    Userspace,
}

/// Squib-only extension keys. Always `#[serde(default)]` and never required.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SquibExtension {
    /// Networking mode override.
    #[serde(default)]
    pub network: Option<SquibNetworkMode>,
    /// Opt-in TSI mode for vsock.
    #[serde(default)]
    pub vsock_tsi: bool,
    /// Path to a bundled `gvproxy` binary.
    #[serde(default)]
    pub gvproxy_path: Option<String>,
    /// Optional macOS sandbox profile name.
    #[serde(default)]
    pub macos_sandbox_profile: Option<String>,
}

/// Static-config-file envelope.
///
/// Replayed in the deterministic order documented in [20 §
/// 6](../../../specs/20-firecracker-api.md#6-static-config-file---config-file): `machine-config` →
/// `cpu-config` → `boot-source` → drives → NICs → vsock → mmds-config → mmds → balloon → entropy →
/// serial → pmem → hotplug-memory → logger → metrics → `Action(InstanceStart)` (only if `--no-api`
/// is also set).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ConfigFile {
    /// `boot-source` is the only mandatory member at the controller level. Marked
    /// optional here so the parser can produce a precise "missing boot-source" error
    /// rather than serde's terser one.
    #[serde(default)]
    pub boot_source: Option<RawBootSourceConfig>,
    /// `drives` array (max 8 — enforced at the controller).
    #[serde(default)]
    pub drives: Vec<RawDriveConfig>,
    /// `machine-config`.
    #[serde(default)]
    pub machine_config: Option<RawMachineConfig>,
    /// `cpu-config`.
    #[serde(default)]
    pub cpu_config: Option<RawCpuConfig>,
    /// `network-interfaces` array (max 8).
    #[serde(default)]
    pub network_interfaces: Vec<RawNetworkInterfaceConfig>,
    /// `vsock`.
    #[serde(default)]
    pub vsock: Option<RawVsockConfig>,
    /// `mmds-config`.
    #[serde(default)]
    pub mmds_config: Option<RawMmdsConfig>,
    /// `mmds` data store seed (any JSON tree).
    #[serde(default)]
    pub mmds: Option<serde_json::Value>,
    /// `balloon`.
    #[serde(default)]
    pub balloon: Option<RawBalloonConfig>,
    /// `entropy`.
    #[serde(default)]
    pub entropy: Option<RawEntropyConfig>,
    /// `serial`.
    #[serde(default)]
    pub serial: Option<RawSerialConfig>,
    /// `pmem` array (max 4).
    #[serde(default)]
    pub pmem: Vec<RawPmemConfig>,
    /// `hotplug-memory`.
    #[serde(default)]
    pub hotplug_memory: Option<super::hotplug_memory::RawHotplugMemoryConfig>,
    /// `logger`.
    #[serde(default)]
    pub logger: Option<RawLoggerConfig>,
    /// `metrics`.
    #[serde(default)]
    pub metrics: Option<RawMetricsConfig>,
    /// Squib-only extension sub-object (forward-compat).
    #[serde(default)]
    pub squib: SquibExtension,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_parse_minimal_config_file() {
        let json = r#"{"boot-source":{"kernel_image_path":"/tmp/k"}}"#;
        let cfg: ConfigFile = serde_json::from_str(json).unwrap();
        assert!(cfg.boot_source.is_some());
        assert!(cfg.drives.is_empty());
    }

    #[test]
    fn test_should_tolerate_unknown_top_level_keys() {
        let json = r#"{"boot-source":{"kernel_image_path":"/tmp/k"},"future-key":42}"#;
        let cfg: ConfigFile = serde_json::from_str(json).unwrap();
        assert!(cfg.boot_source.is_some());
    }

    #[test]
    fn test_should_reject_unknown_keys_inside_squib_extension() {
        let json = r#"{"squib":{"unknown":1}}"#;
        let res: Result<ConfigFile, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }

    #[test]
    fn test_should_parse_squib_network_mode() {
        let json = r#"{"squib":{"network":"shared"}}"#;
        let cfg: ConfigFile = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.squib.network, Some(SquibNetworkMode::Shared));
    }

    #[test]
    fn test_should_parse_full_envelope() {
        let json = r#"{
            "boot-source": {"kernel_image_path":"/tmp/k","boot_args":"console=ttyAMA0"},
            "machine-config": {"vcpu_count":2,"mem_size_mib":256},
            "drives": [{"drive_id":"rootfs","path_on_host":"/tmp/r.img","is_root_device":true}],
            "network-interfaces": [{"iface_id":"eth0","host_dev_name":"tap0"}],
            "logger": {"log_path":"/tmp/squib.log"},
            "squib": {"network":"userspace","vsock_tsi":false}
        }"#;
        let cfg: ConfigFile = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.machine_config.as_ref().unwrap().vcpu_count, 2);
        assert_eq!(cfg.drives.len(), 1);
        assert_eq!(cfg.network_interfaces.len(), 1);
        assert_eq!(cfg.squib.network, Some(SquibNetworkMode::Userspace));
    }
}
