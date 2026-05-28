//! Embeddable facade for Squib microVM runtimes.
//!
//! The facade owns the same controller, config replay path, optional UDS API server,
//! and VMM event loop used by the `squib` CLI binary. Applications that want to embed
//! Squib should depend on this crate instead of wiring `squib-api` and `squib-vmm`
//! directly.
//!
//! # Example
//!
//! ```no_run
//! # async fn run() -> Result<(), squib::SquibError> {
//! let mut vm = squib::Squib::builder()
//!     .try_instance_id("demo")?
//!     .config_file("vm.json")
//!     .start_microvm(false)
//!     .spawn()
//!     .await?;
//!
//! vm.shutdown().await?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::{path::PathBuf, sync::Arc, time::Duration};

use squib_api::{
    ApiAction, ApiResponse, ControllerSnapshot, RuntimeApiController, ServeOptions, TimeoutTable,
    bind_listener, parse_config_file, replay_config, schemas::InstanceId, serve_bound,
};
use squib_net::NetMode;
use thiserror::Error;
use tokio::task::JoinHandle;
#[cfg(not(target_os = "macos"))]
use tracing::{debug, info, warn};

#[cfg(target_os = "macos")]
mod vmm_loop;
#[cfg(target_os = "macos")]
mod vsock_muxer;

/// Channel capacity per AGENTS.md § Async & Concurrency.
pub const DEFAULT_VMM_CHANNEL_CAPACITY: usize = 1024;

/// Firecracker compatibility version reported by the default facade runtime.
pub const FIRECRACKER_COMPAT_VERSION: &str = "1.16.0";

/// Default MMDS data-store size limit for embedded runtimes.
pub const DEFAULT_MMDS_SIZE_LIMIT: usize = 8192;

/// Errors returned by the embeddable facade.
#[derive(Debug, Error)]
pub enum SquibError {
    /// Instance ID failed the Firecracker identifier allowlist.
    #[error("invalid instance id: {0}")]
    InvalidInstanceId(String),

    /// Static config file parsing failed.
    #[error("config file: {source}")]
    Config {
        /// Source error from `squib-api`.
        #[from]
        source: squib_api::ReplayError,
    },

    /// API controller rejected or timed out an action.
    #[error("api action: {source}")]
    Api {
        /// Source error from `squib-api`.
        #[from]
        source: squib_api::ApiError,
    },

    /// UDS API server bind or serving failed.
    #[error("api server io: {source}")]
    Io {
        /// Source IO error.
        #[from]
        source: std::io::Error,
    },

    /// A runtime task failed to join cleanly.
    #[error("runtime task join: {source}")]
    Join {
        /// Source join error.
        #[from]
        source: tokio::task::JoinError,
    },
}

/// Builder for an embedded Squib runtime.
#[derive(Debug, Clone)]
pub struct SquibBuilder {
    instance_id: String,
    config_file: Option<PathBuf>,
    start_microvm: bool,
    api_socket: Option<PathBuf>,
    http_api_max_payload_size: usize,
    timeouts: TimeoutTable,
    channel_capacity: usize,
    network_mode: NetMode,
    gvproxy_path: Option<PathBuf>,
    bridged_iface: Option<String>,
    run_budget: Duration,
    mmds_size_limit: usize,
    firecracker_version: String,
}

impl Default for SquibBuilder {
    fn default() -> Self {
        Self {
            instance_id: "anonymous".to_string(),
            config_file: None,
            start_microvm: true,
            api_socket: None,
            http_api_max_payload_size: squib_api::DEFAULT_MAX_PAYLOAD,
            timeouts: TimeoutTable::from_spec(),
            channel_capacity: DEFAULT_VMM_CHANNEL_CAPACITY,
            network_mode: NetMode::SHARED,
            gvproxy_path: None,
            bridged_iface: None,
            run_budget: Duration::from_mins(5),
            mmds_size_limit: DEFAULT_MMDS_SIZE_LIMIT,
            firecracker_version: FIRECRACKER_COMPAT_VERSION.to_string(),
        }
    }
}

impl SquibBuilder {
    /// Set the validated microVM instance ID.
    #[must_use]
    pub fn instance_id(mut self, instance_id: &InstanceId) -> Self {
        self.instance_id = instance_id.as_str().to_string();
        self
    }

