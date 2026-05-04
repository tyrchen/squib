//! Transcript types for declarative request/response replays.
//!
//! A transcript is the data shape that mirrors the per-row "send X, expect Y" rule
//! in [21-api-compat-matrix.md](../../../specs/21-api-compat-matrix.md). Tests build a
//! transcript and call [`replay`]; the harness boots a [`crate::CompatServer`], drives
//! every step in sequence, and asserts the response shape.

use std::path::Path;

use crate::{
    assertions::{assert_fault_message_contains, assert_firecracker_server_header},
    http::{HttpResponse, build_request, http_request},
};

/// Body-shape expectation for a single response.
#[derive(Debug, Clone)]
pub enum ExpectedResponse {
    /// Status only (any body OK). Useful for the upstream `204 No Content` happy path.
    Status(u16),
    /// Status + a substring the body must contain — primary shape for R rows.
    StatusAndBodyContains(u16, String),
    /// Status + a `fault_message` substring. Convenience over `StatusAndBodyContains`
    /// because it also asserts the body is well-formed JSON with a `fault_message`
    /// field.
    Fault(u16, String),
    /// Status + the body must parse to JSON satisfying the predicate.
    StatusAndJson(u16, fn(&serde_json::Value) -> bool),
}

/// One step of a transcript.
#[derive(Debug, Clone)]
pub struct Step {
    /// Short human-readable label printed on failure (`PUT /machine-config`).
    pub label: String,
    /// HTTP method.
    pub method: &'static str,
    /// Path. Constants are fine; format strings should be expanded by the test.
    pub path: String,
    /// Optional JSON body sent with `Content-Type: application/json`.
    pub body: Option<String>,
    /// What the response must look like.
    pub expect: ExpectedResponse,
}

impl Step {
    /// Build a step with a JSON body.
    pub fn put_json(
        label: impl Into<String>,
        path: impl Into<String>,
        body: impl Into<String>,
        expect: ExpectedResponse,
    ) -> Self {
        Self {
            label: label.into(),
            method: "PUT",
            path: path.into(),
            body: Some(body.into()),
            expect,
        }
    }

    /// Build a step with no body.
    pub fn get(
        label: impl Into<String>,
        path: impl Into<String>,
        expect: ExpectedResponse,
    ) -> Self {
        Self {
            label: label.into(),
            method: "GET",
            path: path.into(),
            body: None,
            expect,
        }
    }

    /// Build a PATCH step with a JSON body.
    pub fn patch_json(
        label: impl Into<String>,
        path: impl Into<String>,
        body: impl Into<String>,
        expect: ExpectedResponse,
    ) -> Self {
        Self {
            label: label.into(),
            method: "PATCH",
            path: path.into(),
            body: Some(body.into()),
            expect,
        }
    }

    /// Build a DELETE step with no body.
    pub fn delete(
        label: impl Into<String>,
        path: impl Into<String>,
        expect: ExpectedResponse,
    ) -> Self {
        Self {
            label: label.into(),
            method: "DELETE",
            path: path.into(),
            body: None,
            expect,
        }
    }
}

/// A named sequence of steps.
#[derive(Debug, Clone)]
pub struct Transcript {
    /// Test-friendly identifier (e.g. `getting-started`).
    pub name: String,
    /// Ordered steps. Replay aborts on the first mismatch.
    pub steps: Vec<Step>,
}

impl Transcript {
    /// Build a transcript with a name + steps.
    pub fn new(name: impl Into<String>, steps: Vec<Step>) -> Self {
        Self {
            name: name.into(),
            steps,
        }
    }
}

/// Drive every step against the live socket. Asserts each response matches the
/// declared expectation; on any mismatch panics with the failing step's label so the
/// transcript reads top-to-bottom in test output.
pub async fn replay(socket: &Path, transcript: &Transcript) {
    for (idx, step) in transcript.steps.iter().enumerate() {
        let raw = build_request(step.method, &step.path, step.body.as_deref());
        let response = http_request(socket, &raw).await;
        match_step(&transcript.name, idx, step, &response);
    }
}

fn match_step(name: &str, idx: usize, step: &Step, response: &HttpResponse) {
    let prefix = format!("[{name} step {idx}: {} {}]", step.method, step.label);
    // Every successful or failed response must carry the upstream Server header.
    assert_firecracker_server_header(response, &prefix);

    match &step.expect {
        ExpectedResponse::Status(code) => {
            assert_eq!(
                response.status,
                *code,
                "{prefix} expected status {code}, got {} body={:?}",
                response.status,
                response.body_str()
            );
        }
        ExpectedResponse::StatusAndBodyContains(code, needle) => {
            assert_eq!(
                response.status,
                *code,
                "{prefix} expected status {code}, got {} body={:?}",
                response.status,
                response.body_str()
            );
            let body = response.body_str().unwrap_or("");
            assert!(
                body.contains(needle.as_str()),
                "{prefix} body did not contain {needle:?}; body={body:?}",
            );
        }
        ExpectedResponse::Fault(code, needle) => {
            assert_eq!(
                response.status,
                *code,
                "{prefix} expected status {code}, got {} body={:?}",
                response.status,
                response.body_str()
            );
            assert_fault_message_contains(response, needle, &prefix);
        }
        ExpectedResponse::StatusAndJson(code, predicate) => {
            assert_eq!(
                response.status,
                *code,
                "{prefix} expected status {code}, got {} body={:?}",
                response.status,
                response.body_str()
            );
            let json = response
                .body_json()
                .unwrap_or_else(|e| panic!("{prefix} body is not JSON: {e}"));
            assert!(
                predicate(&json),
                "{prefix} body predicate failed; body={json}",
            );
        }
    }
}
