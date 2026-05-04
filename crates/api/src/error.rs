//! Error type and the wire-shape Firecracker uses for failed requests.
//!
//! Status code mapping (per [20-firecracker-api.md §
//! 3](../../../specs/20-firecracker-api.md#3-error-envelope)):
//!
//! - **400 Bad Request** — malformed JSON, missing required fields, invalid enum values, illegal
//!   state transitions, unknown paths (Firecracker collapses 404 to 400 for the wire compat-suite
//!   shape), validation failures.
//! - **413 Payload Too Large** — bodies above `--http-api-max-payload-size`, oversized MMDS data
//!   store mutations.
//! - **500 Internal Server Error** — VMM event loop is gone; rare and logged at `error`.
//! - **504 Gateway Timeout** — squib-only; emitted when a per-action-class timeout ([70-security.md
//!   § 6](../../../specs/70-security.md#6-resource-limits)) fires while the controller awaits the
//!   VMM. Upstream Firecracker has no 504; orchestrators should retry with the same idempotency
//!   key. Documented in `docs/api-deviations.md`.
//!
//! Successful PUT/PATCH/DELETE produce `204 No Content` directly without an `ApiError`.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Result alias used throughout `squib-api`.
pub type Result<T, E = ApiError> = core::result::Result<T, E>;

/// The exact JSON body upstream Firecracker emits on every failed API call.
///
/// Wire shape:
/// ```json
/// {"fault_message": "Block device with ID 'rootfs' already exists"}
/// ```
///
/// No additional fields, ever — sniffed verbatim by SDKs and `firectl`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct FaultMessage {
    /// Human-readable description of what went wrong. Squib makes a best-effort
    /// attempt to mirror upstream's phrasing for known failure modes.
    pub fault_message: String,
}

impl FaultMessage {
    /// Construct a [`FaultMessage`] from a string-like value.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            fault_message: reason.into(),
        }
    }
}

/// Errors produced by API handlers, each carrying the HTTP status code Firecracker
/// returns for that class of failure.
///
/// Status codes match upstream where they exist (200 / 204 / 400 / 413 / 500). 504 is
/// squib-only and surfaces the per-action-class timeout taxonomy from [70-security.md
/// § 6](../../../specs/70-security.md#6-resource-limits).
#[derive(Debug, Error)]
pub enum ApiError {
    /// Generic 400 error with a custom fault message — the bulk of validation /
    /// state-machine / parse failures land here.
    #[error("{0}")]
    BadRequest(String),

    /// 413 Payload Too Large — used by the body-size limit middleware and by MMDS
    /// endpoints when the data store would exceed `--mmds-size-limit`.
    #[error("{0}")]
    PayloadTooLarge(String),

    /// 404 Not Found — the path does not match any registered route. We translate this
    /// to a 400 in the response (axum's default 404 is overridden); Firecracker has no
    /// 404 in its surface.
    #[error("Resource not found: {0}")]
    NotFound(String),

    /// 504 Gateway Timeout — squib-only. Emitted when a per-action-class timeout fires
    /// while the controller awaits the VMM. The action remains pending at the VMM
    /// (cancelling it would leave undefined state); the controller logs at `error`.
    /// `&'static str` because the action class name is always known at the call site.
    #[error("VMM action timed out: {0}")]
    Timeout(&'static str),

    /// 500 Internal Server Error — used when the VMM event loop has shut down and a
    /// handler can no longer dispatch. Rare; logged at `error`.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    /// Status code this error variant maps to.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) | Self::NotFound(_) => StatusCode::BAD_REQUEST,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The exact `fault_message` body string this error surfaces.
    pub fn fault_message(&self) -> String {
        match self {
            Self::BadRequest(s) | Self::PayloadTooLarge(s) | Self::Internal(s) => s.clone(),
            Self::NotFound(path) => format!("No such resource: {path}"),
            Self::Timeout(class) => format!("VMM action timed out: {class}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = FaultMessage::new(self.fault_message());
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_message_serializes_with_correct_field_name() {
        let msg = FaultMessage::new("oops");
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"fault_message":"oops"}"#);
    }

    #[test]
    fn fault_message_round_trips_through_serde() {
        let original = FaultMessage::new("kernel image not found");
        let json = serde_json::to_string(&original).unwrap();
        let back: FaultMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn bad_request_maps_to_400() {
        let err = ApiError::BadRequest("bad".into());
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn payload_too_large_maps_to_413() {
        let err = ApiError::PayloadTooLarge("too big".into());
        assert_eq!(err.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn not_found_maps_to_400_for_firecracker_parity() {
        let err = ApiError::NotFound("/missing".into());
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn timeout_maps_to_504() {
        let err = ApiError::Timeout("PUT /actions {InstanceStart}");
        assert_eq!(err.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(err.fault_message().contains("InstanceStart"));
    }

    #[test]
    fn internal_maps_to_500() {
        let err = ApiError::Internal("event loop gone".into());
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn not_found_fault_message_contains_path() {
        let err = ApiError::NotFound("/no-such".into());
        assert_eq!(err.fault_message(), "No such resource: /no-such");
    }
}
