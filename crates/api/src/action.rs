//! Cross-thread `ApiAction` / `ApiResponse` enums.
//!
//! Per [10-data-model.md § 4.1](../../../specs/10-data-model.md#41-api--vmm), the API
//! layer never touches device or hypervisor state directly. Mutating handlers serialize
//! through this enum onto a bounded mpsc channel; the VMM event loop consumes the
//! channel one action at a time.
//!
//! `ApiAction` carries fully-validated payloads. Construction of any variant from raw
//! JSON goes through the corresponding `Raw*` → newtype `TryFrom`, so the VMM event
//! loop never has to re-validate.

use serde::Serialize;

use crate::schemas::{
    BalloonConfig, BalloonHintingOp, BalloonStatsUpdate, BalloonUpdate, BootSourceConfig,
    CpuConfig, DriveConfig, DriveId, DrivePatch, EntropyConfig, HotplugMemoryConfig,
    HotplugMemoryUpdate, IfaceId, InstanceAction, LoggerConfig, MachineConfig, MachineConfigPatch,
    MetricsConfig, MmdsConfig, MmdsContents, NetworkInterfaceConfig, NetworkPatch, PmemConfig,
    PmemPatch, SerialConfig, SnapshotCreateConfig, SnapshotLoadConfig, VmStateChange, VsockConfig,
};

/// Per-action class used to look up the `tokio::time::timeout` budget for an action
/// ([70-security.md § 6](../../../specs/70-security.md#6-resource-limits)).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ActionClass {
    /// Pre-boot configuration mutation (PUT/PATCH/DELETE on `/drives`,
    /// `/network-interfaces`, `/machine-config`, etc.). Default 5 s.
    PreBootConfig,
    /// `Action(InstanceStart)`. Default 30 s — boot orchestration including FDT build,
    /// kernel load, GIC create.
    InstanceStart,
    /// `PUT /snapshot/create`. Default 5 min — bounded by memory-file write throughput.
    SnapshotCreate,
    /// `PUT /snapshot/load`. Default 5 min — symmetric to `SnapshotCreate`.
    SnapshotLoad,
    /// `PATCH /vm` (Pause/Resume). Default 5 s — quiesce wait.
    VmStateChange,
    /// `PATCH /balloon` resize. Default 30 s — large balloons take time as the guest
    /// releases pages.
    BalloonResize,
    /// Other in-flight actions (e.g. `FlushMetrics`). Default 5 s.
    Other,
}

impl ActionClass {
    /// Human-readable name for log lines and `fault_message` payloads.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PreBootConfig => "pre-boot config",
            Self::InstanceStart => "InstanceStart",
            Self::SnapshotCreate => "PUT /snapshot/create",
            Self::SnapshotLoad => "PUT /snapshot/load",
            Self::VmStateChange => "PATCH /vm",
            Self::BalloonResize => "PATCH /balloon",
            Self::Other => "VMM action",
        }
    }
}

