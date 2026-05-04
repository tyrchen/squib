//! F-row replay of the upstream `getting-started.md` curl sequence.
//!
//! Per [91-impl-plan.md § 5 Phase
//! 2.5](../../../specs/91-impl-plan.md#5-phase-2-api-server-and-json-config) and
//! [21-api-compat-matrix.md § 1](../../../specs/21-api-compat-matrix.md#1-http-api-endpoints)
//! the `getting-started.md` sequence is the canonical happy-path: configure the
//! microVM via PUT/PATCH, then `PUT /actions { action_type: InstanceStart }`.
//! Each step's expected response code matches what upstream Firecracker emits;
//! `InstanceStart` returns 400 against the stub VMM today (Phase 1's vCPU run-loop
//! tail is the gating dependency tracked in `93-improvements-review.md`).
//!
//! What this test verifies, even with the live VMM stubbed:
//!
//! - **Wire-shape parity** for every endpoint in the getting-started flow.
//! - The order is independent — `PUT /machine-config` before `PUT /boot-source` is a squib replay
//!   convention; the API server itself does not enforce ordering.
//! - The `Server: Firecracker API` header on every response (sniffed verbatim by `firectl` and
//!   `firecracker-go-sdk`).

use firecracker_compat::{
    CompatServer, ExpectedResponse, Step, Transcript,
    http::{build_request, http_request},
    replay,
};

#[tokio::test]
async fn test_should_replay_getting_started_against_stub_vmm() {
    let server = CompatServer::spawn().await;

    let steps = vec![
        Step::put_json(
            "/boot-source",
            "/boot-source",
            r#"{
                "kernel_image_path": "/tmp/squib-compat-kernel",
                "boot_args": "console=ttyS0 reboot=k panic=1 pci=off"
            }"#,
            ExpectedResponse::Status(204),
        ),
        Step::put_json(
            "/machine-config",
            "/machine-config",
            r#"{ "vcpu_count": 1, "mem_size_mib": 256 }"#,
            ExpectedResponse::Status(204),
        ),
        Step::put_json(
            "/drives/rootfs",
            "/drives/rootfs",
            r#"{
                "drive_id": "rootfs",
                "path_on_host": "/tmp/squib-compat-rootfs",
                "is_root_device": true,
                "is_read_only": false
            }"#,
            ExpectedResponse::Status(204),
        ),
        Step::put_json(
            "/network-interfaces/eth0",
            "/network-interfaces/eth0",
            r#"{ "iface_id": "eth0", "host_dev_name": "tap0" }"#,
            ExpectedResponse::Status(204),
        ),
        Step::get(
            "/",
            "/",
            ExpectedResponse::StatusAndJson(200, |v| {
                v.get("state").and_then(|s| s.as_str()) == Some("Not started")
                    && v.get("app_name").and_then(|s| s.as_str()) == Some("Firecracker")
            }),
        ),
        Step::get(
            "/version",
            "/version",
            ExpectedResponse::StatusAndJson(200, |v| {
                v.get("firecracker_version")
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| s.starts_with("1.16"))
            }),
        ),
        // `PUT /actions {InstanceStart}` returns 400 with the documented stub
        // `fault_message` against the Phase-2 stub VMM — see
        // 93-improvements-review.md, "Phase 1 (lands at end of Phase 1.6) — Boot-to-
        // busybox smoke test (Phase 1 exit-criteria gap)".
        Step::put_json(
            "/actions {InstanceStart}",
            "/actions",
            r#"{ "action_type": "InstanceStart" }"#,
            ExpectedResponse::Fault(400, "VMM not yet wired".into()),
        ),
    ];

    let transcript = Transcript::new("getting-started", steps);
    replay(server.socket(), &transcript).await;
    server.shutdown().await;
}

/// The minimum upstream-shaped probe sequence: `GET /` and `GET /version` should
/// return parseable JSON with the upstream wire shape on a freshly-started server,
/// pre-boot. SDK version sniffers do this on connect; the harness asserts the same
/// shape pre-boot and post-boot.
#[tokio::test]
async fn test_should_serve_get_root_and_version_pre_boot() {
    let server = CompatServer::spawn().await;
    let socket = server.socket();

    // `GET /` returns 200 with InstanceInfo.
    let resp = http_request(socket, &build_request("GET", "/", None)).await;
    assert_eq!(resp.status, 200);
    let json = resp.body_json().expect("instance info json");
    assert_eq!(json["state"], "Not started");
    assert_eq!(json["app_name"], "Firecracker");
    assert!(json["vmm_version"].as_str().unwrap().contains("1.16"));

    // `GET /version` returns 200 with `firecracker_version`.
    let resp = http_request(socket, &build_request("GET", "/version", None)).await;
    assert_eq!(resp.status, 200);
    let json = resp.body_json().expect("version json");
    assert_eq!(json["firecracker_version"], "1.16.0");

    server.shutdown().await;
}