    /// Validate and set the microVM instance ID.
    ///
    /// # Errors
    /// Returns [`SquibError::InvalidInstanceId`] if `id` violates the Firecracker
    /// identifier allowlist.
    pub fn try_instance_id(self, id: impl Into<String>) -> Result<Self, SquibError> {
        let instance_id = InstanceId::new(id).map_err(SquibError::InvalidInstanceId)?;
        Ok(self.instance_id(&instance_id))
    }

    /// Replay a Firecracker-compatible static config file during startup.
    #[must_use]
    pub fn config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = Some(path.into());
        self
    }

    /// Control whether config-file replay sends `InstanceStart`.
    #[must_use]
    pub const fn start_microvm(mut self, start_microvm: bool) -> Self {
        self.start_microvm = start_microvm;
        self
    }

    /// Serve the Firecracker-compatible UDS API at `path`.
    #[must_use]
    pub fn api_socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.api_socket = Some(path.into());
        self
    }

    /// Do not serve the UDS API.
    #[must_use]
    pub fn without_api_socket(mut self) -> Self {
        self.api_socket = None;
        self
    }

    /// Set the maximum HTTP request payload size in bytes.
    #[must_use]
    pub const fn http_api_max_payload_size(mut self, bytes: usize) -> Self {
        self.http_api_max_payload_size = bytes;
        self
    }

    /// Set the API action timeout table.
    #[must_use]
    pub const fn timeouts(mut self, timeouts: TimeoutTable) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Set the bounded API-to-VMM channel capacity.
    #[must_use]
    pub const fn channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity;
        self
    }

    /// Set the host network mode.
    #[must_use]
    pub const fn network_mode(mut self, mode: NetMode) -> Self {
        self.network_mode = mode;
        self
    }

    /// Set the gvproxy binary path used by [`NetMode::Userspace`].
    #[must_use]
    pub fn gvproxy_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.gvproxy_path = Some(path.into());
        self
    }

    /// Set the bridged vmnet physical interface name.
    #[must_use]
    pub fn bridged_iface(mut self, iface: impl Into<String>) -> Self {
        self.bridged_iface = Some(iface.into());
        self
    }

    /// Set the upper bound on guest wall-clock runtime.
    #[must_use]
    pub const fn run_budget(mut self, budget: Duration) -> Self {
        self.run_budget = budget;
        self
    }

    /// Set the MMDS data-store size cap.
    #[must_use]
    pub const fn mmds_size_limit(mut self, bytes: usize) -> Self {
        self.mmds_size_limit = bytes;
        self
    }

    /// Set the Firecracker compatibility version reported by `GET /version`.
    #[must_use]
    pub fn firecracker_version(mut self, version: impl Into<String>) -> Self {
        self.firecracker_version = version.into();
        self
    }

    /// Spawn the configured runtime.
    ///
    /// # Errors
    /// Returns an error if config-file parsing/replay fails, the optional API socket
    /// cannot be bound, or the VMM channel rejects startup actions.
    pub async fn spawn(self) -> Result<Squib, SquibError> {
        let snapshot = ControllerSnapshot::new(
            self.instance_id.as_str(),
            self.firecracker_version.clone(),
            format!(
                "{} (squib {})",
                self.firecracker_version,
                env!("CARGO_PKG_VERSION")
            ),
        );
        let (controller, vmm_rx) =
            RuntimeApiController::new(snapshot, self.timeouts, self.channel_capacity);
        let controller = Arc::new(controller);

        let vmm_task = spawn_vmm_loop(Arc::clone(&controller), vmm_rx, &self);

        if let Some(config_path) = self.config_file.as_ref() {
            let cfg = parse_config_file(config_path).await?;
            replay_config(&controller, cfg, self.start_microvm).await?;
        }

        let api_task = if let Some(socket_path) = self.api_socket.as_ref() {
            let opts = ServeOptions::new(socket_path)
                .with_max_payload_size(self.http_api_max_payload_size);
            let listener = bind_listener(&opts).await?;
            let controller_for_server = Arc::clone(&controller);
            Some(tokio::spawn(async move {
                serve_bound(listener, opts, controller_for_server).await
            }))
        } else {
            None
        };

        Ok(Squib {
            controller,
            vmm_task: Some(vmm_task),
            api_task,
            shutdown_sent: false,
        })
    }
}

