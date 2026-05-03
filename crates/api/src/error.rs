//! Error type and the wire-shape Firecracker uses for failed requests.

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
/// Status code mapping is identical to upstream Firecracker's: malformed JSON, missing
/// required fields, invalid enum values, illegal state transitions all map to **400 Bad
/// Request**; oversized MMDS bodies map to **413 Payload Too Large**. Successful PUT/PATCH
/// requests do not produce an `ApiError` — they return a `204 No Content` directly.
#[derive(Debug, Error)]
pub enum ApiError {
    /// Generic 400 error with a custom fault message.
    #[error("{0}")]
    BadRequest(String),

    /// 413 — used by MMDS endpoints when the data store would exceed the configured
    /// `--mmds-size-limit`.
    #[error("{0}")]
    PayloadTooLarge(String),

    /// 404 — the path does not match any registered route. Upstream Firecracker
    /// returns a 400 here, but axum has its own 404 fallback that we adapt to match.
    #[error("Resource not found: {0}")]
    NotFound(String),
}

impl ApiError {
    /// Status code this error variant maps to.
    pub fn status(&self) -> StatusCode {
        match self {
            // Firecracker returns 400 for unknown paths too — we collapse `BadRequest`
            // and `NotFound` to the same code intentionally.
            Self::BadRequest(_) | Self::NotFound(_) => StatusCode::BAD_REQUEST,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = FaultMessage::new(self.to_string());
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
}
