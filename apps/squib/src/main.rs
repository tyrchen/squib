//! `squib` — macOS-native microVM monitor with a Firecracker-compatible API.
//!
//! Apple-Silicon-only, HVF-only, aarch64 Linux guests. This binary parses the
//! Firecracker-compatible CLI flag set documented in [50-cli.md](../../specs/50-cli.md)
//! and starts the API server on a Unix domain socket.
//!
//! Phase 2 ships the full API surface against a stub VMM event loop; the HVF backend
//! is wired in by Phase 1. Until the HVF/runtime pieces land, every dispatched
//! `ApiAction` returns a `204 No Content` for `Put*`/`Patch*`/`Delete*` and a
//! `400 BadRequest` with `fault_message="VMM not yet wired"` for `Action(InstanceStart)`
//! / `PUT /snapshot/*` so orchestrators get a deterministic shape rather than a hang.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use clap::Parser;
use squib_api::{
    ActionReceiver, ApiAction, ApiResponse, ControllerSnapshot, RuntimeApiController, ServeOptions,
    TimeoutTable, parse_config_file, replay_config, serve,
};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

mod cli;

use cli::{Args, LogLevel};

/// Channel capacity per CLAUDE.md § Async & Concurrency.
const VMM_CHANNEL_CAPACITY: usize = 1024;

const FIRECRACKER_COMPAT_VERSION: &str = "1.16.0";

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(args.level);

    if args.no_api && args.config_file.is_none() {
        anyhow::bail!("--no-api requires --config-file");
    }

    if args.seccomp_filter.is_some() || args.no_seccomp {
        info!("seccomp options are accepted for Firecracker compatibility but no-op on macOS");
    }
    if args.enable_pci {
        info!("--enable-pci is accepted for compatibility; squib uses virtio-MMIO transport");
    }

    // Validate the requested network mode; surface the bridged-mode build-time
    // gate before any VMM action runs. Phase 4 wires the host-side backends
    // through the device manager — the actual VmnetIface/gvproxy spawn happens
    // inside the VMM event loop when `Action(InstanceStart)` lands.
    let net_mode = args.network.to_net_mode().map_err(|e| anyhow::anyhow!(e))?;
    if matches!(net_mode, squib_net::NetMode::Userspace) && args.gvproxy_path.is_none() {
        warn!(
            "--network=userspace selected without --gvproxy-path; falling back to \
             /usr/local/libexec/squib/gvproxy"
        );
    }

    let snapshot = ControllerSnapshot::new(
        args.id.clone(),
        FIRECRACKER_COMPAT_VERSION,
        format!(
            "{FIRECRACKER_COMPAT_VERSION} (squib {})",
            env!("CARGO_PKG_VERSION")
        ),
    );
    let (controller, vmm_rx) =
        RuntimeApiController::new(snapshot, TimeoutTable::from_spec(), VMM_CHANNEL_CAPACITY);
    let controller = Arc::new(controller);

    // Spawn the stub VMM consumer. Track A's HVF backend replaces this with the real
    // VMM event loop when Phase 1 lands.
    let stub_vmm = tokio::spawn(stub_vmm_loop(vmm_rx));

    if let Some(config_path) = args.config_file.clone() {
        replay_static_config(&controller, &config_path, args.no_api).await?;
    }

    if args.no_api {
        info!(
            instance_id = %args.id,
            "squib running without API socket (--no-api). The static config has been replayed.",
        );
        // With --no-api there is no axum server to keep alive; let the stub VMM run
        // until SIGINT (Ctrl-C). We rely on tokio's signal handler.
        tokio::signal::ctrl_c().await.ok();
        info!("SIGINT received; shutting down");
        let _ = stub_vmm.await;
        return Ok(());
    }

    let socket_path = args
        .api_sock
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--api-sock must be a path"))?;
    let opts = ServeOptions::new(&socket_path)
        .with_max_payload_size(args.http_api_max_payload_size as usize);

    info!(
        instance_id = %args.id,
        api_sock = %socket_path.display(),
        config_file = %display_path(args.config_file.as_deref()),
        network = ?args.network,
        "squib API server starting (HVF backend wiring is pending — Track A)",
    );

    serve(opts, controller).await?;
    let _ = stub_vmm.await;
    Ok(())
}

async fn replay_static_config(
    controller: &Arc<RuntimeApiController>,
    path: &PathBuf,
    start_microvm: bool,
) -> Result<()> {
    let cfg = parse_config_file(path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    replay_config(controller, cfg, start_microvm)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// Phase-2 stub VMM event loop. Acks every action with `204 No Content`, except for
/// the actions that actually need a running VMM — those return a `BadRequest` with a
/// stable `fault_message` so orchestrator integrations get a deterministic answer.
///
/// Phase 1 replaces this with the real event loop in `squib-vmm`.
async fn stub_vmm_loop(mut rx: ActionReceiver) {
    while let Some((action, ack)) = rx.recv().await {
        let label = action.label();
        let response = match &action {
            ApiAction::Action(squib_api::schemas::InstanceAction::InstanceStart) => {
                warn!(action = label, "stub VMM: InstanceStart not yet wired");
                ApiResponse::Fault {
                    status: 400,
                    fault_message: "VMM not yet wired (Track A in progress)".into(),
                }
            }
            ApiAction::SnapshotCreate(_) | ApiAction::SnapshotLoad(_) => {
                warn!(action = label, "stub VMM: snapshot path not yet wired");
                ApiResponse::Fault {
                    status: 400,
                    fault_message: "Snapshot subsystem not yet implemented (Phase 5)".into(),
                }
            }
            ApiAction::Shutdown => {
                info!("stub VMM: received Shutdown, draining");
                let _ = ack.send(ApiResponse::NoContent);
                break;
            }
            _ => ApiResponse::NoContent,
        };
        if ack.send(response).is_err() {
            error!(action = label, "stub VMM: response dropped");
        }
    }
}

fn init_tracing(level: LogLevel) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level.as_directive()));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn display_path(p: Option<&std::path::Path>) -> String {
    p.map_or_else(|| "<unset>".to_string(), |p| p.display().to_string())
}
