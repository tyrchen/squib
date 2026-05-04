//! `RuntimeApiController` — channel to VMM + lock-free read mirror + timeout taxonomy.
//!
//! Per [20-firecracker-api.md §
//! 5](../../../specs/20-firecracker-api.md#5-channel-to-vmm-and-the-read-only-fast-path):
//!
//! - `ArcSwap<ControllerSnapshot>` lock-free read mirror; written by the VMM event loop on every
//!   state transition, read by every `GET` handler.
//! - `tokio::sync::mpsc::Sender<(ApiAction, oneshot::Sender<ApiResponse>)>` single-writer channel
//!   into the VMM event loop. Bounded (capacity 1024 per CLAUDE.md § Async).
//! - Per-action-class `tokio::time::timeout` (D26); on timeout we surface 504 and log the
//!   still-pending action at `error`.
//!
//! Pre-boot vs post-boot admissibility is checked synchronously against the
//! `LifecyclePhase` carried in `ControllerSnapshot` — no VMM round-trip needed for
//! rejection.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::{sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use squib_core::LifecyclePhase;
use tokio::sync::{mpsc, oneshot};
use tracing::error;

use crate::{
    action::{ActionClass, ApiAction, ApiResponse},
    error::ApiError,
    schemas::{InstanceAction, InstanceInfo, MAX_DRIVES, MAX_NICS, MAX_PMEM, VmState},
};

/// Per-class `tokio::time::timeout` budget per
/// [70-security.md § 6](../../../specs/70-security.md#6-resource-limits).
#[derive(Debug, Clone, Copy)]
pub struct TimeoutTable {
    /// Pre-boot configuration mutations. Default 5 s.
    pub pre_boot_config: Duration,
    /// `Action(InstanceStart)`. Default 30 s.
    pub instance_start: Duration,
    /// `PUT /snapshot/create`. Default 5 min.
    pub snapshot_create: Duration,
    /// `PUT /snapshot/load`. Default 5 min.
    pub snapshot_load: Duration,
    /// `PATCH /vm`. Default 5 s.
    pub vm_state_change: Duration,
    /// `PATCH /balloon` resize. Default 30 s.
    pub balloon_resize: Duration,
    /// Other actions (e.g. `FlushMetrics`). Default 5 s.
    pub other: Duration,
}

impl TimeoutTable {
    /// Defaults from the spec.
    #[must_use]
    pub const fn from_spec() -> Self {
        Self {
            pre_boot_config: Duration::from_secs(5),
            instance_start: Duration::from_secs(30),
            snapshot_create: Duration::from_mins(5),
            snapshot_load: Duration::from_mins(5),
            vm_state_change: Duration::from_secs(5),
            balloon_resize: Duration::from_secs(30),
            other: Duration::from_secs(5),
        }
    }

    /// Look up the budget for a given action class.
    #[must_use]
    pub const fn for_class(&self, class: ActionClass) -> Duration {
        match class {
            ActionClass::PreBootConfig => self.pre_boot_config,
            ActionClass::InstanceStart => self.instance_start,
            ActionClass::SnapshotCreate => self.snapshot_create,
            ActionClass::SnapshotLoad => self.snapshot_load,
            ActionClass::VmStateChange => self.vm_state_change,
            ActionClass::BalloonResize => self.balloon_resize,
            ActionClass::Other => self.other,
        }
    }
}

impl Default for TimeoutTable {
    fn default() -> Self {
        Self::from_spec()
    }
}

/// Lock-free read mirror surfaced via every `GET` handler.
#[derive(Debug, Clone)]
pub struct ControllerSnapshot {
    /// Body of `GET /` (already collapsed to the upstream three-value vocabulary).
    pub instance_info: InstanceInfo,
    /// `firecracker_version` returned by `GET /version`.
    pub firecracker_version: String,
    /// Materialised `VmmConfig` for `GET /vm/config` — opaque JSON tree at this layer
    /// (the VMM populates it from validated typed configs).
    pub vm_config: Arc<serde_json::Value>,
    /// Internal lifecycle phase. Never serialized to the wire.
    pub phase: LifecyclePhase,
}

