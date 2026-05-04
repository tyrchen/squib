//! F rows — every endpoint in [21-api-compat-matrix.md §
//! 1](../../../specs/21-api-compat-matrix.md#1-http-api-endpoints) returns its
//! documented status for at least one happy-path call.
//!
//! Each test below exercises a single endpoint with the simplest valid body and
//! asserts the upstream status code. The list mirrors § 1 row-by-row so a future
//! addition to the matrix surfaces as a missing test.

use firecracker_compat::{
    CompatServer,
    http::{build_request, http_request},
};
use squib_core::LifecyclePhase;

#[tokio::test]
async fn f_get_root() {
    let server = CompatServer::spawn().await;
    let resp = http_request(server.socket(), &build_request("GET", "/", None)).await;
    assert_eq!(resp.status, 200);
    server.shutdown().await;
}

#[tokio::test]
async fn f_get_version() {
    let server = CompatServer::spawn().await;
    let resp = http_request(server.socket(), &build_request("GET", "/version", None)).await;
    assert_eq!(resp.status, 200);
    server.shutdown().await;
}

#[tokio::test]
async fn f_get_vm_config() {
    let server = CompatServer::spawn().await;
    let resp = http_request(server.socket(), &build_request("GET", "/vm/config", None)).await;
    assert_eq!(resp.status, 200);
    server.shutdown().await;
}

#[tokio::test]
async fn f_get_machine_config() {
    let server = CompatServer::spawn().await;
    let resp = http_request(
        server.socket(),
        &build_request("GET", "/machine-config", None),
    )
    .await;
    assert_eq!(resp.status, 200);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_machine_config() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "vcpu_count": 1, "mem_size_mib": 128 }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/machine-config", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_patch_machine_config() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "vcpu_count": 2 }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PATCH", "/machine-config", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_boot_source() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "kernel_image_path": "/tmp/k", "boot_args": "console=ttyAMA0" }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/boot-source", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_drive_root() {
    let server = CompatServer::spawn().await;
    let body = r#"{
        "drive_id": "rootfs",
        "path_on_host": "/tmp/rootfs.img",
        "is_root_device": true,
        "is_read_only": false
    }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/drives/rootfs", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_network_interface() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "iface_id": "eth0", "host_dev_name": "tap0" }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/network-interfaces/eth0", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_vsock() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "guest_cid": 3, "uds_path": "/tmp/squib-vsock.sock" }"#;
    let resp = http_request(server.socket(), &build_request("PUT", "/vsock", Some(body))).await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_get_mmds_pre_seed() {
    let server = CompatServer::spawn().await;
    let resp = http_request(server.socket(), &build_request("GET", "/mmds", None)).await;
    assert_eq!(resp.status, 200);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_mmds_then_patch() {
    let server = CompatServer::spawn().await;
    let put_body = r#"{ "latest": { "meta-data": { "instance-id": "i-12345" } } }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/mmds", Some(put_body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    let patch_body = r#"{ "latest": { "meta-data": { "instance-id": "i-67890" } } }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PATCH", "/mmds", Some(patch_body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_mmds_config() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "version": "V2", "network_interfaces": ["eth0"] }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/mmds/config", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_balloon() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "amount_mib": 64, "deflate_on_oom": true }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/balloon", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_get_balloon_pre_boot_returns_empty() {
    let server = CompatServer::spawn().await;
    let resp = http_request(server.socket(), &build_request("GET", "/balloon", None)).await;
    assert_eq!(resp.status, 200);
    server.shutdown().await;
}

#[tokio::test]
async fn f_balloon_hinting_post_boot() {
    let server = CompatServer::spawn_with(
        LifecyclePhase::Running,
        firecracker_compat::StubBehaviour::Production,
    )
    .await;
    for op in ["start", "status", "stop"] {
        let resp = http_request(
            server.socket(),
            &build_request("PATCH", &format!("/balloon/hinting/{op}"), None),
        )
        .await;
        assert_eq!(resp.status, 204, "balloon-hinting {op} status");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_entropy() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "rate_limiter": null }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/entropy", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_serial() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "log_path": "/dev/null" }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/serial", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_pmem() {
    let server = CompatServer::spawn().await;
    let body = r#"{
        "pmem_id": "pm0",
        "path_on_host": "/tmp/pmem.img",
        "is_read_only": false
    }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/pmem/pm0", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_hotplug_memory() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "total_size_mib": 256, "block_size_mib": 2, "slot_size_mib": 128 }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/hotplug/memory", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_get_hotplug_memory() {
    let server = CompatServer::spawn().await;
    let resp = http_request(
        server.socket(),
        &build_request("GET", "/hotplug/memory", None),
    )
    .await;
    assert_eq!(resp.status, 200);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_logger() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "log_path": "/dev/null", "level": "Info", "show_level": true, "show_log_origin": true }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/logger", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_metrics() {
    let server = CompatServer::spawn().await;
    let body = r#"{ "metrics_path": "/dev/null" }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/metrics", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}

#[tokio::test]
async fn f_put_actions_flush_metrics() {
    // FlushMetrics is post-boot only — see crates/api/src/action.rs.
    let server = CompatServer::spawn_with(
        LifecyclePhase::Running,
        firecracker_compat::StubBehaviour::Production,
    )
    .await;
    let body = r#"{ "action_type": "FlushMetrics" }"#;
    let resp = http_request(
        server.socket(),
        &build_request("PUT", "/actions", Some(body)),
    )
    .await;
    assert_eq!(resp.status, 204);
    server.shutdown().await;
}
