//! `squib` — macOS-native microVM monitor with a Firecracker-compatible API.
//!
//! Apple-Silicon-only, HVF-only, aarch64 Linux guests. This binary parses the
//! Firecracker-compatible CLI flag set documented in [50-cli.md](../../specs/50-cli.md)
//! and starts the API server on a Unix domain socket.
//!
//! macOS build: the API server is backed by the live HVF VMM event
//! loop (`vmm_loop`), which drives `build_microvm_for_boot` +
//! `run_microvm_with_budget` against accumulated API state, and a
//! UDS-backed vsock multiplexer (`vsock_muxer`). Non-macOS builds
//! retain the Phase-2 stub so the cross-platform check pipeline
//! continues to exercise the API surface.

#![allow(clippy::too_many_lines)]

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use clap::Parser;
#[cfg(not(target_os = "macos"))]
use squib_api::{ActionReceiver, ApiAction, ApiResponse};
use squib_api::{
    ControllerSnapshot, RuntimeApiController, ServeOptions, TimeoutTable, parse_config_file,
    replay_config, serve,
};
#[cfg(not(target_os = "macos"))]
use tracing::error;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod cli;
#[cfg(target_os = "macos")]
mod vmm_loop;
#[cfg(target_os = "macos")]
mod vsock_muxer;

use cli::{Args, LogLevel};

/// Channel capacity per CLAUDE.md § Async & Concurrency.
const VMM_CHANNEL_CAPACITY: usize = 1024;

const FIRECRACKER_COMPAT_VERSION: &str = "1.16.0";

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(args.level);

    // `--snapshot-version` and `--describe-snapshot` are read-only side-channels;
    // both exit before the API server starts. They short-circuit here so they do
    // not require `--api-sock` / `--config-file`.
    if args.snapshot_version {
        println!("{}", squib_snapshot::SNAPSHOT_VERSION);
        return Ok(());
    }
    if let Some(path) = args.describe_snapshot.as_deref() {
        let desc = squib_snapshot::describe(path)
            .map_err(|e| anyhow::anyhow!("describe-snapshot: {}", e.wire_message()))?;
        print!("{}", desc.human());
        if !desc.crc_ok {
            // CRC failure is operator-visible but not a hard exit — the operator
            // wanted to inspect the file, and they've now seen the warning.
            std::process::exit(2);
        }
        return Ok(());
    }

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
    // `--bridged-iface` is only meaningful in bridged mode; warn (don't fail) so
    // an operator who left the flag in a script can still use `--network=shared`
    // for everyday testing without surfacing a hard error.
    // Bridged-only knob: only meaningful when the `bridged` cargo feature is on AND the
    // operator selected `--network=bridged`. In every other build / mode, surface a one-
    // shot warning so a stray flag doesn't silently disappear into the void.
    #[cfg(feature = "bridged")]
    let is_bridged = matches!(
        net_mode,
        squib_net::NetMode::Vmnet(squib_net::VmnetMode::Bridged)
    );
    #[cfg(not(feature = "bridged"))]
    let is_bridged = false;
    if args.bridged_iface.is_some() && !is_bridged {
        warn!(
            iface = ?args.bridged_iface,
            "--bridged-iface ignored: only effective when --network=bridged",
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

    // Spawn the VMM consumer. On macOS this is the real HVF-backed
    // event loop; on other targets we keep the Phase-2 stub so the
    // non-Apple-Silicon build compiles.
    #[cfg(target_os = "macos")]
    let stub_vmm = {
        let run_budget = std::time::Duration::from_mins(5);
        let loop_cfg = vmm_loop::VmmLoopConfig {
            net_mode,
            gvproxy_path: args.gvproxy_path.clone(),
            bridged_iface: args.bridged_iface.clone(),
            run_budget,
            mmds_size_cap: args
                .mmds_size_limit
                .map_or(8192usize, |v| usize::try_from(v).unwrap_or(usize::MAX)),
        };
        let controller_for_loop = Arc::clone(&controller);
        tokio::spawn(async move { vmm_loop::run(controller_for_loop, vmm_rx, loop_cfg).await })
    };
    #[cfg(not(target_os = "macos"))]
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

/// Phase-2 stub VMM event loop — retained for non-macOS targets so the
/// cross-platform build continues to work; on macOS the real event
/// loop in [`vmm_loop`] replaces it. Acks every action with
/// `204 No Content`, except for the actions that actually need a
/// running VMM — those return a `BadRequest` with a stable
/// `fault_message` so orchestrator integrations get a deterministic
/// answer on platforms where HVF isn't available.
#[cfg(not(target_os = "macos"))]
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
                // Phase 5 ships the snapshot subsystem (`squib-snapshot`) — atomic
                // save, sparse Diff memory file, dirty-page tracking, postcopy
                // pager — but capturing live vCPU + GIC state requires the VMM
                // event loop (Track A) to be plumbed through. The stub returns a
                // deterministic `BadRequest` so SDKs see a stable shape.
                warn!(
                    action = label,
                    "snapshot subsystem available; awaiting VMM event-loop integration",
                );
                ApiResponse::Fault {
                    status: 400,
                    fault_message: "Snapshot subsystem ready but VMM event loop not yet wired \
                                    (Track A in progress)"
                        .into(),
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