/// Mutating action posted onto the API → VMM channel.
#[derive(Debug)]
pub enum ApiAction {
    /// `PUT /boot-source`.
    PutBootSource(BootSourceConfig),
    /// `PUT /drives/{id}`.
    PutDrive(DriveConfig),
    /// `PATCH /drives/{id}`.
    PatchDrive(DrivePatch),
    /// `DELETE /drives/{id}`.
    DeleteDrive {
        /// Validated drive ID.
        drive_id: DriveId,
    },
    /// `PUT /network-interfaces/{id}`.
    PutNetwork(NetworkInterfaceConfig),
    /// `PATCH /network-interfaces/{id}`.
    PatchNetwork(NetworkPatch),
    /// `DELETE /network-interfaces/{id}`.
    DeleteNetwork {
        /// Validated interface ID.
        iface_id: IfaceId,
    },
    /// `PUT /vsock`.
    PutVsock(VsockConfig),
    /// `PUT /mmds` — replace MMDS data store.
    PutMmds(MmdsContents),
    /// `PATCH /mmds` — JSON-merge-patch on the data store.
    PatchMmds(MmdsContents),
    /// `PUT /mmds/config`.
    PutMmdsConfig(MmdsConfig),
    /// `PUT /balloon`.
    PutBalloon(BalloonConfig),
    /// `PATCH /balloon`.
    PatchBalloon(BalloonUpdate),
    /// `PATCH /balloon/statistics`.
    PatchBalloonStats(BalloonStatsUpdate),
    /// `PATCH /balloon/hinting/{op}`.
    PatchBalloonHinting {
        /// `start | status | stop`.
        op: BalloonHintingOp,
    },
    /// `PUT /entropy`.
    PutEntropy(EntropyConfig),
    /// `PUT /serial`.
    PutSerial(SerialConfig),
    /// `PUT /pmem/{id}`.
    PutPmem(PmemConfig),
    /// `PATCH /pmem/{id}`.
    PatchPmem(PmemPatch),
    /// `DELETE /pmem/{id}`.
    DeletePmem {
        /// Validated pmem ID.
        pmem_id: DriveId,
    },
    /// `PUT /hotplug/memory`.
    PutHotplugMemory(HotplugMemoryConfig),
    /// `PATCH /hotplug/memory`.
    PatchHotplugMemory(HotplugMemoryUpdate),
    /// `PUT /cpu-config`.
    PutCpuConfig(CpuConfig),
    /// `PUT /machine-config`.
    PutMachineConfig(MachineConfig),
    /// `PATCH /machine-config`.
    PatchMachineConfig(MachineConfigPatch),
    /// `PUT /logger`.
    PutLogger(LoggerConfig),
    /// `PUT /metrics`.
    PutMetrics(MetricsConfig),
    /// `PUT /actions` — every action variant including `InstanceStart`.
    Action(InstanceAction),
    /// `PATCH /vm` — pause/resume.
    PatchVm(VmStateChange),
    /// `PUT /snapshot/create`.
    SnapshotCreate(SnapshotCreateConfig),
    /// `PUT /snapshot/load`.
    SnapshotLoad(SnapshotLoadConfig),
    /// SIGINT / process shutdown.
    Shutdown,
}

impl ApiAction {
    /// The timeout class for this action.
    #[must_use]
    pub const fn class(&self) -> ActionClass {
        match self {
            Self::Action(InstanceAction::InstanceStart) => ActionClass::InstanceStart,
            Self::Action(_) | Self::Shutdown => ActionClass::Other,
            Self::PatchVm(_) => ActionClass::VmStateChange,
            Self::PatchBalloon(_) => ActionClass::BalloonResize,
            Self::SnapshotCreate(_) => ActionClass::SnapshotCreate,
            Self::SnapshotLoad(_) => ActionClass::SnapshotLoad,
            _ => ActionClass::PreBootConfig,
        }
    }

    /// `true` if this action should be admissible **before** the microVM has booted.
    #[must_use]
    pub const fn is_pre_boot(&self) -> bool {
        matches!(
            self,
            Self::PutBootSource(_)
                | Self::PutDrive(_)
                | Self::PutNetwork(_)
                | Self::PutVsock(_)
                | Self::PutMmds(_)
                | Self::PatchMmds(_)
                | Self::PutMmdsConfig(_)
                | Self::PutBalloon(_)
                | Self::PutEntropy(_)
                | Self::PutSerial(_)
                | Self::PutPmem(_)
                | Self::PutHotplugMemory(_)
                | Self::PutCpuConfig(_)
                | Self::PutMachineConfig(_)
                | Self::PatchMachineConfig(_)
                | Self::PutLogger(_)
                | Self::PutMetrics(_)
                | Self::PatchDrive(_)
                | Self::PatchNetwork(_)
                | Self::Action(InstanceAction::InstanceStart)
                | Self::SnapshotLoad(_)
                | Self::Shutdown
        )
    }

