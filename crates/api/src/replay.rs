//! `--config-file` static-config replay path.
//!
//! Per [20-firecracker-api.md §
//! 6](../../../specs/20-firecracker-api.md#6-static-config-file---config-file): a static-config
//! file is parsed, validated, and **replayed** through the same `RuntimeApiController::dispatch`
//! path as the HTTP server. There is no parallel codepath; the same validation, the same errors,
//! the same logging.
//!
//! The replay order is deterministic:
//! `machine-config` → `cpu-config` → `boot-source` → drives → NICs → vsock →
//! mmds-config → mmds → balloon → entropy → serial → pmem → hotplug-memory → logger →
//! metrics → `Action(InstanceStart)` (only when `start_microvm = true`).

use std::{path::Path, sync::Arc};

use thiserror::Error;

use crate::{
    action::{ApiAction, ApiResponse},
    controller::RuntimeApiController,
    error::ApiError,
    schemas::{
        BalloonConfig, BootSourceConfig, ConfigFile, CpuConfig, DriveConfig, EntropyConfig,
        HotplugMemoryConfig, InstanceAction, LoggerConfig, MachineConfig, MetricsConfig,
        MmdsConfig, MmdsContents, NetworkInterfaceConfig, PmemConfig, SerialConfig, VsockConfig,
        common::{MAX_DRIVES, MAX_NICS, MAX_PMEM},
    },
};

/// Errors produced while replaying a static configuration file.
#[derive(Debug, Error)]
pub enum ReplayError {
    /// File could not be read.
    #[error("failed to read --config-file {path}: {source}")]
    Io {
        /// Failing path.
        path: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
    /// File could not be parsed as JSON.
    #[error("failed to parse --config-file {path}: {source}")]
    Parse {
        /// Failing path.
        path: String,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// `boot-source` is mandatory.
    #[error("--config-file is missing the required `boot-source` member")]
    MissingBootSource,
    /// Per-class collection caps overflow.
    #[error("--config-file collection {field} exceeds {max} entries")]
    CollectionCap {
        /// Field name.
        field: &'static str,
        /// Per-class cap.
        max: usize,
    },
    /// A nested validation rejected an action.
    #[error("--config-file rejected: {0}")]
    Validation(String),
    /// The controller surfaced an error while dispatching the action.
    #[error("--config-file action `{label}` failed: {source}")]
    Dispatch {
        /// Action label.
        label: &'static str,
        /// Underlying API error surfaced through dispatch.
        #[source]
        source: ApiError,
    },
    /// VMM responded with a `fault_message`.
    #[error("--config-file action `{label}` rejected by VMM (status {status}): {fault_message}")]
    Fault {
        /// Action label.
        label: &'static str,
        /// HTTP-equivalent status returned by the VMM.
        status: u16,
        /// Wire-shape `fault_message`.
        fault_message: String,
    },
}

/// Read and parse a static-config file. The file is read fully into memory; the API
/// body limit does not apply (the file is local and trusted to the operator).
pub async fn parse_config_file(path: impl AsRef<Path>) -> Result<ConfigFile, ReplayError> {
    let path = path.as_ref();
    let bytes = tokio::fs::read(path).await.map_err(|e| ReplayError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    let cfg: ConfigFile = serde_json::from_slice(&bytes).map_err(|e| ReplayError::Parse {
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(cfg)
}

/// Replay a parsed [`ConfigFile`] through the controller. If `start_microvm` is true,
/// a final `Action(InstanceStart)` is dispatched after all configuration actions
/// succeed (matches the spec's `--no-api` semantics).
pub async fn replay_config(
    controller: &Arc<RuntimeApiController>,
    cfg: ConfigFile,
    start_microvm: bool,
) -> Result<(), ReplayError> {
    if cfg.boot_source.is_none() {
        return Err(ReplayError::MissingBootSource);
    }

    if cfg.drives.len() > MAX_DRIVES {
        return Err(ReplayError::CollectionCap {
            field: "drives",
            max: MAX_DRIVES,
        });
    }
    if cfg.network_interfaces.len() > MAX_NICS {
        return Err(ReplayError::CollectionCap {
            field: "network-interfaces",
            max: MAX_NICS,
        });
    }
    if cfg.pmem.len() > MAX_PMEM {
        return Err(ReplayError::CollectionCap {
            field: "pmem",
            max: MAX_PMEM,
        });
    }

    // 1. machine-config
    if let Some(raw) = cfg.machine_config {
        let validated = MachineConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutMachineConfig(validated)).await?;
    }
    // 2. cpu-config
    if let Some(raw) = cfg.cpu_config {
        let validated = CpuConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutCpuConfig(validated)).await?;
    }
    // 3. boot-source — already proven to exist above.
    if let Some(raw) = cfg.boot_source {
        let validated = BootSourceConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutBootSource(validated)).await?;
    }
    // 4. drives
    for raw in cfg.drives {
        let validated = DriveConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutDrive(validated)).await?;
    }
    // 5. network-interfaces
    for raw in cfg.network_interfaces {
        let validated = NetworkInterfaceConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutNetwork(validated)).await?;
    }
    // 6. vsock
    if let Some(raw) = cfg.vsock {
        let validated = VsockConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutVsock(validated)).await?;
    }
    // 7. mmds-config
    if let Some(raw) = cfg.mmds_config {
        let validated = MmdsConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutMmdsConfig(validated)).await?;
    }
    // 8. mmds (data store seed)
    if let Some(value) = cfg.mmds {
        dispatch(controller, ApiAction::PutMmds(MmdsContents::new(value))).await?;
    }
    // 9. balloon
    if let Some(raw) = cfg.balloon {
        let validated = BalloonConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutBalloon(validated)).await?;
    }
    // 10. entropy
    if let Some(raw) = cfg.entropy {
        let validated = EntropyConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutEntropy(validated)).await?;
    }
    // 11. serial
    if let Some(raw) = cfg.serial {
        let validated = SerialConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutSerial(validated)).await?;
    }
    // 12. pmem
    for raw in cfg.pmem {
        let validated = PmemConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutPmem(validated)).await?;
    }
    // 13. hotplug-memory
    if let Some(raw) = cfg.hotplug_memory {
        let validated = HotplugMemoryConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutHotplugMemory(validated)).await?;
    }
    // 14. logger
    if let Some(raw) = cfg.logger {
        let validated = LoggerConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutLogger(validated)).await?;
    }
    // 15. metrics
    if let Some(raw) = cfg.metrics {
        let validated = MetricsConfig::try_from(raw).map_err(ReplayError::Validation)?;
        dispatch(controller, ApiAction::PutMetrics(validated)).await?;
    }
    // 16. InstanceStart (only with `start_microvm = true`).
    if start_microvm {
        dispatch(controller, ApiAction::Action(InstanceAction::InstanceStart)).await?;
    }
    Ok(())
}

