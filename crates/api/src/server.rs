//! Axum router on a Unix domain socket; the [`serve`] entrypoint.
//!
//! Per [20-firecracker-api.md § 2](../../../specs/20-firecracker-api.md#2-server-shape):
//!
//! - Every response carries `Server: Firecracker API` (a `SetResponseHeaderLayer`).
//! - Bodies above `--http-api-max-payload-size` return `413 Payload Too Large` via
//!   `tower_http::limit::RequestBodyLimitLayer`.
//! - Unknown paths are translated by the fallback into `400 BadRequest` with `{"fault_message": "No
//!   such resource: ..."}`.
//!
//! The handler set lives in [`crate::handlers`]; this module is just plumbing.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    Router,
    extract::{MatchedPath, Request},
    http::{HeaderValue, Response as HttpResponse, header},
    routing::{get, patch, put},
    serve as axum_serve,
};
use tokio::net::UnixListener;
use tower_http::{
    limit::RequestBodyLimitLayer, set_header::SetResponseHeaderLayer, trace::TraceLayer,
};
use tracing::{Span, info, info_span};

use crate::{
    controller::RuntimeApiController,
    handlers::{
        delete_drive, delete_network, delete_pmem, fallback, get_balloon, get_balloon_statistics,
        get_hotplug_memory, get_machine_config, get_mmds, get_root, get_version, get_vm_config,
        patch_balloon, patch_balloon_hinting, patch_balloon_statistics, patch_drive,
        patch_hotplug_memory, patch_machine_config, patch_mmds, patch_network, patch_pmem,
        patch_vm, put_actions, put_balloon, put_boot_source, put_cpu_config, put_drive,
        put_entropy, put_hotplug_memory, put_logger, put_machine_config, put_metrics, put_mmds,
        put_mmds_config, put_network, put_pmem, put_serial, put_snapshot_create, put_snapshot_load,
        put_vsock,
    },
};

/// The literal value upstream Firecracker emits for the `Server` header. SDKs and
/// orchestrator-side smoke tests sniff this string; we emit it verbatim.
pub const FIRECRACKER_SERVER_HEADER: &str = "Firecracker API";

/// Default Firecracker-compat HTTP body limit (51200 bytes); overridable via [`ServeOptions`].
pub const DEFAULT_MAX_PAYLOAD: usize = 51_200;

/// Lower bound on the body limit ([70-security.md §
/// 6](../../../specs/70-security.md#6-resource-limits)).
pub const MIN_MAX_PAYLOAD: usize = 1_024;

/// Upper bound on the body limit.
pub const MAX_MAX_PAYLOAD: usize = 1_048_576;

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
    /// The value is clamped into `[MIN_MAX_PAYLOAD, MAX_MAX_PAYLOAD]`.
    #[must_use]
    pub fn with_max_payload_size(mut self, bytes: usize) -> Self {
        self.max_payload_size = bytes.clamp(MIN_MAX_PAYLOAD, MAX_MAX_PAYLOAD);
        self
    }
}

