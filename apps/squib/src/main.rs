//! `squib` — macOS-native microVM monitor with a Firecracker-compatible API.
//!
//! Apple-Silicon-only, HVF-only, aarch64 Linux guests. This binary parses the
//! Firecracker-compatible CLI flag set documented in `specs/squib-api-compat-design.md`
//! and starts the API server on a Unix domain socket. The HVF backend is not yet wired
//! in — Track A delivers it; this binary currently serves the API endpoints that don't
//! depend on a running VMM (`GET /`, `GET /version`).

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use squib_api::{
    InstanceInfo, VmState,
    server::{Runtime, ServeOptions, serve},
};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod cli;

use cli::{Args, LogLevel};

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

    let runtime = Arc::new(StubRuntime {
        instance_id: args.id.clone(),
    });

    if args.no_api {
        info!(
            instance_id = %args.id,
            "squib starting without API socket (--no-api); --config-file replay is not yet wired",
        );
        anyhow::bail!("--config-file replay is not yet implemented (Track B in progress)");
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

    serve(opts, runtime).await?;
    Ok(())
}

/// Pre-VMM stub runtime. Knows the instance ID and the Firecracker-compat version
/// string; reports state as `NotStarted` since no microvm has booted yet. Replaced
/// by the VMM crate's runtime implementation once Track A's HVF backend lands.
struct StubRuntime {
    instance_id: String,
}

impl Runtime for StubRuntime {
    fn instance_info(&self) -> InstanceInfo {
        InstanceInfo {
            id: self.instance_id.clone(),
            state: VmState::NotStarted,
            vmm_version: format!(
                "{FIRECRACKER_COMPAT_VERSION} (squib {})",
                env!("CARGO_PKG_VERSION")
            ),
            app_name: "Firecracker".into(),
        }
    }

    fn firecracker_version(&self) -> String {
        FIRECRACKER_COMPAT_VERSION.into()
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