impl ControllerSnapshot {
    /// Build a snapshot for a freshly-launched VMM (no boot yet).
    pub fn new(
        instance_id: impl Into<String>,
        firecracker_version: impl Into<String>,
        vmm_version: impl Into<String>,
    ) -> Self {
        let firecracker_version = firecracker_version.into();
        Self {
            instance_info: InstanceInfo {
                id: instance_id.into(),
                state: VmState::NotStarted,
                vmm_version: vmm_version.into(),
                app_name: "Firecracker".into(),
            },
            firecracker_version,
            vm_config: Arc::new(serde_json::json!({})),
            phase: LifecyclePhase::Uninitialized,
        }
    }
}

/// Channel sender type used by mutating handlers.
pub type ActionSender = mpsc::Sender<(ApiAction, oneshot::Sender<ApiResponse>)>;

/// Channel receiver type owned by the VMM event loop.
pub type ActionReceiver = mpsc::Receiver<(ApiAction, oneshot::Sender<ApiResponse>)>;

/// Cross-field limits that the controller enforces synchronously, *before* an action is
/// forwarded to the VMM event loop. These are the upper bounds that the per-field
/// `Raw* → Validated TryFrom` conversions cannot enforce because they need the running
/// machine state (host RAM cap, configured `mem_size_mib`, running counts).
///
/// Per `93-improvements-review.md` Phase 2 entry — fix shape "thread `BackendCapabilities`
/// into the controller, add `MachineConfig::validate_against_host(...)` invoked at
/// dispatch time before the action reaches the VMM event loop".
#[derive(Debug)]
pub struct LimitsState {
    /// Host RAM (MiB). Defaults to a generous-but-finite value so tests have a fixed
    /// upper bound; production builds set this from the live `BackendCapabilities`.
    pub host_ram_mib: u64,
    /// Most recently configured `mem_size_mib`. `None` until the first
    /// `PUT /machine-config` lands.
    pub mem_size_mib: Option<u64>,
    /// Drives the controller has accepted (≤ [`MAX_DRIVES`] = 8).
    pub running_drives: u32,
    /// Network interfaces accepted (≤ [`MAX_NICS`] = 8).
    pub running_nics: u32,
    /// pmem devices accepted (≤ [`MAX_PMEM`] = 4).
    pub running_pmem: u32,
}

impl LimitsState {
    /// Default limits — host RAM caps generously high so that vanilla unit tests keep
    /// passing without injecting a `BackendCapabilities` mock. Production builds
    /// override `host_ram_mib` from the live HVF `BackendCapabilities` snapshot.
    #[must_use]
    pub fn from_host_ram_mib(host_ram_mib: u64) -> Self {
        Self {
            host_ram_mib,
            mem_size_mib: None,
            running_drives: 0,
            running_nics: 0,
            running_pmem: 0,
        }
    }
}

impl Default for LimitsState {
    fn default() -> Self {
        // 1 TiB; "any realistic Apple Silicon Mac" will be far below this. The
        // controller surfaces the real number from BackendCapabilities once the
        // HVF backend is plumbed.
        Self::from_host_ram_mib(1024 * 1024)
    }
}

/// Controller surfaced to handlers. Written by the VMM event loop, read by every
/// handler.
#[derive(Debug)]
pub struct RuntimeApiController {
    snapshot: ArcSwap<ControllerSnapshot>,
    vmm_tx: ActionSender,
    timeouts: TimeoutTable,
    limits: Mutex<LimitsState>,
}

impl RuntimeApiController {
    /// Build a controller paired with a VMM event loop receiver. The receiver must be
    /// drained by the VMM (or a stub for tests / Phase 2 wiring).
    ///
    /// `capacity` is the bounded mpsc capacity; CLAUDE.md § Async recommends 1024.
    #[must_use]
    pub fn new(
        snapshot: ControllerSnapshot,
        timeouts: TimeoutTable,
        capacity: usize,
    ) -> (Self, ActionReceiver) {
        Self::new_with_limits(snapshot, timeouts, capacity, LimitsState::default())
    }

