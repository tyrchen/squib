//! Cross-cutting wire-shape invariants per [21-api-compat-matrix.md §
//! 9](../../../specs/21-api-compat-matrix.md#9-error-response-shape).
//!
//! Every successful or failed response must carry `Server: Firecracker API`. Every
//! 4xx/5xx response body must be a JSON object with the single field
//! `fault_message`. SDK code sniffs both byte-for-byte; tests assert them centrally so
//! a regression in either is caught by every transcript replay.

use crate::http::HttpResponse;

/// Assert the response carries the upstream `Server: Firecracker API` header. Panics
/// with the supplied prefix on failure to make multi-step transcript output
/// readable.
pub fn assert_firecracker_server_header(response: &HttpResponse, prefix: &str) {
    let server = response.header("server");
    assert_eq!(
        server,
        Some("Firecracker API"),
        "{prefix} missing or wrong `Server` header (expected 'Firecracker API', got {server:?})",
    );
}

/// Assert the response body is `{"fault_message": "..."}` and that the message
/// contains `needle`. Used for R-row tests.
pub fn assert_fault_message_contains(response: &HttpResponse, needle: &str, prefix: &str) {
    let json = response.body_json().unwrap_or_else(|e| {
        panic!(
            "{prefix} body is not JSON: {e}; body={:?}",
            response.body_str()
        )
    });
    let obj = json
        .as_object()
        .unwrap_or_else(|| panic!("{prefix} body is not a JSON object; body={json}"));
    let msg = obj
        .get("fault_message")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("{prefix} body has no `fault_message` field; body={json}"));
    assert!(
        msg.contains(needle),
        "{prefix} fault_message {msg:?} does not contain {needle:?}",
    );
    // Wire-shape invariant: the only top-level field is `fault_message`.
    assert_eq!(
        obj.len(),
        1,
        "{prefix} fault body has unexpected extra keys: {:?}",
        obj.keys().collect::<Vec<_>>()
    );
}
