//! Axum handlers per [20-firecracker-api.md §
//! 2](../../../specs/20-firecracker-api.md#2-server-shape).
//!
//! Three handler categories:
//!
//! - **Read-only fast path** (§ 5.1) — `GET /`, `GET /version`, `GET /vm/config`, `GET
//!   /machine-config`, `GET /balloon`, `GET /balloon/statistics`, `GET /mmds`. These read directly
//!   from `ArcSwap` and never touch the VMM channel, so a long-running `PUT /snapshot/load` cannot
//!   cause liveness probes to time out.
//! - **Mutating** — every PUT/PATCH/DELETE/POST. Validates the JSON body via `Raw<T> → T::TryFrom`,
//!   validates phase, dispatches to the VMM through [`RuntimeApiController`], applies the per-class
//!   timeout.
//! - **Fallback** — unknown paths translated to `400 Bad Request` per the upstream shape (squib has
//!   no 404; see [20 § 3](../../../specs/20-firecracker-api.md#3-error-envelope)).

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::{
    action::{ApiAction, ApiResponse},
    controller::RuntimeApiController,
    error::{ApiError, Result},
    schemas::{
        BalloonConfig, BalloonHintingOp, BalloonStatsUpdate, BalloonUpdate, BootSourceConfig,
        CpuConfig, DriveConfig, DriveId, DrivePatch, EntropyConfig, HotplugMemoryConfig,
        HotplugMemoryUpdate, IfaceId, InstanceActionInfo, InstanceInfo, LoggerConfig,
        MachineConfig, MachineConfigPatch, MetricsConfig, MmdsConfig, MmdsContents,
        NetworkInterfaceConfig, NetworkPatch, PmemConfig, PmemPatch, SerialConfig,
        SnapshotCreateConfig, SnapshotLoadConfig, VersionResponse, VmStateChange, VsockConfig,
        actions::RawInstanceActionInfo,
        balloon::{RawBalloonConfig, RawBalloonStatsUpdate, RawBalloonUpdate},
        boot_source::RawBootSourceConfig,
        cpu_config::RawCpuConfig,
        drive::{RawDriveConfig, RawDrivePatch},
        entropy::RawEntropyConfig,
        hotplug_memory::{RawHotplugMemoryConfig, RawHotplugMemoryUpdate},
        logger::RawLoggerConfig,
        machine_config::{RawMachineConfig, RawMachineConfigPatch},
        metrics::RawMetricsConfig,
        mmds::RawMmdsConfig,
        network::{RawNetworkInterfaceConfig, RawNetworkPatch},
        pmem::{RawPmemConfig, RawPmemPatch},
        serial::RawSerialConfig,
        snapshot::{RawSnapshotCreateConfig, RawSnapshotLoadConfig},
        vm::RawVmPatch,
        vsock::RawVsockConfig,
    },
};

/// Convert a validation error string into a 400 `ApiError::BadRequest`.
///
/// Every `Raw* → Validated` `TryFrom` returns `Error = String`; this collapses the
/// repeated `.map_err(ApiError::BadRequest)?` boilerplate at every handler.
fn bad(s: String) -> ApiError {
    ApiError::BadRequest(s)
}

/// Helper: turn the controller dispatch result into an axum response.
fn finish(resp: ApiResponse) -> Response {
    match resp {
        ApiResponse::NoContent => StatusCode::NO_CONTENT.into_response(),
        ApiResponse::Json(v) => (StatusCode::OK, Json(v)).into_response(),
        ApiResponse::Fault {
            status,
            fault_message,
        } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
            (status, Json(crate::error::FaultMessage::new(fault_message))).into_response()
        }
    }
}

// ---------------- Read-only fast path ----------------

/// `GET /` — `InstanceInfo`. Read-only fast path; never touches the VMM channel.
pub async fn get_root(
    State(controller): State<Arc<RuntimeApiController>>,
) -> Result<Json<InstanceInfo>> {
    Ok(Json(controller.snapshot().instance_info.clone()))
}

/// `GET /version` — Firecracker-compat version string.
pub async fn get_version(
    State(controller): State<Arc<RuntimeApiController>>,
) -> Result<Json<VersionResponse>> {
    Ok(Json(VersionResponse {
        firecracker_version: controller.snapshot().firecracker_version.clone(),
    }))
}