/// Build the axum router with all middleware applied. Exposed for use in integration
/// tests (callers can wrap it in `axum::serve` against any [`tokio::net::UnixListener`]).
///
/// Middleware order (innermost first): handler → response-header `Server` →
/// request-body cap → tracing span. Per [20-firecracker-api.md §
/// 2](../../../specs/20-firecracker-api.md#2-server-shape), each request gets an
/// `info`-level `tracing` span carrying `instance_id`, `method`, and the matched path
/// pattern (not the raw URI — pattern names avoid PII like a `drive_id` in `path`).
pub fn router(controller: Arc<RuntimeApiController>, max_payload: usize) -> Router {
    let server_header_value = HeaderValue::from_static(FIRECRACKER_SERVER_HEADER);
    let server_layer = SetResponseHeaderLayer::overriding(header::SERVER, server_header_value);
    let instance_id = controller.snapshot().instance_info.id.clone();
    let trace_layer = TraceLayer::new_for_http()
        .make_span_with(move |req: &Request<_>| {
            // Use the matched route pattern instead of `req.uri().path()` so identifiers
            // baked into the URL (drive_id, iface_id, …) don't leak into log streams.
            let matched = req
                .extensions()
                .get::<MatchedPath>()
                .map_or("<unmatched>", MatchedPath::as_str);
            info_span!(
                "squib_api_request",
                instance_id = %instance_id,
                method = %req.method(),
                path = %matched,
            )
        })
        .on_request(())
        .on_response(|_resp: &HttpResponse<_>, _latency: std::time::Duration, _span: &Span| {});

    Router::new()
        // Read-only fast path
        .route("/", get(get_root))
        .route("/version", get(get_version))
        .route("/vm/config", get(get_vm_config))
        // Mutating
        .route("/vm", patch(patch_vm))
        .route(
            "/machine-config",
            get(get_machine_config)
                .put(put_machine_config)
                .patch(patch_machine_config),
        )
        .route("/boot-source", put(put_boot_source))
        .route(
            "/drives/{id}",
            put(put_drive).patch(patch_drive).delete(delete_drive),
        )
        .route(
            "/network-interfaces/{id}",
            put(put_network).patch(patch_network).delete(delete_network),
        )
        .route("/vsock", put(put_vsock))
        .route("/mmds", get(get_mmds).put(put_mmds).patch(patch_mmds))
        .route("/mmds/config", put(put_mmds_config))
        .route(
            "/balloon",
            get(get_balloon).put(put_balloon).patch(patch_balloon),
        )
        .route(
            "/balloon/statistics",
            get(get_balloon_statistics).patch(patch_balloon_statistics),
        )
        .route("/balloon/hinting/{op}", patch(patch_balloon_hinting))
        .route("/entropy", put(put_entropy))
        .route("/serial", put(put_serial))
        .route(
            "/pmem/{id}",
            put(put_pmem).patch(patch_pmem).delete(delete_pmem),
        )
        .route(
            "/hotplug/memory",
            get(get_hotplug_memory)
                .put(put_hotplug_memory)
                .patch(patch_hotplug_memory),
        )
        .route("/cpu-config", put(put_cpu_config))
        .route("/actions", put(put_actions))
        .route("/snapshot/create", put(put_snapshot_create))
        .route("/snapshot/load", put(put_snapshot_load))
        .route("/logger", put(put_logger))
        .route("/metrics", put(put_metrics))
        .fallback(fallback)
        .with_state(controller)
        .layer(server_layer)
        .layer(RequestBodyLimitLayer::new(max_payload))
        .layer(trace_layer)
}

/// Bind a Unix domain socket for [`serve_bound`].
///
/// Removes any stale socket file at `opts.socket_path` before binding (Firecracker
/// does the same — long-running VMM hosts often relaunch with the same path).
///
/// # Errors
/// Returns an error if the socket file cannot be unlinked or the bind fails.
pub async fn bind_listener(opts: &ServeOptions) -> std::io::Result<UnixListener> {
    if opts.socket_path.exists() {
        tokio::fs::remove_file(&opts.socket_path).await?;
    }
    UnixListener::bind(&opts.socket_path)
}

/// Serve the API on an already-bound Unix listener until the future is dropped.
///
/// # Errors
/// Returns an error if the underlying axum service errors.
pub async fn serve_bound(
    listener: UnixListener,
    opts: ServeOptions,
    controller: Arc<RuntimeApiController>,
) -> std::io::Result<()> {
    info!(
        socket = %opts.socket_path.display(),
        max_payload_size = opts.max_payload_size,
        "squib-api listening",
    );

    let app = router(controller, opts.max_payload_size);
    axum_serve(listener, app).await
}

/// Bind a Unix domain socket and serve the API on it until the future is dropped.
///
/// # Errors
/// Returns an error if the socket file cannot be unlinked, the bind fails, or the
/// underlying axum service errors.
pub async fn serve(
    opts: ServeOptions,
    controller: Arc<RuntimeApiController>,
) -> std::io::Result<()> {
    let listener = bind_listener(&opts).await?;
    serve_bound(listener, opts, controller).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{ControllerSnapshot, TimeoutTable};

    fn ctl() -> Arc<RuntimeApiController> {
        let snap = ControllerSnapshot::new("anonymous", "1.16.0", "1.16.0 (squib 0.0.0-test)");
        let (c, _rx) = RuntimeApiController::new(snap, TimeoutTable::from_spec(), 16);
        Arc::new(c)
    }

    #[test]
    fn test_should_build_router_against_controller() {
        let _ = router(ctl(), DEFAULT_MAX_PAYLOAD);
    }

    #[test]
    fn test_should_default_payload_limit_to_51200() {
        let opts = ServeOptions::new("/tmp/squib.sock");
        assert_eq!(opts.max_payload_size, DEFAULT_MAX_PAYLOAD);
    }

    #[test]
    fn test_should_clamp_payload_limit_to_lower_bound() {
        let opts = ServeOptions::new("/tmp/squib.sock").with_max_payload_size(0);
        assert_eq!(opts.max_payload_size, MIN_MAX_PAYLOAD);
    }

    #[test]
    fn test_should_clamp_payload_limit_to_upper_bound() {
        let opts = ServeOptions::new("/tmp/squib.sock").with_max_payload_size(usize::MAX);
        assert_eq!(opts.max_payload_size, MAX_MAX_PAYLOAD);
    }
}