    /// `true` if this action is admissible **after** the microVM has booted.
    #[must_use]
    pub const fn is_post_boot(&self) -> bool {
        matches!(
            self,
            Self::PatchVm(_)
                | Self::PatchBalloon(_)
                | Self::PatchBalloonStats(_)
                | Self::PatchBalloonHinting { .. }
                | Self::PatchHotplugMemory(_)
                | Self::PatchMmds(_)
                | Self::PutMmds(_)
                | Self::PatchDrive(_)
                | Self::PatchNetwork(_)
                | Self::PatchPmem(_)
                | Self::DeleteDrive { .. }
                | Self::DeleteNetwork { .. }
                | Self::DeletePmem { .. }
                | Self::SnapshotCreate(_)
                | Self::Action(InstanceAction::FlushMetrics)
                | Self::Shutdown
        )
    }

    /// Human-readable label for log lines.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::PutBootSource(_) => "PUT /boot-source",
            Self::PutDrive(_) => "PUT /drives/{id}",
            Self::PatchDrive(_) => "PATCH /drives/{id}",
            Self::DeleteDrive { .. } => "DELETE /drives/{id}",
            Self::PutNetwork(_) => "PUT /network-interfaces/{id}",
            Self::PatchNetwork(_) => "PATCH /network-interfaces/{id}",
            Self::DeleteNetwork { .. } => "DELETE /network-interfaces/{id}",
            Self::PutVsock(_) => "PUT /vsock",
            Self::PutMmds(_) => "PUT /mmds",
            Self::PatchMmds(_) => "PATCH /mmds",
            Self::PutMmdsConfig(_) => "PUT /mmds/config",
            Self::PutBalloon(_) => "PUT /balloon",
            Self::PatchBalloon(_) => "PATCH /balloon",
            Self::PatchBalloonStats(_) => "PATCH /balloon/statistics",
            Self::PatchBalloonHinting { .. } => "PATCH /balloon/hinting/{op}",
            Self::PutEntropy(_) => "PUT /entropy",
            Self::PutSerial(_) => "PUT /serial",
            Self::PutPmem(_) => "PUT /pmem/{id}",
            Self::PatchPmem(_) => "PATCH /pmem/{id}",
            Self::DeletePmem { .. } => "DELETE /pmem/{id}",
            Self::PutHotplugMemory(_) => "PUT /hotplug/memory",
            Self::PatchHotplugMemory(_) => "PATCH /hotplug/memory",
            Self::PutCpuConfig(_) => "PUT /cpu-config",
            Self::PutMachineConfig(_) => "PUT /machine-config",
            Self::PatchMachineConfig(_) => "PATCH /machine-config",
            Self::PutLogger(_) => "PUT /logger",
            Self::PutMetrics(_) => "PUT /metrics",
            Self::Action(_) => "PUT /actions",
            Self::PatchVm(_) => "PATCH /vm",
            Self::SnapshotCreate(_) => "PUT /snapshot/create",
            Self::SnapshotLoad(_) => "PUT /snapshot/load",
            Self::Shutdown => "Shutdown",
        }
    }
}

/// Response posted back through the `oneshot` paired with each `ApiAction`.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ApiResponse {
    /// `204 No Content`.
    NoContent,
    /// `200 OK` with a JSON body.
    Json(serde_json::Value),
    /// `4xx` with a `fault_message`.
    Fault {
        /// HTTP status code (400, 413, ...).
        status: u16,
        /// Body `fault_message` value.
        fault_message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_classify_instance_start_for_30s_timeout() {
        let action = ApiAction::Action(InstanceAction::InstanceStart);
        assert_eq!(action.class(), ActionClass::InstanceStart);
    }

    #[test]
    fn test_should_classify_snapshot_actions_for_5min_timeout() {
        let create_label = ActionClass::SnapshotCreate.label();
        assert_eq!(create_label, "PUT /snapshot/create");
    }

    #[test]
    fn test_should_classify_pre_boot_config_default() {
        let action = ApiAction::PutEntropy(EntropyConfig::default());
        assert_eq!(action.class(), ActionClass::PreBootConfig);
    }

    #[test]
    fn test_should_label_actions_human_readable() {
        let action = ApiAction::PatchVm(VmStateChange::Paused);
        assert_eq!(action.label(), "PATCH /vm");
    }
}
