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
use squib_core::LifecyclePhase;
use tokio::sync::{mpsc, oneshot};
use tracing::error;

use crate::{
    action::{ActionClass, ApiAction, ApiResponse},
    error::ApiError,
    schemas::{InstanceAction, InstanceInfo, VmState},
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

/// Controller surfaced to handlers. Written by the VMM event loop, read by every
/// handler.
#[derive(Debug)]
pub struct RuntimeApiController {
    snapshot: ArcSwap<ControllerSnapshot>,
    vmm_tx: ActionSender,
    timeouts: TimeoutTable,
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
        let (tx, rx) = mpsc::channel(capacity);
        let controller = Self {
            snapshot: ArcSwap::from(Arc::new(snapshot)),
            vmm_tx: tx,
            timeouts,
        };
        (controller, rx)
    }

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
        let class = action.class();
        let timeout = self.timeouts.for_class(class);
        let label = action.label();
        let (resp_tx, resp_rx) = oneshot::channel();
        self.vmm_tx
            .send((action, resp_tx))
            .await
            .map_err(|_| ApiError::Internal("VMM event loop is gone".into()))?;
        match tokio::time::timeout(timeout, resp_rx).await {
            Ok(Ok(resp)) => Ok(resp),
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