/// `GET /vm/config` — materialized `VmmConfig` (read-only fast path).
pub async fn get_vm_config(
    State(controller): State<Arc<RuntimeApiController>>,
) -> Result<Json<serde_json::Value>> {
    Ok(Json((*controller.snapshot().vm_config).clone()))
}

/// `GET /machine-config` — read-only mirror of the last-applied machine config (or an
/// empty object pre-boot).
pub async fn get_machine_config(
    State(controller): State<Arc<RuntimeApiController>>,
) -> Result<Json<serde_json::Value>> {
    let snap = controller.snapshot();
    let cfg = snap
        .vm_config
        .get("machine-config")
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(Json(cfg))
}

/// `GET /balloon` — read-only mirror.
pub async fn get_balloon(
    State(controller): State<Arc<RuntimeApiController>>,
) -> Result<Json<serde_json::Value>> {
    let snap = controller.snapshot();
    let cfg = snap
        .vm_config
        .get("balloon")
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(Json(cfg))
}

/// `GET /balloon/statistics` — read-only mirror.
pub async fn get_balloon_statistics(
    State(controller): State<Arc<RuntimeApiController>>,
) -> Result<Json<serde_json::Value>> {
    let snap = controller.snapshot();
    let stats = snap
        .vm_config
        .get("balloon_statistics")
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(Json(stats))
}

/// `GET /mmds` — return the MMDS data store tree.
pub async fn get_mmds(
    State(controller): State<Arc<RuntimeApiController>>,
) -> Result<Json<serde_json::Value>> {
    let snap = controller.snapshot();
    let tree = snap
        .vm_config
        .get("mmds")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Ok(Json(tree))
}

// ---------------- Mutating handlers ----------------

/// `PUT /boot-source`.
pub async fn put_boot_source(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawBootSourceConfig>,
) -> Result<Response> {
    let cfg = BootSourceConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutBootSource(cfg)).await?;
    Ok(finish(resp))
}

/// `PUT /machine-config`.
pub async fn put_machine_config(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawMachineConfig>,
) -> Result<Response> {
    let cfg = MachineConfig::try_from(raw).map_err(bad)?;
    let resp = controller
        .dispatch(ApiAction::PutMachineConfig(cfg))
        .await?;
    Ok(finish(resp))
}

/// `PATCH /machine-config`.
pub async fn patch_machine_config(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawMachineConfigPatch>,
) -> Result<Response> {
    let patch = MachineConfigPatch::try_from(raw).map_err(bad)?;
    let resp = controller
        .dispatch(ApiAction::PatchMachineConfig(patch))
        .await?;
    Ok(finish(resp))
}

/// `PUT /drives/{id}`. The URL `{id}` must match the body `drive_id`.
pub async fn put_drive(
    State(controller): State<Arc<RuntimeApiController>>,
    Path(id): Path<String>,
    Json(raw): Json<RawDriveConfig>,
) -> Result<Response> {
    if raw.drive_id != id {
        return Err(ApiError::BadRequest(
            "Invalid drive: URL id and body drive_id must match".into(),
        ));
    }
    let cfg = DriveConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutDrive(cfg)).await?;
    Ok(finish(resp))
}

/// `PATCH /drives/{id}`.
pub async fn patch_drive(
    State(controller): State<Arc<RuntimeApiController>>,
    Path(id): Path<String>,
    Json(raw): Json<RawDrivePatch>,
) -> Result<Response> {
    if raw.drive_id != id {
        return Err(ApiError::BadRequest(
            "Invalid drive: URL id and body drive_id must match".into(),
        ));
    }
    let patch = DrivePatch::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PatchDrive(patch)).await?;
    Ok(finish(resp))
}

/// `DELETE /drives/{id}`.
pub async fn delete_drive(
    State(controller): State<Arc<RuntimeApiController>>,
    Path(id): Path<String>,
) -> Result<Response> {
    let drive_id = DriveId::new(id).map_err(bad)?;
    let resp = controller
        .dispatch(ApiAction::DeleteDrive { drive_id })
        .await?;
    Ok(finish(resp))
}

/// `PUT /network-interfaces/{id}`.
pub async fn put_network(
    State(controller): State<Arc<RuntimeApiController>>,
    Path(id): Path<String>,
    Json(raw): Json<RawNetworkInterfaceConfig>,
) -> Result<Response> {
    if raw.iface_id != id {
        return Err(ApiError::BadRequest(
            "Invalid network-interface: URL id and body iface_id must match".into(),
        ));
    }
    let cfg = NetworkInterfaceConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutNetwork(cfg)).await?;
    Ok(finish(resp))
}

