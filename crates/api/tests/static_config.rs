//! Integration tests for the static-config replay path (`--config-file`).
//!
//! Per [20-firecracker-api.md §
//! 6](../../../specs/20-firecracker-api.md#6-static-config-file---config-file) and **I-API-4**: the
//! static-config-file path produces the *same* `ApiAction` sequence (and same errors) as the
//! equivalent HTTP transcript. This test asserts both the deterministic order and the
//! same-validation invariant.

use std::{sync::Arc, time::Duration};

use squib_api::{
    ActionReceiver, ApiResponse, ControllerSnapshot, RuntimeApiController, TimeoutTable,
    parse_config_file, replay_config, schemas::ConfigFile,
};

fn build_controller() -> (Arc<RuntimeApiController>, ActionReceiver) {
    let snap = ControllerSnapshot::new("anonymous", "1.16.0", "1.16.0 (squib test)");
    let (c, rx) = RuntimeApiController::new(snap, TimeoutTable::from_spec(), 64);
    (Arc::new(c), rx)
}

fn drain(mut rx: ActionReceiver) -> tokio::task::JoinHandle<Vec<&'static str>> {
    tokio::spawn(async move {
        let mut v = Vec::new();
        while let Some((action, ack)) = rx.recv().await {
            v.push(action.label());
            let _ = ack.send(ApiResponse::NoContent);
        }
        v
    })
}

#[tokio::test]
async fn test_should_replay_full_envelope_in_deterministic_order() {
    let (c, rx) = build_controller();
    let drain_handle = drain(rx);

    let cfg: ConfigFile = serde_json::from_str(
        r#"{
            "boot-source": {"kernel_image_path":"/tmp/k","boot_args":"console=ttyAMA0"},
            "machine-config": {"vcpu_count":2,"mem_size_mib":512,"track_dirty_pages":true},
            "drives": [
                {"drive_id":"rootfs","path_on_host":"/tmp/r.img","is_root_device":true},
                {"drive_id":"data","path_on_host":"/tmp/d.img"}
            ],
            "network-interfaces": [
                {"iface_id":"eth0","host_dev_name":"tap0","guest_mac":"aa:bb:cc:dd:ee:ff"}
            ],
            "vsock": {"guest_cid":3,"uds_path":"/tmp/vsock.sock"},
            "mmds-config": {"network_interfaces":["eth0"]},
            "balloon": {"amount_mib":0},
            "entropy": {},
            "logger": {"log_path":"/tmp/squib.log"},
            "metrics": {"metrics_path":"/tmp/squib.metrics"},
            "squib": {"network":"shared"}
        }"#,
    )
    .unwrap();
    replay_config(&c, cfg, true).await.unwrap();
    drop(c);

    let labels = tokio::time::timeout(Duration::from_secs(2), drain_handle)
        .await
        .unwrap()
        .unwrap();

    // The deterministic order from the spec.
    let expected = [
        "PUT /machine-config",
        "PUT /boot-source",
        "PUT /drives/{id}",
        "PUT /drives/{id}",
        "PUT /network-interfaces/{id}",
        "PUT /vsock",
        "PUT /mmds/config",
        "PUT /balloon",
        "PUT /entropy",
        "PUT /logger",
        "PUT /metrics",
        "PUT /actions",
    ];
    assert_eq!(labels, expected, "replay order does not match the spec");
}

#[tokio::test]
async fn test_should_apply_same_validation_for_static_config_as_http() {
    // HTTP would reject `smt: true` on aarch64 with the documented fault. The same
    // input through the static-config path must reject the same way (I-API-4).
    let (c, _rx) = build_controller();
    let cfg: ConfigFile = serde_json::from_str(
        r#"{"boot-source":{"kernel_image_path":"/tmp/k"},
             "machine-config":{"vcpu_count":1,"mem_size_mib":256,"smt":true}}"#,
    )
    .unwrap();
    let err = replay_config(&c, cfg, false).await.unwrap_err();
    let s = err.to_string();
    assert!(
        s.contains("SMT not supported on Apple Silicon"),
        "static-config validation drifted from HTTP shape: {s}"
    );
}

#[tokio::test]
async fn test_should_reject_static_config_without_boot_source() {
    let (c, _rx) = build_controller();
    let cfg = ConfigFile::default();
    let err = replay_config(&c, cfg, false).await.unwrap_err();
    let s = err.to_string();
    assert!(s.contains("boot-source"), "wrong error: {s}");
}

#[tokio::test]
async fn test_should_parse_config_file_from_disk() {
    let dir = tempdir_path();
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("config.json");
    let content = r#"{
        "boot-source": {"kernel_image_path":"/tmp/k"},
        "machine-config": {"vcpu_count":1,"mem_size_mib":128}
    }"#;
    tokio::fs::write(&path, content).await.unwrap();

    let cfg = parse_config_file(&path).await.unwrap();
    assert!(cfg.boot_source.is_some());
    assert_eq!(cfg.machine_config.unwrap().vcpu_count, 1);
}

fn tempdir_path() -> std::path::PathBuf {
    let pid = std::process::id();
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("squib-static-config-{pid}-{n}"))
}