    /// Build a controller with explicit cross-field limits — the production caller
    /// derives `host_ram_mib` from the live HVF `BackendCapabilities`; tests use the
    /// default 1 TiB cap.
    #[must_use]
    pub fn new_with_limits(
        snapshot: ControllerSnapshot,
        timeouts: TimeoutTable,
        capacity: usize,
        limits: LimitsState,
    ) -> (Self, ActionReceiver) {
        let (tx, rx) = mpsc::channel(capacity);
        let controller = Self {
            snapshot: ArcSwap::from(Arc::new(snapshot)),
            vmm_tx: tx,
            timeouts,
            limits: Mutex::new(limits),
        };
        (controller, rx)
    }

    /// Expose the current limits snapshot — tests use this to verify counter
    /// progression after PUTs land.
    #[must_use]
    pub fn limits_snapshot(&self) -> LimitsSnapshot {
        let g = self.limits.lock();
        LimitsSnapshot {
            host_ram_mib: g.host_ram_mib,
            mem_size_mib: g.mem_size_mib,
            running_drives: g.running_drives,
            running_nics: g.running_nics,
            running_pmem: g.running_pmem,
        }
    }

    /// Cross-field admissibility check against the running machine state.
    /// Runs *after* `validate_phase` and *before* the VMM channel is touched.
    fn validate_cross_field(&self, action: &ApiAction) -> Result<(), ApiError> {
        let g = self.limits.lock();
        match action {
            ApiAction::PutMachineConfig(cfg) => {
                let req = cfg.mem_size_mib.get();
                if req > g.host_ram_mib {
                    return Err(ApiError::BadRequest(format!(
                        "mem_size_mib={req} exceeds host RAM cap of {host} MiB",
                        host = g.host_ram_mib,
                    )));
                }
            }
            ApiAction::PutBalloon(b) => {
                cross_check_balloon(b.amount_mib, g.mem_size_mib)?;
            }
            ApiAction::PatchBalloon(u) => {
                cross_check_balloon(u.amount_mib, g.mem_size_mib)?;
            }
            ApiAction::PutDrive(_) if u64::from(g.running_drives) >= MAX_DRIVES_AS_U64 => {
                return Err(ApiError::BadRequest(format!(
                    "drives: per-class cap {MAX_DRIVES} exceeded"
                )));
            }
            ApiAction::PutNetwork(_) if u64::from(g.running_nics) >= MAX_NICS_AS_U64 => {
                return Err(ApiError::BadRequest(format!(
                    "network_interfaces: per-class cap {MAX_NICS} exceeded"
                )));
            }
            ApiAction::PutPmem(_) if u64::from(g.running_pmem) >= MAX_PMEM_AS_U64 => {
                return Err(ApiError::BadRequest(format!(
                    "pmem: per-class cap {MAX_PMEM} exceeded"
                )));
            }
            _ => {}
        }
        Ok(())
    }
}

/// Captures the *kind* of state mutation an action would commit on success, so the
/// dispatch path can apply it after the VMM event loop returns a non-fault response.
/// Capturing the kind (rather than holding `&action`) lets us forward the owned
/// `ApiAction` into the channel and still know what to bump on success.
#[derive(Debug, Clone, Copy)]
enum ActionCounterKick {
    None,
    SetMemSize(u64),
    AddDrive,
    AddNic,
    AddPmem,
}

impl ActionCounterKick {
    fn for_action(action: &ApiAction) -> Self {
        match action {
            ApiAction::PutMachineConfig(cfg) => Self::SetMemSize(cfg.mem_size_mib.get()),
            ApiAction::PutDrive(_) => Self::AddDrive,
            ApiAction::PutNetwork(_) => Self::AddNic,
            ApiAction::PutPmem(_) => Self::AddPmem,
            _ => Self::None,
        }
    }

    fn apply(self, ctl: &RuntimeApiController) {
        if matches!(self, Self::None) {
            return;
        }
        let mut g = ctl.limits.lock();
        match self {
            Self::SetMemSize(v) => g.mem_size_mib = Some(v),
            Self::AddDrive => g.running_drives = g.running_drives.saturating_add(1),
            Self::AddNic => g.running_nics = g.running_nics.saturating_add(1),
            Self::AddPmem => g.running_pmem = g.running_pmem.saturating_add(1),
            Self::None => {}
        }
    }
}