/// `PATCH /network-interfaces/{id}`.
pub async fn patch_network(
    State(controller): State<Arc<RuntimeApiController>>,
    Path(id): Path<String>,
    Json(raw): Json<RawNetworkPatch>,
) -> Result<Response> {
    if raw.iface_id != id {
        return Err(ApiError::BadRequest(
            "Invalid network-interface: URL id and body iface_id must match".into(),
        ));
    }
    let patch = NetworkPatch::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PatchNetwork(patch)).await?;
    Ok(finish(resp))
}

/// `DELETE /network-interfaces/{id}`.
pub async fn delete_network(
    State(controller): State<Arc<RuntimeApiController>>,
    Path(id): Path<String>,
) -> Result<Response> {
    let iface_id = IfaceId::new(id).map_err(bad)?;
    let resp = controller
        .dispatch(ApiAction::DeleteNetwork { iface_id })
        .await?;
    Ok(finish(resp))
}

/// `PUT /vsock`.
pub async fn put_vsock(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawVsockConfig>,
) -> Result<Response> {
    let cfg = VsockConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutVsock(cfg)).await?;
    Ok(finish(resp))
}

/// `PUT /mmds` — replace the data store.
pub async fn put_mmds(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(value): Json<serde_json::Value>,
) -> Result<Response> {
    let resp = controller
        .dispatch(ApiAction::PutMmds(MmdsContents::new(value)))
        .await?;
    Ok(finish(resp))
}

/// `PATCH /mmds` — JSON-merge-patch the data store.
pub async fn patch_mmds(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(value): Json<serde_json::Value>,
) -> Result<Response> {
    let resp = controller
        .dispatch(ApiAction::PatchMmds(MmdsContents::new(value)))
        .await?;
    Ok(finish(resp))
}

/// `PUT /mmds/config`.
pub async fn put_mmds_config(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawMmdsConfig>,
) -> Result<Response> {
    let cfg = MmdsConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutMmdsConfig(cfg)).await?;
    Ok(finish(resp))
}

/// `PUT /balloon`.
pub async fn put_balloon(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawBalloonConfig>,
) -> Result<Response> {
    let cfg = BalloonConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutBalloon(cfg)).await?;
    Ok(finish(resp))
}

/// `PATCH /balloon`.
pub async fn patch_balloon(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawBalloonUpdate>,
) -> Result<Response> {
    let upd = BalloonUpdate::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PatchBalloon(upd)).await?;
    Ok(finish(resp))
}

/// `PATCH /balloon/statistics`.
pub async fn patch_balloon_statistics(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawBalloonStatsUpdate>,
) -> Result<Response> {
    let upd = BalloonStatsUpdate::try_from(raw).map_err(bad)?;
    let resp = controller
        .dispatch(ApiAction::PatchBalloonStats(upd))
        .await?;
    Ok(finish(resp))
}

/// `PATCH /balloon/hinting/{op}` handler. The op is validated against `start | status | stop`.
///
/// Body is ignored upstream (the entire signal lives in the `{op}` URL segment); we
/// accept any JSON body to stay shape-compatible.
pub async fn patch_balloon_hinting(
    State(controller): State<Arc<RuntimeApiController>>,
    Path(op): Path<String>,
) -> Result<Response> {
    let op = BalloonHintingOp::from_url_segment(&op).map_err(bad)?;
    let resp = controller
        .dispatch(ApiAction::PatchBalloonHinting { op })
        .await?;
    Ok(finish(resp))
}

/// `PUT /entropy`.
pub async fn put_entropy(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawEntropyConfig>,
) -> Result<Response> {
    let cfg = EntropyConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutEntropy(cfg)).await?;
    Ok(finish(resp))
}

/// `PUT /serial`.
pub async fn put_serial(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawSerialConfig>,
) -> Result<Response> {
    let cfg = SerialConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutSerial(cfg)).await?;
    Ok(finish(resp))
}

/// `PUT /pmem/{id}`.
pub async fn put_pmem(
    State(controller): State<Arc<RuntimeApiController>>,
    Path(id): Path<String>,
    Json(raw): Json<RawPmemConfig>,
) -> Result<Response> {
    if raw.pmem_id != id {
        return Err(ApiError::BadRequest(
            "Invalid pmem: URL id and body pmem_id must match".into(),
        ));
    }
    let cfg = PmemConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutPmem(cfg)).await?;
    Ok(finish(resp))
}