/// Owned handle for an embedded Squib runtime.
#[derive(Debug)]
pub struct Squib {
    controller: Arc<RuntimeApiController>,
    vmm_task: Option<JoinHandle<()>>,
    api_task: Option<JoinHandle<std::io::Result<()>>>,
    shutdown_sent: bool,
}

impl Squib {
    /// Start building an embedded Squib runtime.
    #[must_use]
    pub fn builder() -> SquibBuilder {
        SquibBuilder::default()
    }

    /// Borrow the low-level runtime controller.
    #[must_use]
    pub fn controller(&self) -> &Arc<RuntimeApiController> {
        &self.controller
    }

    /// Borrow the current read-only runtime snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Arc<ControllerSnapshot> {
        self.controller.snapshot()
    }

    /// Dispatch a validated API action through the same controller path used by HTTP handlers.
    ///
    /// # Errors
    /// Returns [`SquibError::Api`] if phase validation fails, the VMM loop is gone, or
    /// the per-action timeout fires.
    pub async fn dispatch(&self, action: ApiAction) -> Result<ApiResponse, SquibError> {
        self.controller
            .dispatch(action)
            .await
            .map_err(SquibError::from)
    }

    /// `true` if the optional API server task has completed.
    #[must_use]
    pub fn api_server_finished(&self) -> bool {
        self.api_task.as_ref().is_some_and(JoinHandle::is_finished)
    }

    /// Await the optional API server task if it has completed or if the caller wants
    /// to block until it completes.
    ///
    /// # Errors
    /// Returns IO errors from the server or join errors from the task.
    pub async fn join_api_server(&mut self) -> Result<(), SquibError> {
        let Some(task) = self.api_task.take() else {
            return Ok(());
        };
        task.await??;
        Ok(())
    }

    /// Shut the runtime down. This method is idempotent.
    ///
    /// # Errors
    /// Returns an error if the VMM or API tasks fail to join. A controller-side
    /// `Internal` error caused by an already-closed VMM loop is treated as a completed
    /// shutdown so repeated calls remain idempotent.
    pub async fn shutdown(&mut self) -> Result<(), SquibError> {
        if !self.shutdown_sent {
            self.shutdown_sent = true;
            match self.controller.dispatch(ApiAction::Shutdown).await {
                Ok(_) => {}
                Err(squib_api::ApiError::Internal(message))
                    if message == "VMM event loop is gone" => {}
                Err(err) => return Err(SquibError::from(err)),
            }
        }

        if let Some(task) = self.api_task.take() {
            task.abort();
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(Err(err)) => return Err(SquibError::from(err)),
                Err(err) if err.is_cancelled() => {}
                Err(err) => return Err(SquibError::from(err)),
            }
        }

        if let Some(task) = self.vmm_task.take() {
            task.await?;
        }
        Ok(())
    }
}

impl Drop for Squib {
    fn drop(&mut self) {
        if !self.shutdown_sent {
            self.shutdown_sent = true;
            let (ack, _rx) = tokio::sync::oneshot::channel();
            let _ = self
                .controller
                .action_sender()
                .try_send((ApiAction::Shutdown, ack));
        }
        if let Some(task) = self.api_task.take() {
            task.abort();
        }
    }
}

#[cfg(target_os = "macos")]
fn spawn_vmm_loop(
    controller: Arc<RuntimeApiController>,
    vmm_rx: squib_api::ActionReceiver,
    builder: &SquibBuilder,
) -> JoinHandle<()> {
    let cfg = vmm_loop::VmmLoopConfig {
        net_mode: builder.network_mode,
        gvproxy_path: builder.gvproxy_path.clone(),
        bridged_iface: builder.bridged_iface.clone(),
        run_budget: builder.run_budget,
        mmds_size_cap: builder.mmds_size_limit,
    };
    tokio::spawn(async move { vmm_loop::run(controller, vmm_rx, cfg).await })
}

#[cfg(not(target_os = "macos"))]
fn spawn_vmm_loop(
    _controller: Arc<RuntimeApiController>,
    vmm_rx: squib_api::ActionReceiver,
    _builder: &SquibBuilder,
) -> JoinHandle<()> {
    tokio::spawn(stub_vmm_loop(vmm_rx))
}