/// Read-only view of [`LimitsState`] for callers that want a momentary snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitsSnapshot {
    /// Host RAM cap (MiB).
    pub host_ram_mib: u64,
    /// Configured `mem_size_mib`; `None` before the first PUT.
    pub mem_size_mib: Option<u64>,
    /// Drives accepted so far.
    pub running_drives: u32,
    /// Network interfaces accepted so far.
    pub running_nics: u32,
    /// pmem devices accepted so far.
    pub running_pmem: u32,
}

// `MAX_*` constants in `schemas::common` are `usize`; widening to `u64` once and
// comparing against widened counters avoids the cast_possible_truncation lint without
// hand-rolling a const-checked path.
const MAX_DRIVES_AS_U64: u64 = MAX_DRIVES as u64;
const MAX_NICS_AS_U64: u64 = MAX_NICS as u64;
const MAX_PMEM_AS_U64: u64 = MAX_PMEM as u64;

fn cross_check_balloon(amount_mib: u64, mem_size_mib: Option<u64>) -> Result<(), ApiError> {
    // If `mem_size_mib` hasn't been configured yet (no PUT /machine-config), defer the
    // bound check — upstream Firecracker accepts the balloon config at this point and
    // re-validates it at `InstanceStart`. Squib mirrors that behaviour: only enforce
    // the cap when both halves of the cross-field pair are present.
    let Some(mem) = mem_size_mib else {
        return Ok(());
    };
    // Upstream Firecracker `MAX_BALLOON_SIZE_MIB` is `mem_size_mib − 32`; squib mirrors
    // it here. `mem - 32` saturates to zero on tiny VMs to avoid underflow surfacing as
    // a confusing "exceeds u64::MAX − 32" message.
    let max_balloon = mem.saturating_sub(32);
    if amount_mib > max_balloon {
        return Err(ApiError::BadRequest(format!(
            "balloon amount_mib={amount_mib} exceeds max ({max_balloon} = mem_size_mib {mem} - 32)"
        )));
    }
    Ok(())
}

impl RuntimeApiController {
    /// Borrow the current snapshot. Returns an `Arc` so the caller can drop it without
    /// holding a lock — this is the read-only fast path.
    #[must_use]
    pub fn snapshot(&self) -> Arc<ControllerSnapshot> {
        self.snapshot.load_full()
    }

    /// Replace the snapshot atomically. Called by the VMM event loop on every state
    /// transition; not exposed to handlers.
    pub fn store_snapshot(&self, snap: ControllerSnapshot) {
        self.snapshot.store(Arc::new(snap));
    }

    /// Validate admissibility synchronously against the cached lifecycle phase.
    ///
    /// Per spec § 5.2: pre-flight rejection runs before the channel is ever touched.
    pub fn validate_phase(&self, action: &ApiAction) -> Result<(), ApiError> {
        let phase = self.snapshot.load().phase;
        // Two architectural rules: (a) `SendCtrlAltDel` is x86-only and rejected
        // unconditionally (R row in the compat matrix). (b) `Shutdown` is always
        // admissible.
        if let ApiAction::Action(InstanceAction::SendCtrlAltDel) = action {
            return Err(ApiError::BadRequest(
                "Invalid action: SendCtrlAltDel is x86-only and not supported on aarch64".into(),
            ));
        }
        if matches!(action, ApiAction::Shutdown) {
            return Ok(());
        }
        // Ordinary admissibility: pre-boot before boot, post-boot after.
        if phase.is_pre_boot() && !action.is_pre_boot() {
            return Err(ApiError::BadRequest(
                "The requested operation is not allowed before the microVM has booted".into(),
            ));
        }
        if phase.is_post_boot() && !action.is_post_boot() {
            return Err(ApiError::BadRequest(
                "The requested operation is not supported after the microVM has booted".into(),
            ));
        }
        if matches!(phase, LifecyclePhase::Starting) {
            return Err(ApiError::BadRequest(
                "The requested operation cannot be served during boot orchestration".into(),
            ));
        }
        if matches!(phase, LifecyclePhase::Shutdown) {
            return Err(ApiError::Internal("VMM is shut down".into()));
        }
        Ok(())
    }

