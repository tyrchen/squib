//! P rows — parity in shape, semantics differ in a documented way.
//!
//! Per [21-api-compat-matrix.md §
//! 2](../../../specs/21-api-compat-matrix.md#2-field-level-compatibility), the load-bearing P rows
//! are:
//!
//! - `network-interfaces.host_dev_name` — accepted but mapped to a vmnet handle
//!   `squib-tap-<iface_id>`. Wire-shape parity (200 / 204) is what this test asserts; the vmnet
//!   mapping is host-side and exercised by `make vmnet-test`.
//! - `cpu-config` aarch64 best-effort vs x86 fields accept-and-warn (covered in `a_rows.rs` since
//!   the x86 fields warn-not-error).

use firecracker_compat::{
    CompatServer,
    http::{build_request, http_request},
};

/// P: `host_dev_name` accepts any literal-looking TAP name and the request
/// succeeds. Squib does not honour Linux TAP semantics; it maps to a vmnet handle
/// internally. The wire contract is that the request returns 204 with the
/// upstream-shaped header.
#[tokio::test]
async fn test_p_should_accept_host_dev_name_with_linux_style_tap_value() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "iface_id": "eth0", "host_dev_name": "tap0" }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/network-interfaces/eth0", Some(body)),
    )
    .await;
    assert_eq!(
        resp.status,
        204,
        "host_dev_name should accept Linux-style names; got body={:?}",
        resp.body_str()
    );
    server.shutdown().await;
}

/// P: `host_dev_name` containing a vmnet-flavoured prefix is also accepted (the
/// validation runs before the vmnet host-side mapping; it is opaque to the schema
/// layer).
#[tokio::test]
async fn test_p_should_accept_host_dev_name_with_squib_tap_prefix() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "iface_id": "eth0", "host_dev_name": "squib-tap-eth0" }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/network-interfaces/eth0", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

/// P: `cpu-config` accepts an aarch64-shaped body (best-effort applied per the
/// matrix). x86 fields (`cpuid_modifiers` / `msr_modifiers` / `kvm_capabilities`)
/// live in `a_rows.rs` as accept-and-warn rows.
#[tokio::test]
async fn test_p_should_accept_cpu_config_aarch64_reg_modifiers() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "reg_modifiers": [{"addr": "0x603000000013c020", "bitmap": "0b1xx0"}] }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/cpu-config", Some(body)),
    )
    .await;
    // CPU-config is permissive — any well-formed JSON accepted; we assert wire
    // shape, not semantics.
    assert!(
        matches!(resp.status, 200 | 204),
        "cpu-config should accept aarch64 reg_modifiers; got {} body={:?}",
        resp.status,
        resp.body_str()
    );
    server.shutdown().await;
}
