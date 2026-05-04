//! R rows from [21-api-compat-matrix.md §
//! 9](../../../specs/21-api-compat-matrix.md#9-error-response-shape) and § 1 actions table.
//!
//! Each test asserts the exact `fault_message` substring squib's documented R rows
//! emit. SDKs and the upstream compat suite sniff these substrings; a rename is a
//! breaking change for orchestrators.

use firecracker_compat::{
    CompatServer,
    http::{build_request, http_request},
};
use squib_core::LifecyclePhase;

/// R: `smt: true` rejected on aarch64. Per
/// [21-api-compat-matrix.md § 2
/// `/machine-config`](../../../specs/21-api-compat-matrix.md#machine-config) the documented
/// `fault_message` mentions "SMT not supported on Apple Silicon".
#[tokio::test]
async fn test_r_should_reject_smt_true_with_apple_silicon_message() {
    let server = CompatServer::spawn().await;
    let body = r#"{"vcpu_count":1,"mem_size_mib":256,"smt":true}"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/machine-config", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 400);
    let body = resp.body_str().unwrap_or("");
    assert!(
        body.contains("SMT not supported on Apple Silicon"),
        "expected R-row fault_message; got {body:?}"
    );
    server.shutdown().await;
}

/// R: `SendCtrlAltDel` is x86-only and rejected. Per
/// [21-api-compat-matrix.md § 2 `/actions`](../../../specs/21-api-compat-matrix.md#actions-put).
#[tokio::test]
async fn test_r_should_reject_send_ctrl_alt_del_with_aarch64_message() {
    let server = CompatServer::spawn().await;
    let body = r#"{"action_type":"SendCtrlAltDel"}"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/actions", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 400);
    let body = resp.body_str().unwrap_or("");
    assert!(
        body.contains("SendCtrlAltDel is x86-only"),
        "expected R-row fault_message; got {body:?}"
    );
    server.shutdown().await;
}

/// R: unknown route maps to 400 with the `No such resource:` prefix that
/// [21-api-compat-matrix.md § 9](../../../specs/21-api-compat-matrix.md#9-error-response-shape)
/// promises. Upstream collapses all unknown URIs to 400 (no 404).
#[tokio::test]
async fn test_r_should_collapse_unknown_path_to_400_no_such_resource() {
    let server = CompatServer::spawn().await;
    let resp = http_request(
        server.socket(),
        &build_request("GET", "/no-such-thing", None),
    )
    .await;
    assert_eq!(resp.status, 400);
    let body = resp.body_str().unwrap_or("");
    assert!(
        body.contains("No such resource"),
        "expected upstream collapse-to-400 wording; got {body:?}",
    );
    assert!(body.contains("/no-such-thing"));
    server.shutdown().await;
}

/// R: PATCH on an endpoint with no body (legitimate input, but bad pre-boot/post-boot
/// admissibility) returns 400. The `RuntimeApiController` enforces phase rules.
#[tokio::test]
async fn test_r_should_reject_patch_vm_pre_boot_with_phase_message() {
    let server = CompatServer::spawn_with(
        LifecyclePhase::Uninitialized,
        firecracker_compat::StubBehaviour::Production,
    )
    .await;
    let body = r#"{"state":"Paused"}"#;
    let resp = http_request(server.socket(), &build_request("PATCH", "/vm", Some(body))).await;
    assert_eq!(resp.status, 400);
    let body = resp.body_str().unwrap_or("");
    assert!(
        body.contains("not allowed before the microVM has booted"),
        "expected admissibility fault_message; got {body:?}",
    );
    server.shutdown().await;
}

/// R: invalid `balloon_hinting` op returns 400. The op set is `start | status | stop`
/// — anything else fails URL-segment parsing with the offending op echoed back.
#[tokio::test]
async fn test_r_should_reject_unknown_balloon_hinting_op() {
    let server = CompatServer::spawn_with(
        LifecyclePhase::Running,
        firecracker_compat::StubBehaviour::Production,
    )
    .await;
    let resp = http_request(
        server.socket(),
        &build_request("PATCH", "/balloon/hinting/frobnicate", None),
    )
    .await;
    assert_eq!(resp.status, 400);
    let body = resp.body_str().unwrap_or("");
    assert!(body.contains("frobnicate"));
    server.shutdown().await;
}

/// R: oversize body returns 413. The default cap is 51200 bytes; we send 64 KiB to
/// trip `tower_http::limit::RequestBodyLimitLayer`.
#[tokio::test]
async fn test_r_should_413_oversized_request_body() {
    let server = CompatServer::spawn().await;
    let pad = "x".repeat(60_000);
    let body = format!(r#"{{"kernel_image_path":"/tmp/k","boot_args":"{pad}"}}"#);
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/boot-source", Some(&body)),
    )
    .await;
    assert_eq!(resp.status, 413);
    server.shutdown().await;
}