async fn dispatch(
    controller: &Arc<RuntimeApiController>,
    action: ApiAction,
) -> Result<(), ReplayError> {
    let label = action.label();
    let resp = controller
        .dispatch(action)
        .await
        .map_err(|e| ReplayError::Dispatch { label, source: e })?;
    match resp {
        ApiResponse::NoContent | ApiResponse::Json(_) => Ok(()),
        ApiResponse::Fault {
            status,
            fault_message,
        } => Err(ReplayError::Fault {
            label,
            status,
            fault_message,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::controller::{ControllerSnapshot, TimeoutTable};

    fn build_controller() -> (Arc<RuntimeApiController>, crate::controller::ActionReceiver) {
        let snap = ControllerSnapshot::new("anonymous", "1.16.0", "1.16.0 (squib test)");
        let (c, rx) = RuntimeApiController::new(snap, TimeoutTable::from_spec(), 64);
        (Arc::new(c), rx)
    }

    fn drain_acker(
        mut rx: crate::controller::ActionReceiver,
    ) -> tokio::task::JoinHandle<Vec<&'static str>> {
        tokio::spawn(async move {
            let mut labels = Vec::new();
            while let Some((action, ack)) = rx.recv().await {
                labels.push(action.label());
                let _ = ack.send(ApiResponse::NoContent);
            }
            labels
        })
    }

    #[tokio::test]
    async fn test_should_reject_config_without_boot_source() {
        let (c, _rx) = build_controller();
        let cfg = ConfigFile::default();
        let res = replay_config(&c, cfg, false).await;
        assert!(matches!(res, Err(ReplayError::MissingBootSource)));
    }

    #[tokio::test]
    async fn test_should_replay_minimal_config_in_order() {
        let (c, rx) = build_controller();
        let drain = drain_acker(rx);

        let cfg: ConfigFile = serde_json::from_str(
            r#"{
                "boot-source": {"kernel_image_path":"/tmp/k"},
                "machine-config": {"vcpu_count":1,"mem_size_mib":256}
            }"#,
        )
        .unwrap();
        replay_config(&c, cfg, false).await.unwrap();
        drop(c);
        let labels = tokio::time::timeout(Duration::from_millis(500), drain)
            .await
            .unwrap()
            .unwrap();
        // machine-config must precede boot-source in the deterministic replay order.
        assert_eq!(labels[0], "PUT /machine-config");
        assert_eq!(labels[1], "PUT /boot-source");
    }

    #[tokio::test]
    async fn test_should_dispatch_instance_start_when_requested() {
        let (c, rx) = build_controller();
        let drain = drain_acker(rx);

        let cfg: ConfigFile = serde_json::from_str(
            r#"{"boot-source":{"kernel_image_path":"/tmp/k"},
                 "machine-config":{"vcpu_count":1,"mem_size_mib":256}}"#,
        )
        .unwrap();
        replay_config(&c, cfg, true).await.unwrap();
        drop(c);
        let labels = tokio::time::timeout(Duration::from_millis(500), drain)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*labels.last().unwrap(), "PUT /actions");
    }

    #[tokio::test]
    async fn test_should_reject_drives_over_cap() {
        let (c, _rx) = build_controller();
        let mut cfg = ConfigFile {
            boot_source: Some(crate::schemas::boot_source::RawBootSourceConfig {
                kernel_image_path: "/tmp/k".into(),
                initrd_path: None,
                boot_args: None,
            }),
            ..ConfigFile::default()
        };
        for i in 0..=MAX_DRIVES {
            cfg.drives.push(crate::schemas::drive::RawDriveConfig {
                drive_id: format!("d_{i}"),
                path_on_host: "/tmp/x".into(),
                is_root_device: false,
                is_read_only: false,
                cache_type: crate::schemas::drive::CacheType::Unsafe,
                io_engine: crate::schemas::drive::IoEngine::Sync,
                partuuid: None,
                rate_limiter: None,
                socket: None,
            });
        }
        let res = replay_config(&c, cfg, false).await;
        assert!(matches!(
            res,
            Err(ReplayError::CollectionCap {
                field: "drives",
                ..
            })
        ));
    }
}
