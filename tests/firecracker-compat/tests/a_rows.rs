//! A rows — accept-and-warn fields.
//!
//! Per [21-api-compat-matrix.md](../../../specs/21-api-compat-matrix.md), several
//! upstream fields are macOS-irrelevant but accepted for compat. Squib parses them
//! without error so SDK orchestrators can supply identical configs.
//!
//! - `/drives/{id}.socket` — vhost-user socket (Linux-only); accept-and-warn.
//! - `/machine-config.huge_pages` — `2M` warns once on macOS (page-size managed by the kernel).
//! - `/snapshot/load.clock_realtime` — x86-only kvmclock setting; ignored.
//! - `/cpu-config` x86 fields — `cpuid_modifiers`, `msr_modifiers`, `kvm_capabilities`
//!   accept-and-warn.
//! - `--seccomp-filter` / `--no-seccomp` CLI flags — covered by `apps/squib/tests`.
//! - `--enable-pci` CLI flag — covered by `apps/squib/tests`.

use firecracker_compat::{
    CompatServer,
    http::{build_request, http_request},
};

/// A: `/drives/{id}.socket` (vhost-user) is accepted with no error.
#[tokio::test]
async fn test_a_should_accept_vhost_user_socket_field() {
    let server = CompatServer::spawn().await;
    let body = r#"{
        "drive_id": "rootfs",
        "path_on_host": "/tmp/rootfs.img",
        "is_root_device": true,
        "socket": "/tmp/vhost-user.sock"
    }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/drives/rootfs", Some(body)),
    )
    .await;
    assert_eq!(
        resp.status,
        204,
        "vhost-user socket should be accept-and-warn; got {} body={:?}",
        resp.status,
        resp.body_str()
    );
    server.shutdown().await;
}

/// A: `/machine-config.huge_pages = "2M"` is accepted.
#[tokio::test]
async fn test_a_should_accept_huge_pages_field() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "vcpu_count": 1, "mem_size_mib": 256, "huge_pages": "2M" }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/machine-config", Some(body)),
    )
    .await;
    assert_eq!(
        resp.status,
        204,
        "huge_pages=\"2M\" should be accept-and-warn; got {} body={:?}",
        resp.status,
        resp.body_str()
    );
    server.shutdown().await;
}

/// A: `/cpu-config` accepts x86-shaped fields (`cpuid_modifiers` /
/// `msr_modifiers` / `kvm_capabilities`) without error.
#[tokio::test]
async fn test_a_should_accept_cpu_config_x86_fields() {
    let server = CompatServer::spawn().await;
    let body = r#"{
        "cpuid_modifiers": [{ "leaf": "0x1", "subleaf": "0x0", "flags": "0", "modifiers": [] }],
        "msr_modifiers":   [{ "addr": "0x10", "bitmap": "0bxxxxxxxx" }],
        "kvm_capabilities": ["+IRQCHIP"]
    }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/cpu-config", Some(body)),
    )
    .await;
    assert!(
        matches!(resp.status, 200 | 204),
        "cpu-config x86 fields should be accept-and-warn; got {} body={:?}",
        resp.status,
        resp.body_str()
    );
    server.shutdown().await;
}

/// A: `/snapshot/load.clock_realtime` is accepted but ignored. With the stub VMM
/// the action surfaces a `400` `fault_message` documenting the wiring gap; that is
/// the *post-validation* response, which proves `clock_realtime` did not get
/// rejected during parse. (When the live VMM lands the response is 204.)
#[tokio::test]
async fn test_a_should_accept_clock_realtime_in_snapshot_load() {
    let server = CompatServer::spawn().await;
    let body = r#"{
        "snapshot_path": "/tmp/x.snap",
        "mem_backend": { "backend_type": "File", "backend_path": "/tmp/x.mem" },
        "clock_realtime": 1234567890
    }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/snapshot/load", Some(body)),
    )
    .await;
    // Stub VMM rejects the post-validation action with a documented `fault_message`
    // (see `93-improvements-review.md`, Phase 1 boot-tail entry). The status is 400
    // and the body must mention the snapshot subsystem stub. *Either way*,
    // clock_realtime did not cause a parse-time 400.
    assert_eq!(resp.status, 400);
    let body = resp.body_str().unwrap_or("");
    assert!(
        body.contains("Snapshot subsystem ready") || body.contains("track-A"),
        "unexpected stub response: {body:?}"
    );
    server.shutdown().await;
}