    /// Dispatch an action to the VMM event loop. Applies the per-class timeout.
    pub async fn dispatch(&self, action: ApiAction) -> Result<ApiResponse, ApiError> {
        self.validate_phase(&action)?;
        self.validate_cross_field(&action)?;
        let class = action.class();
        let timeout = self.timeouts.for_class(class);
        let label = action.label();
        // Take a peek at the action shape so we can apply the post-success counter
        // mutation against an `ApiAction` we no longer own. Cheap because the
        // discriminant + the small embedded fields we touch are cloneable.
        let counter_kick = ActionCounterKick::for_action(&action);
        let (resp_tx, resp_rx) = oneshot::channel();
        self.vmm_tx
            .send((action, resp_tx))
            .await
            .map_err(|_| ApiError::Internal("VMM event loop is gone".into()))?;
        match tokio::time::timeout(timeout, resp_rx).await {
            Ok(Ok(resp)) => {
                if matches!(resp, ApiResponse::NoContent | ApiResponse::Json { .. }) {
                    counter_kick.apply(self);
                }
                Ok(resp)
            }
            Ok(Err(_)) => Err(ApiError::Internal("VMM event loop is gone".into())),
            Err(_) => {
                error!(
                    action = label,
                    timeout_secs = timeout.as_secs(),
                    "VMM action timed out; the action remains pending at the VMM",
                );
                Err(ApiError::Timeout(class.label()))
            }
        }
    }

    /// Borrow the underlying timeout table (used in tests).
    #[must_use]
    pub fn timeouts(&self) -> TimeoutTable {
        self.timeouts
    }

