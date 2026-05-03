//! Axum router, the [`Runtime`] trait API handlers call into, and the [`serve`] entrypoint.
//!
//! The router shape and middleware mirror upstream Firecracker:
//! - Long-lived connections multiplexed on a single Unix domain socket.
//! - Every response carries `Server: Firecracker API`.
//! - Bodies above the configured payload limit return `413 Payload Too Large`.
//! - Unknown paths are translated by axum's fallback into our [`ApiError::NotFound`].

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    Router,
    extract::State,
    http::{HeaderName, HeaderValue, header},
    response::Json,
    routing::get,
    serve as axum_serve,
};
use tokio::net::UnixListener;
use tower_http::{limit::RequestBodyLimitLayer, set_header::SetResponseHeaderLayer};
use tracing::info;

use crate::{
    error::Result,
    schemas::{InstanceInfo, VersionResponse},
};

/// The literal value upstream Firecracker emits for the `Server` header. SDKs and
/// orchestrator-side smoke tests sniff this string; we emit it verbatim.
pub const FIRECRACKER_SERVER_HEADER: &str = "Firecracker API";

/// Default Firecracker-compat HTTP body limit (51200 bytes); overridable via [`ServeOptions`].
pub const DEFAULT_MAX_PAYLOAD: usize = 51_200;

/// Trait the API server calls into for state.
///
/// The VMM crate (`squib-vmm`) provides the production implementation. Tests and the
/// CLI's pre-VMM-wiring stub provide their own. The trait is intentionally tiny right
/// now — endpoints are added as they land.
pub trait Runtime: Send + Sync + 'static {
    /// Snapshot the current [`InstanceInfo`]. Called for every `GET /`.
    fn instance_info(&self) -> InstanceInfo;

    /// The Firecracker-compatible version string surfaced via `GET /version` and
    /// folded into [`InstanceInfo::vmm_version`].
    fn firecracker_version(&self) -> String;
}

/// Configuration for [`serve`].
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Path the Unix domain socket binds. The file is unlinked on drop.
    pub socket_path: PathBuf,
    /// Maximum HTTP request body, in bytes. Mirrors `--http-api-max-payload-size`.
    pub max_payload_size: usize,
}

impl ServeOptions {
    /// Build with the Firecracker-compatible default body limit.
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            max_payload_size: DEFAULT_MAX_PAYLOAD,
        }
    }

    /// Override the body limit; matches the `--http-api-max-payload-size` CLI flag.
    #[must_use]
    pub fn with_max_payload_size(mut self, bytes: usize) -> Self {
        self.max_payload_size = bytes;
        self
    }
}

/// Build the axum router with all middleware applied. Exposed for use in integration tests
/// (callers can wrap it in `axum::serve` against any [`tokio::net::UnixListener`]).
pub fn router<R: Runtime>(runtime: Arc<R>, max_payload: usize) -> Router {
    let server_header_value = HeaderValue::from_static(FIRECRACKER_SERVER_HEADER);
    let server_layer = SetResponseHeaderLayer::overriding(header::SERVER, server_header_value);

    Router::new()
        .route("/", get(get_root::<R>))
        .route("/version", get(get_version::<R>))
        .with_state(runtime)
        .layer(server_layer)
        .layer(RequestBodyLimitLayer::new(max_payload))
}

/// Bind a Unix domain socket and serve the API on it until the future is dropped.
///
/// Removes any stale socket file at `opts.socket_path` before binding (Firecracker does
/// the same — long-running VMM hosts often relaunch with the same path).
///
/// # Errors
/// Returns an error if the socket file cannot be unlinked, the bind fails, or the
/// underlying axum service errors.
pub async fn serve<R: Runtime>(opts: ServeOptions, runtime: Arc<R>) -> std::io::Result<()> {
    if opts.socket_path.exists() {
        tokio::fs::remove_file(&opts.socket_path).await?;
    }
    let listener = UnixListener::bind(&opts.socket_path)?;
    info!(
        socket = %opts.socket_path.display(),
        max_payload_size = opts.max_payload_size,
        "squib-api listening",
    );

    let app = router(runtime, opts.max_payload_size);
    let serve_future = axum_serve(listener, app);
    serve_future.await
}

async fn get_root<R: Runtime>(State(runtime): State<Arc<R>>) -> Result<Json<InstanceInfo>> {
    Ok(Json(runtime.instance_info()))
}

async fn get_version<R: Runtime>(State(runtime): State<Arc<R>>) -> Result<Json<VersionResponse>> {
    Ok(Json(VersionResponse {
        firecracker_version: runtime.firecracker_version(),
    }))
}

/// Best-effort cleanup helper: unlinks `path` if present, ignoring `NotFound`.
///
/// Useful for tests and graceful-shutdown paths to keep socket detritus out of `/tmp`.
pub async fn unlink_socket_if_exists(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

// Allow tracing to actually mention `header` import.
#[allow(dead_code)]
const _SERVER_HEADER_TYPE: HeaderName = header::SERVER;

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedRuntime;

    impl Runtime for FixedRuntime {
        fn instance_info(&self) -> InstanceInfo {
            InstanceInfo {
                id: "anonymous".into(),
                state: crate::schemas::InstanceState::NotStarted,
                vmm_version: "1.16.0 (squib 0.0.0-test)".into(),
                app_name: "Firecracker".into(),
            }
        }
        fn firecracker_version(&self) -> String {
            "1.16.0".into()
        }
    }

    #[test]
    fn router_builds() {
        let _ = router(Arc::new(FixedRuntime), DEFAULT_MAX_PAYLOAD);
    }

    #[test]
    fn serve_options_defaults_match_firecracker() {
        let opts = ServeOptions::new("/tmp/squib.sock");
        assert_eq!(opts.max_payload_size, 51_200);
    }

    #[test]
    fn serve_options_override_payload_limit() {
        let opts = ServeOptions::new("/tmp/squib.sock").with_max_payload_size(1024);
        assert_eq!(opts.max_payload_size, 1024);
    }
}