/// `PATCH /pmem/{id}`.
pub async fn patch_pmem(
    State(controller): State<Arc<RuntimeApiController>>,
    Path(id): Path<String>,
    Json(raw): Json<RawPmemPatch>,
) -> Result<Response> {
    if raw.pmem_id != id {
        return Err(ApiError::BadRequest(
            "Invalid pmem: URL id and body pmem_id must match".into(),
        ));
    }
    let patch = PmemPatch::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PatchPmem(patch)).await?;
    Ok(finish(resp))
}

/// `DELETE /pmem/{id}`.
pub async fn delete_pmem(
    State(controller): State<Arc<RuntimeApiController>>,
    Path(id): Path<String>,
) -> Result<Response> {
    let pmem_id = DriveId::new(id).map_err(bad)?;
    let resp = controller
        .dispatch(ApiAction::DeletePmem { pmem_id })
        .await?;
    Ok(finish(resp))
}

/// `GET /hotplug/memory` — read-only mirror.
pub async fn get_hotplug_memory(
    State(controller): State<Arc<RuntimeApiController>>,
) -> Result<Json<serde_json::Value>> {
    let snap = controller.snapshot();
    let cfg = snap
        .vm_config
        .get("hotplug-memory")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(Json(cfg))
}

/// `PUT /hotplug/memory`.
pub async fn put_hotplug_memory(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawHotplugMemoryConfig>,
) -> Result<Response> {
    let cfg = HotplugMemoryConfig::try_from(raw).map_err(bad)?;
    let resp = controller
        .dispatch(ApiAction::PutHotplugMemory(cfg))
        .await?;
    Ok(finish(resp))
}

/// `PATCH /hotplug/memory`.
pub async fn patch_hotplug_memory(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawHotplugMemoryUpdate>,
) -> Result<Response> {
    let upd = HotplugMemoryUpdate::try_from(raw).map_err(bad)?;
    let resp = controller
        .dispatch(ApiAction::PatchHotplugMemory(upd))
        .await?;
    Ok(finish(resp))
}

/// `PUT /cpu-config`.
pub async fn put_cpu_config(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawCpuConfig>,
) -> Result<Response> {
    let cfg = CpuConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutCpuConfig(cfg)).await?;
    Ok(finish(resp))
}

/// `PUT /actions`.
pub async fn put_actions(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawInstanceActionInfo>,
) -> Result<Response> {
    let info = InstanceActionInfo::from(raw);
    let resp = controller
        .dispatch(ApiAction::Action(info.action_type))
        .await?;
    Ok(finish(resp))
}

/// `PATCH /vm` — pause/resume.
pub async fn patch_vm(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawVmPatch>,
) -> Result<Response> {
    let action = match raw.state {
        VmStateChange::Paused => ApiAction::PatchVm(VmStateChange::Paused),
        VmStateChange::Resumed => ApiAction::PatchVm(VmStateChange::Resumed),
    };
    let resp = controller.dispatch(action).await?;
    Ok(finish(resp))
}

/// `PUT /snapshot/create`.
pub async fn put_snapshot_create(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawSnapshotCreateConfig>,
) -> Result<Response> {
    let cfg = SnapshotCreateConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::SnapshotCreate(cfg)).await?;
    Ok(finish(resp))
}

/// `PUT /snapshot/load`.
pub async fn put_snapshot_load(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawSnapshotLoadConfig>,
) -> Result<Response> {
    let cfg = SnapshotLoadConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::SnapshotLoad(cfg)).await?;
    Ok(finish(resp))
}

/// `PUT /logger`.
pub async fn put_logger(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawLoggerConfig>,
) -> Result<Response> {
    let cfg = LoggerConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutLogger(cfg)).await?;
    Ok(finish(resp))
}

/// `PUT /metrics`.
pub async fn put_metrics(
    State(controller): State<Arc<RuntimeApiController>>,
    Json(raw): Json<RawMetricsConfig>,
) -> Result<Response> {
    let cfg = MetricsConfig::try_from(raw).map_err(bad)?;
    let resp = controller.dispatch(ApiAction::PutMetrics(cfg)).await?;
    Ok(finish(resp))
}

/// Fallback for unmatched paths — translate to 400 with a `fault_message`.
pub async fn fallback(uri: Uri) -> ApiError {
    ApiError::NotFound(uri.to_string())
}
