//! UDS server harness used by every compat-suite test.
//!
//! Spins up the real `squib-api` `axum::Router` on a per-test socket and a stub VMM
//! drainer that mirrors the production binary's `stub_vmm_loop`. Mirrors the shape
//! used by `crates/api/tests/uds_server.rs` so the compat suite asserts the same wire
//! contract that production exposes.

use std::{
    path::{Path, PathBuf},
    process,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use squib_api::{
    ActionReceiver, ApiAction, ApiResponse, ControllerSnapshot, RuntimeApiController, ServeOptions,
    TimeoutTable, schemas::InstanceAction, serve, unlink_socket_if_exists,
};
use squib_core::LifecyclePhase;
use tokio::task::JoinHandle;

/// Stub VMM behaviour the harness applies to the `(action, ack)` channel.
///
/// `Production` mirrors `apps/squib/src/main.rs::stub_vmm_loop` — the shape end-to-end
/// SDK soak tests observe. `AlwaysAck` drains every action with `204 No Content`,
/// useful when a test only cares about the request-line / parsing path.
#[derive(Debug, Clone, Copy)]
pub enum StubBehaviour {
    /// Mirror the production binary's stub VMM (rejects `InstanceStart` and snapshot
    /// actions with the documented `fault_message`).
    Production,
    /// Acknowledge every action with `204 No Content`.
    AlwaysAck,
}

/// Live compat-suite server. Drop closes the socket and joins the drainer.
#[derive(Debug)]
pub struct CompatServer {
    socket: PathBuf,
    serve_handle: JoinHandle<()>,
    drainer: JoinHandle<()>,
}

impl CompatServer {
    /// Spawn the server with the production stub-VMM behaviour, in `Uninitialized`
    /// (pre-boot) phase.
    pub async fn spawn() -> Self {
        Self::spawn_with(LifecyclePhase::Uninitialized, StubBehaviour::Production).await
    }

    /// Spawn the server in a specific lifecycle phase with the requested stub
    /// behaviour. Panics on bind failure — tests treat the harness as infallible.
    pub async fn spawn_with(phase: LifecyclePhase, stub: StubBehaviour) -> Self {
        let socket = unique_socket_path();
        unlink_socket_if_exists(&socket)
            .await
            .expect("unlink stale socket");

        let mut snap = ControllerSnapshot::new("anonymous", "1.16.0", "1.16.0 (squib compat)");
        snap.phase = phase;
        snap.instance_info.state = phase.wire_state().into();
        let (controller, vmm_rx) = RuntimeApiController::new(snap, TimeoutTable::from_spec(), 64);
        let controller = Arc::new(controller);

        let drainer = tokio::spawn(stub_vmm_loop(vmm_rx, stub));

        let opts = ServeOptions::new(&socket);
        let serve_handle = tokio::spawn(async move {
            if let Err(err) = serve(opts, controller).await {
                panic!("compat server failed: {err}");
            }
        });

        // Wait for the bind to land before returning. The 2 s budget mirrors the
        // crates/api UDS test harness; on any reasonable host the bind is
        // sub-millisecond.
        for _ in 0..100 {
            if socket.exists() {
                return Self {
                    socket,
                    serve_handle,
                    drainer,
                };
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "compat server failed to bind {} within 2s",
            socket.display()
        );
    }

    /// Path of the bound Unix domain socket.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Tear down the server, dropping handles. Best-effort; joining the serve task is
    /// not strictly necessary because dropping the controller closes the channel.
    pub async fn shutdown(self) {
        // The serve future runs until the listener is dropped; we abort it
        // explicitly so test runtimes don't leak.
        self.serve_handle.abort();
        self.drainer.abort();
        let _ = unlink_socket_if_exists(&self.socket).await;
    }
}

/// Stub VMM event loop. Mirrors `apps/squib/src/main.rs::stub_vmm_loop` for
/// `Production` and a flat `204` for `AlwaysAck`.
async fn stub_vmm_loop(mut rx: ActionReceiver, behaviour: StubBehaviour) {
    while let Some((action, ack)) = rx.recv().await {
        let response = match (behaviour, &action) {
            (StubBehaviour::Production, ApiAction::Action(InstanceAction::InstanceStart)) => {
                ApiResponse::Fault {
                    status: 400,
                    fault_message: "VMM not yet wired (Track A in progress)".into(),
                }
            }
            (
                StubBehaviour::Production,
                ApiAction::SnapshotCreate(_) | ApiAction::SnapshotLoad(_),
            ) => ApiResponse::Fault {
                status: 400,
                fault_message: "Snapshot subsystem ready but VMM event loop not yet wired (Track \
                                A in progress)"
                    .into(),
            },
            (_, ApiAction::Shutdown) => {
                let _ = ack.send(ApiResponse::NoContent);
                break;
            }
            _ => ApiResponse::NoContent,
        };
        let _ = ack.send(response);
    }
}

fn unique_socket_path() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = process::id();
    std::env::temp_dir().join(format!("squib-compat-{pid}-{n}.sock"))
}
