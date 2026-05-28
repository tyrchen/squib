//! `squib` — macOS-native microVM monitor with a Firecracker-compatible API.
//!
//! Apple-Silicon-only, HVF-only, aarch64 Linux guests. This binary parses the
//! Firecracker-compatible CLI flag set documented in [50-cli.md](../../specs/50-cli.md)
//! and delegates runtime ownership to the public `squib` facade crate.

#![allow(clippy::too_many_lines)]

use std::{path::Path, time::Duration};

use anyhow::{Context as _, Result};
use clap::Parser;
use squib::{DEFAULT_MMDS_SIZE_LIMIT, Squib};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod cli;

use cli::{Args, LogLevel};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(args.level);

    if args.snapshot_version {
        println!("{}", squib_snapshot::SNAPSHOT_VERSION);
        return Ok(());
    }
    if let Some(path) = args.describe_snapshot.as_deref() {
        let desc = squib_snapshot::describe(path)
            .map_err(|e| anyhow::anyhow!("describe-snapshot: {}", e.wire_message()))?;
        print!("{}", desc.human());
        if !desc.crc_ok {
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

    let net_mode = args.network.to_net_mode().map_err(|e| anyhow::anyhow!(e))?;
    if matches!(net_mode, squib_net::NetMode::Userspace) && args.gvproxy_path.is_none() {
        warn!(
            "--network=userspace selected without --gvproxy-path; falling back to \
             /usr/local/libexec/squib/gvproxy"
        );
    }
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

    let mut builder = Squib::builder()
        .try_instance_id(args.id.clone())
        .map_err(|e| anyhow::anyhow!(e))?
        .network_mode(net_mode)
        .start_microvm(args.no_api)
        .http_api_max_payload_size(args.http_api_max_payload_size as usize)
        .run_budget(Duration::from_mins(5))
        .mmds_size_limit(
            args.mmds_size_limit
                .map_or(Ok(DEFAULT_MMDS_SIZE_LIMIT), usize::try_from)
                .context("mmds size limit does not fit usize")?,
        );

    if let Some(path) = args.config_file.clone() {
        builder = builder.config_file(path);
    }
    if let Some(path) = args.gvproxy_path.clone() {
        builder = builder.gvproxy_path(path);
    }
    if let Some(iface) = args.bridged_iface.clone() {
        builder = builder.bridged_iface(iface);
    }
    if !args.no_api {
        let socket_path = args
            .api_sock
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--api-sock must be a path"))?;
        builder = builder.api_socket(socket_path);
    }

    info!(
        instance_id = %args.id,
        api_sock = %display_path(args.api_sock.as_deref()),
        config_file = %display_path(args.config_file.as_deref()),
        network = ?args.network,
        "squib runtime starting",
    );

    let mut vm = builder.spawn().await.map_err(|e| anyhow::anyhow!(e))?;

    if args.no_api {
        info!(
            instance_id = %args.id,
            "squib running without API socket (--no-api). The static config has been replayed.",
        );
        wait_for_termination_signal().await;
        info!("termination signal received; draining VMM and shutting down");
        vm.shutdown().await.map_err(|e| anyhow::anyhow!(e))?;
        return Ok(());
    }

    loop {
        if vm.api_server_finished() {
            vm.join_api_server().await.map_err(|e| anyhow::anyhow!(e))?;
            break;
        }
        tokio::select! {
            () = wait_for_termination_signal() => {
                info!("termination signal received; draining VMM and shutting down");
                vm.shutdown().await.map_err(|e| anyhow::anyhow!(e))?;
                break;
            }
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }

    Ok(())
}

/// Resolve on the first of SIGINT (Ctrl-C) or SIGTERM (parent-process kill signal).
async fn wait_for_termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
            tokio::signal::ctrl_c().await.ok();
            return;
        };
        let Ok(mut sigint) = signal(SignalKind::interrupt()) else {
            let _ = sigterm.recv().await;
            return;
        };
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
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

fn display_path(p: Option<&Path>) -> String {
    p.map_or_else(|| "<unset>".to_string(), |p| p.display().to_string())
}