    /// Borrow the action sender — used for tests that want to bypass `dispatch` to
    /// drive the channel directly.
    #[must_use]
    pub fn action_sender(&self) -> ActionSender {
        self.vmm_tx.clone()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use squib_core::LifecyclePhase;

    use super::*;
    use crate::schemas::{BootSourceConfig, EntropyConfig, VmStateChange};

    fn ctl(phase: LifecyclePhase) -> (RuntimeApiController, ActionReceiver) {
        let mut snap = ControllerSnapshot::new("anonymous", "1.16.0", "1.16.0 (squib 0.1.0)");
        snap.phase = phase;
        snap.instance_info.state = phase.wire_state().into();
        RuntimeApiController::new(snap, TimeoutTable::from_spec(), 16)
    }

    fn boot_source() -> BootSourceConfig {
        BootSourceConfig::try_from(crate::schemas::boot_source::RawBootSourceConfig {
            kernel_image_path: "/tmp/k".into(),
            initrd_path: None,
            boot_args: None,
        })
        .unwrap()
    }

    #[test]
    fn test_should_admit_pre_boot_action_in_uninitialized() {
        let (c, _rx) = ctl(LifecyclePhase::Uninitialized);
        let action = ApiAction::PutBootSource(boot_source());
        c.validate_phase(&action).unwrap();
    }

    #[test]
    fn test_should_reject_post_boot_action_in_uninitialized() {
        let (c, _rx) = ctl(LifecyclePhase::Uninitialized);
        let action = ApiAction::PatchVm(VmStateChange::Paused);
        let err = c.validate_phase(&action).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn test_should_reject_pre_boot_action_in_running() {
        let (c, _rx) = ctl(LifecyclePhase::Running);
        let action = ApiAction::PutEntropy(EntropyConfig::default());
        let err = c.validate_phase(&action).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn test_should_admit_pause_in_running() {
        let (c, _rx) = ctl(LifecyclePhase::Running);
        let action = ApiAction::PatchVm(VmStateChange::Paused);
        c.validate_phase(&action).unwrap();
    }

    #[test]
    fn test_should_reject_send_ctrl_alt_del_with_upstream_message() {
        let (c, _rx) = ctl(LifecyclePhase::Running);
        let action = ApiAction::Action(InstanceAction::SendCtrlAltDel);
        let err = c.validate_phase(&action).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
        assert!(err.fault_message().contains("SendCtrlAltDel"));
    }

    #[test]
    fn test_should_reject_anything_in_shutdown() {
        let (c, _rx) = ctl(LifecyclePhase::Shutdown);
        let action = ApiAction::PutEntropy(EntropyConfig::default());
        let err = c.validate_phase(&action).unwrap_err();
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[test]
    fn test_should_reject_during_starting_phase() {
        let (c, _rx) = ctl(LifecyclePhase::Starting);
        let action = ApiAction::PutEntropy(EntropyConfig::default());
        assert!(c.validate_phase(&action).is_err());
    }

    #[tokio::test]
    async fn test_should_surface_504_on_action_timeout() {
        // Build a controller whose pre-boot timeout is 50 ms and never drain the
        // receiver — the dispatch must surface a Timeout(504).
        let mut snap = ControllerSnapshot::new("anonymous", "1.16.0", "1.16.0 (squib test)");
        snap.phase = LifecyclePhase::Uninitialized;
        snap.instance_info.state = VmState::NotStarted;
        let mut t = TimeoutTable::from_spec();
        t.pre_boot_config = Duration::from_millis(50);
        let (c, _rx) = RuntimeApiController::new(snap, t, 16);
        let action = ApiAction::PutBootSource(boot_source());
        let res = c.dispatch(action).await;
        assert!(matches!(res, Err(ApiError::Timeout(_))));
    }

    #[tokio::test]
    async fn test_should_dispatch_to_vmm_and_return_no_content() {
        let (c, mut rx) = ctl(LifecyclePhase::Uninitialized);
        let action = ApiAction::PutBootSource(boot_source());

        // Spawn a task that drains the channel and acks 204.
        tokio::spawn(async move {
            if let Some((_action, ack)) = rx.recv().await {
                let _ = ack.send(ApiResponse::NoContent);
            }
        });

        let resp = c.dispatch(action).await.unwrap();
        assert!(matches!(resp, ApiResponse::NoContent));
    }

    #[tokio::test]
    async fn test_should_surface_500_when_event_loop_drops_response() {
        let (c, rx) = ctl(LifecyclePhase::Uninitialized);
        let action = ApiAction::PutBootSource(boot_source());

        // Drop the receiver first — drains the channel and drops oneshot senders.
        tokio::spawn(async move {
            let mut rx = rx;
            if let Some((_action, ack)) = rx.recv().await {
                drop(ack);
            }
        });

        let res = c.dispatch(action).await;
        assert!(matches!(res, Err(ApiError::Internal(_))));
    }

    fn machine_cfg(mem_mib: u64) -> crate::schemas::MachineConfig {
        crate::schemas::MachineConfig::try_from(crate::schemas::machine_config::RawMachineConfig {
            vcpu_count: 1,
            mem_size_mib: mem_mib,
            smt: false,
            track_dirty_pages: false,
            cpu_template: None,
            huge_pages: None,
        })
        .unwrap()
    }

    fn drive_cfg(id: &str) -> crate::schemas::DriveConfig {
        crate::schemas::DriveConfig::try_from(crate::schemas::drive::RawDriveConfig {
            drive_id: id.into(),
            path_on_host: format!("/tmp/{id}.img"),
            is_root_device: false,
            is_read_only: true,
            cache_type: crate::schemas::drive::CacheType::Unsafe,
            io_engine: crate::schemas::drive::IoEngine::default(),
            partuuid: None,
            rate_limiter: None,
            socket: None,
        })
        .unwrap()
    }

    fn balloon_cfg(amount_mib: u64) -> crate::schemas::BalloonConfig {
        crate::schemas::BalloonConfig::try_from(crate::schemas::balloon::RawBalloonConfig {
            amount_mib,
            deflate_on_oom: false,
            stats_polling_interval_s: 0,
            free_page_hinting: false,
            free_page_reporting: false,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn test_should_reject_machine_config_above_host_ram_cap() {
        let mut snap = ControllerSnapshot::new("test", "1.16.0", "1.16.0 (squib test)");
        snap.phase = LifecyclePhase::Uninitialized;
        let limits = LimitsState::from_host_ram_mib(256);
        let (c, _rx) =
            RuntimeApiController::new_with_limits(snap, TimeoutTable::from_spec(), 16, limits);
        let action = ApiAction::PutMachineConfig(machine_cfg(1024));
        let err = c.dispatch(action).await.unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
        assert!(err.fault_message().contains("host RAM cap"));
    }

    #[tokio::test]
    async fn test_should_reject_balloon_above_mem_minus_32() {
        let (c, mut rx) = ctl(LifecyclePhase::Uninitialized);
        // First, set mem_size to 256 via a successful PutMachineConfig.
        tokio::spawn(async move {
            while let Some((_action, ack)) = rx.recv().await {
                let _ = ack.send(ApiResponse::NoContent);
            }
        });
        c.dispatch(ApiAction::PutMachineConfig(machine_cfg(256)))
            .await
            .unwrap();
        assert_eq!(c.limits_snapshot().mem_size_mib, Some(256));

        // 256 - 32 = 224; a 256 MiB balloon overflows by 32.
        let err = c
            .dispatch(ApiAction::PutBalloon(balloon_cfg(256)))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
        assert!(err.fault_message().contains("exceeds max"));
    }

    #[tokio::test]
    async fn test_should_defer_balloon_cap_check_when_mem_size_not_yet_set() {
        // Without a prior PutMachineConfig the controller cannot evaluate
        // `mem_size_mib - 32`; rather than rejecting (which would diverge from
        // upstream Firecracker semantics), it defers the cap check to boot.
        let (c, mut rx) = ctl(LifecyclePhase::Uninitialized);
        tokio::spawn(async move {
            while let Some((_action, ack)) = rx.recv().await {
                let _ = ack.send(ApiResponse::NoContent);
            }
        });
        let resp = c
            .dispatch(ApiAction::PutBalloon(balloon_cfg(64)))
            .await
            .unwrap();
        assert!(matches!(resp, ApiResponse::NoContent));
    }

    #[tokio::test]
    async fn test_should_enforce_drives_class_cap_via_running_count() {
        let (c, mut rx) = ctl(LifecyclePhase::Uninitialized);
        tokio::spawn(async move {
            while let Some((_action, ack)) = rx.recv().await {
                let _ = ack.send(ApiResponse::NoContent);
            }
        });
        // Eight successful drives bump the counter to MAX_DRIVES.
        for i in 0..8 {
            c.dispatch(ApiAction::PutDrive(drive_cfg(&format!("d{i}"))))
                .await
                .unwrap();
        }
        assert_eq!(c.limits_snapshot().running_drives, 8);
        // Ninth must reject without round-tripping the channel.
        let err = c
            .dispatch(ApiAction::PutDrive(drive_cfg("d9")))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
        assert!(err.fault_message().contains("drives"));
    }

    #[tokio::test]
    async fn test_should_not_bump_running_count_on_vmm_fault() {
        let (c, mut rx) = ctl(LifecyclePhase::Uninitialized);
        tokio::spawn(async move {
            while let Some((_action, ack)) = rx.recv().await {
                let _ = ack.send(ApiResponse::Fault {
                    status: 400,
                    fault_message: "stub VMM rejected this".into(),
                });
            }
        });
        let _ = c
            .dispatch(ApiAction::PutDrive(drive_cfg("d0")))
            .await
            .unwrap();
        // The VMM faulted → running_drives must remain 0.
        assert_eq!(c.limits_snapshot().running_drives, 0);
    }

    #[test]
    fn test_should_apply_default_timeouts_per_spec() {
        let t = TimeoutTable::from_spec();
        assert_eq!(
            t.for_class(ActionClass::PreBootConfig),
            Duration::from_secs(5)
        );
        assert_eq!(
            t.for_class(ActionClass::InstanceStart),
            Duration::from_secs(30)
        );
        assert_eq!(
            t.for_class(ActionClass::SnapshotCreate),
            Duration::from_mins(5)
        );
        assert_eq!(
            t.for_class(ActionClass::SnapshotLoad),
            Duration::from_mins(5)
        );
    }
}
