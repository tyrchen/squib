//! Firecracker-compatible HTTP API server for squib.
//!
//! `squib-api` exposes the same `OpenAPI` surface that upstream Firecracker speaks over
//! a Unix domain socket: same paths, same JSON shapes, same status codes, same
//! `{"fault_message": "..."}` error body, same `Server: Firecracker API` response
//! header. Per-endpoint compatibility lives in
//! [21-api-compat-matrix.md](../../specs/21-api-compat-matrix.md); the machinery is
//! pinned in [20-firecracker-api.md](../../specs/20-firecracker-api.md).
//!
//! # Architecture
//!
//! - [`schemas`] — `Raw*` request/response shapes plus validated newtypes whose only construction
//!   path is via `TryFrom`. Per [10-data-model.md §
//!   2.3](../../specs/10-data-model.md#23-schema-layer), validation is the constructor;
//!   "unvalidated `DriveConfig`" is unrepresentable.
//! - [`action`] — the `ApiAction` / `ApiResponse` enum the API thread posts onto the bounded mpsc
//!   channel into the VMM event loop.
//! - [`controller`] — the `RuntimeApiController` with `ArcSwap` read-mirror, lock-free read fast
//!   path, and per-action-class `tokio::time::timeout` (D26).
//! - [`error`] — the wire-shape `FaultMessage`, `ApiError` (incl. squib-only 504), and the
//!   `IntoResponse` impl that translates everything into the upstream wire shape.
//! - [`handlers`] — axum endpoint handlers for every Firecracker route.
//! - [`server`] — axum router on `tokio::net::UnixListener` plus the `serve` entry.
//! - [`replay`] — `--config-file` static-config replay path; deterministic order, identical
//!   validation/dispatch as the HTTP layer.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod action;
pub mod controller;
pub mod error;
pub mod handlers;
pub mod replay;
pub mod schemas;
pub mod server;

pub use action::{ActionClass, ApiAction, ApiResponse};
pub use controller::{
    ActionReceiver, ActionSender, ControllerSnapshot, RuntimeApiController, TimeoutTable,
};
pub use error::{ApiError, FaultMessage, Result};
pub use replay::{ReplayError, parse_config_file, replay_config};
pub use schemas::{InstanceInfo, VersionResponse, VmState};
pub use server::{
    DEFAULT_MAX_PAYLOAD, FIRECRACKER_SERVER_HEADER, MAX_MAX_PAYLOAD, MIN_MAX_PAYLOAD, ServeOptions,
    router, serve, unlink_socket_if_exists,
};