#[cfg(not(target_os = "macos"))]
async fn stub_vmm_loop(mut rx: squib_api::ActionReceiver) {
    while let Some((action, ack)) = rx.recv().await {
        let label = action.label();
        let response = match &action {
            ApiAction::Action(squib_api::schemas::InstanceAction::InstanceStart) => {
                warn!(
                    action = label,
                    "stub VMM: InstanceStart not wired on this platform"
                );
                ApiResponse::Fault {
                    status: 400,
                    fault_message: "InstanceStart failed: VMM backend is only available on macOS"
                        .into(),
                }
            }
            ApiAction::PatchVm(_) => ApiResponse::Fault {
                status: 400,
                fault_message: "VM state changes require a running macOS HVF backend".into(),
            },
            ApiAction::SnapshotCreate(_) | ApiAction::SnapshotLoad(_) => ApiResponse::Fault {
                status: 400,
                fault_message: "Snapshot actions require a running macOS HVF backend".into(),
            },
            ApiAction::Shutdown => {
                let _ = ack.send(ApiResponse::NoContent);
                break;
            }
            _ => ApiResponse::NoContent,
        };
        if ack.send(response).is_err() {
            debug!(action = label, "stub VMM: response channel closed");
        }
    }
    info!("stub VMM: exiting");
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use squib_api::{
        ApiResponse,
        schemas::{
            BootSourceConfig, MachineConfig, boot_source::RawBootSourceConfig,
            machine_config::RawMachineConfig,
        },
    };

    use super::*;

    fn boot_source() -> BootSourceConfig {
        BootSourceConfig::try_from(RawBootSourceConfig {
            kernel_image_path: "/tmp/kernel".into(),
            initrd_path: None,
            boot_args: None,
        })
        .unwrap_or_else(|err| panic!("valid boot source fixture: {err}"))
    }

    fn machine_config() -> MachineConfig {
        MachineConfig::try_from(RawMachineConfig {
            vcpu_count: 1,
            mem_size_mib: 128,
            smt: false,
            track_dirty_pages: false,
            cpu_template: None,
            huge_pages: None,
        })
        .unwrap_or_else(|err| panic!("valid machine config fixture: {err}"))
    }

    #[tokio::test]
    async fn test_should_spawn_dispatch_and_shutdown_stub_runtime() {
        let mut vm = Squib::builder()
            .start_microvm(false)
            .spawn()
            .await
            .unwrap_or_else(|err| panic!("spawn stub runtime: {err}"));

        let response = vm
            .dispatch(ApiAction::PutBootSource(boot_source()))
            .await
            .unwrap_or_else(|err| panic!("dispatch boot source: {err}"));
        assert!(matches!(response, ApiResponse::NoContent));

        vm.shutdown()
            .await
            .unwrap_or_else(|err| panic!("first shutdown: {err}"));
        vm.shutdown()
            .await
            .unwrap_or_else(|err| panic!("second shutdown is idempotent: {err}"));
    }

    #[tokio::test]
    async fn test_should_replay_config_without_starting_microvm() {
        let mut file = tempfile::NamedTempFile::new()
            .unwrap_or_else(|err| panic!("create config fixture: {err}"));
        write!(
            file,
            r#"{{
                "boot-source": {{"kernel_image_path": "/tmp/kernel"}},
                "machine-config": {{
                    "vcpu_count": 1,
                    "mem_size_mib": 128,
                    "smt": false,
                    "track_dirty_pages": false
                }}
            }}"#
        )
        .unwrap_or_else(|err| panic!("write config fixture: {err}"));

        let mut vm = Squib::builder()
            .config_file(file.path())
            .start_microvm(false)
            .spawn()
            .await
            .unwrap_or_else(|err| panic!("spawn with config replay: {err}"));

        let response = vm
            .dispatch(ApiAction::PutMachineConfig(machine_config()))
            .await
            .unwrap_or_else(|err| panic!("dispatch after replay: {err}"));
        assert!(matches!(response, ApiResponse::NoContent));
        vm.shutdown()
            .await
            .unwrap_or_else(|err| panic!("shutdown after replay: {err}"));
    }
}
