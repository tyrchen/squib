//! End-to-end integration tests for the squib-api server over a real Unix domain socket.
//!
//! Each test spins up the server on a unique socket path under the runtime tmp dir,
//! sends a raw HTTP/1.1 request over `tokio::net::UnixStream`, parses the response
//! with `httparse`, and asserts on status + headers + body. No higher-level HTTP
//! client library is involved — the wire shape is exactly what a Firecracker SDK
//! would see.

use std::{
    path::PathBuf,
    process,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use squib_api::{
    ActionReceiver, ApiResponse, ControllerSnapshot, RuntimeApiController, ServeOptions,
    TimeoutTable,
    schemas::{InstanceInfo, VersionResponse, VmState},
    serve, unlink_socket_if_exists,
};
use squib_core::LifecyclePhase;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};

fn unique_socket_path() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = process::id();
    std::env::temp_dir().join(format!("squib-api-test-{pid}-{n}.sock"))
}

fn make_controller() -> (Arc<RuntimeApiController>, ActionReceiver) {
    make_controller_in(LifecyclePhase::Uninitialized)
}

fn make_controller_in(phase: LifecyclePhase) -> (Arc<RuntimeApiController>, ActionReceiver) {
    let mut snap = ControllerSnapshot::new("anonymous", "1.16.0", "1.16.0 (squib 0.0.0-test)");
    snap.phase = phase;
    snap.instance_info.state = phase.wire_state().into();
    let (c, rx) = RuntimeApiController::new(snap, TimeoutTable::from_spec(), 64);
    (Arc::new(c), rx)
}

/// Spawn the server and a no-op VMM stub that ack's every action with `204 No Content`.
async fn start_server(
    controller: Arc<RuntimeApiController>,
) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let socket = unique_socket_path();
    unlink_socket_if_exists(&socket).await.unwrap();
    let opts = ServeOptions::new(&socket);
    let socket_for_handle = socket.clone();
    let handle = tokio::spawn(async move {
        if let Err(err) = serve(opts, controller).await {
            panic!("squib-api serve failed: {err}");
        }
    });
    for _ in 0..100 {
        if socket_for_handle.exists() {
            return (socket_for_handle, handle);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "server failed to bind {} within 2s",
        socket_for_handle.display()
    );
}

fn drain_acker(mut rx: ActionReceiver) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some((_action, ack)) = rx.recv().await {
            let _ = ack.send(ApiResponse::NoContent);
        }
    })
}

async fn http_request(socket: &std::path::Path, raw_request: &str) -> Vec<u8> {
    let mut stream = UnixStream::connect(socket).await.expect("connect");
    stream
        .write_all(raw_request.as_bytes())
        .await
        .expect("write request");
    let mut buf = Vec::with_capacity(1024);
    timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .expect("response read timed out")
        .expect("response read");
    buf
}

fn parse_response(buf: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    let parsed = response.parse(buf).expect("parse").unwrap();
    let status = response.code.expect("status code");
    let header_vec = response
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).to_string(),
            )
        })
        .collect::<Vec<_>>();
    let body = buf[parsed..].to_vec();
    (status, header_vec, body)
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn test_should_serve_get_root_with_firecracker_server_header() {
    let (c, rx) = make_controller();
    let (socket, _handle) = start_server(c).await;
    let _drain = drain_acker(rx);

    let req = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let raw = http_request(&socket, req).await;
    let (status, headers, body) = parse_response(&raw);

    assert_eq!(status, 200);
    assert_eq!(header_value(&headers, "server"), Some("Firecracker API"));
    assert_eq!(
        header_value(&headers, "content-type").map(str::to_lowercase),
        Some("application/json".into())
    );

    let body_str = std::str::from_utf8(&body).expect("utf8 body");
    assert!(
        body_str.contains(r#""state":"Not started""#),
        "body did not contain upstream-shaped state field; got: {body_str}"
    );

    let info: InstanceInfo = serde_json::from_slice(&body).expect("parse json");
    assert_eq!(info.id, "anonymous");
    assert_eq!(info.app_name, "Firecracker");
    assert_eq!(info.state, VmState::NotStarted);
    assert!(info.vmm_version.contains("1.16.0"));
}

#[tokio::test]
async fn test_should_serve_get_version() {
    let (c, rx) = make_controller();
    let (socket, _handle) = start_server(c).await;
    let _drain = drain_acker(rx);

    let req = "GET /version HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let raw = http_request(&socket, req).await;
    let (status, headers, body) = parse_response(&raw);

    assert_eq!(status, 200);
    assert_eq!(header_value(&headers, "server"), Some("Firecracker API"));

    let v: VersionResponse = serde_json::from_slice(&body).expect("parse json");
    assert_eq!(v.firecracker_version, "1.16.0");
}

#[tokio::test]
async fn test_should_translate_unknown_path_to_400_with_fault_message() {
    let (c, rx) = make_controller();
    let (socket, _handle) = start_server(c).await;
    let _drain = drain_acker(rx);

    let req = "GET /no-such-route HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let raw = http_request(&socket, req).await;
    let (status, headers, body) = parse_response(&raw);

    assert_eq!(status, 400);
    assert_eq!(header_value(&headers, "server"), Some("Firecracker API"));
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(
        body_str.contains(r#""fault_message""#),
        "missing fault_message; got {body_str}"
    );
    assert!(body_str.contains("/no-such-route"));
}

#[tokio::test]
async fn test_should_accept_put_machine_config_with_fault_message_on_smt_true() {
    let (c, rx) = make_controller();
    let (socket, _handle) = start_server(c).await;
    let _drain = drain_acker(rx);

    let body = r#"{"vcpu_count":1,"mem_size_mib":256,"smt":true}"#;
    let req = format!(
        "PUT /machine-config HTTP/1.1\r\nHost: localhost\r\nContent-Type: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let raw = http_request(&socket, &req).await;
    let (status, _headers, response_body) = parse_response(&raw);
    assert_eq!(status, 400);
    let body_str = std::str::from_utf8(&response_body).unwrap();
    assert!(
        body_str.contains("SMT not supported on Apple Silicon"),
        "unexpected body: {body_str}"
    );
}

#[tokio::test]
async fn test_should_accept_put_boot_source_against_stub_vmm() {
    let (c, rx) = make_controller();
    let (socket, _handle) = start_server(c).await;
    let _drain = drain_acker(rx);

    let body = r#"{"kernel_image_path":"/tmp/vmlinux.bin"}"#;
    let req = format!(
        "PUT /boot-source HTTP/1.1\r\nHost: localhost\r\nContent-Type: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let raw = http_request(&socket, &req).await;
    let (status, headers, _body) = parse_response(&raw);
    assert_eq!(status, 204);
    assert_eq!(header_value(&headers, "server"), Some("Firecracker API"));
}

#[tokio::test]
async fn test_should_route_pmem_patch_and_delete() {
    // PATCH/DELETE on /pmem are post-boot only.
    let (c, rx) = make_controller_in(LifecyclePhase::Running);
    let (socket, _handle) = start_server(c).await;
    let _drain = drain_acker(rx);

    let body = r#"{"pmem_id":"pmem0"}"#;
    let req = format!(
        "PATCH /pmem/pmem0 HTTP/1.1\r\nHost: localhost\r\nContent-Type: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let raw = http_request(&socket, &req).await;
    let (status, _h, _b) = parse_response(&raw);
    assert_eq!(status, 204);

    let req = "DELETE /pmem/pmem0 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let raw = http_request(&socket, req).await;
    let (status, _h, _b) = parse_response(&raw);
    assert_eq!(status, 204);
}

#[tokio::test]
async fn test_should_route_hotplug_memory_put_get_patch() {
    // PUT is pre-boot; PATCH is post-boot. We exercise both phases.
    let (c, rx) = make_controller_in(LifecyclePhase::Uninitialized);
    let (socket, _handle) = start_server(c).await;
    let _drain = drain_acker(rx);

    let body = r#"{"total_size_mib":256,"block_size_mib":2,"slot_size_mib":128}"#;
    let req = format!(
        "PUT /hotplug/memory HTTP/1.1\r\nHost: localhost\r\nContent-Type: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let raw = http_request(&socket, &req).await;
    let (status, _h, _b) = parse_response(&raw);
    assert_eq!(status, 204);

    let raw = http_request(
        &socket,
        "GET /hotplug/memory HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, _h, _b) = parse_response(&raw);
    assert_eq!(status, 200);
}

#[tokio::test]
async fn test_should_route_hotplug_memory_patch_post_boot() {
    let (c, rx) = make_controller_in(LifecyclePhase::Running);
    let (socket, _handle) = start_server(c).await;
    let _drain = drain_acker(rx);

    let body = r#"{"requested_size_mib":128}"#;
    let req = format!(
        "PATCH /hotplug/memory HTTP/1.1\r\nHost: localhost\r\nContent-Type: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let raw = http_request(&socket, &req).await;
    let (status, _h, _b) = parse_response(&raw);
    assert_eq!(status, 204);
}

#[tokio::test]
async fn test_should_route_balloon_hinting() {
    // balloon-hinting is post-boot only.
    let (c, rx) = make_controller_in(LifecyclePhase::Running);
    let (socket, _handle) = start_server(c).await;
    let _drain = drain_acker(rx);

    for op in ["start", "status", "stop"] {
        let req = format!(
            "PATCH /balloon/hinting/{op} HTTP/1.1\r\nHost: localhost\r\nContent-Length: \
             0\r\nConnection: close\r\n\r\n"
        );
        let raw = http_request(&socket, &req).await;
        let (status, _h, _b) = parse_response(&raw);
        assert_eq!(status, 204, "balloon-hinting {op} should 204");
    }

    // Invalid op should return 400.
    let req = "PATCH /balloon/hinting/frobnicate HTTP/1.1\r\nHost: localhost\r\nContent-Length: \
               0\r\nConnection: close\r\n\r\n";
    let raw = http_request(&socket, req).await;
    let (status, _h, body) = parse_response(&raw);
    assert_eq!(status, 400);
    assert!(std::str::from_utf8(&body).unwrap().contains("frobnicate"));
}

#[tokio::test]
async fn test_should_reject_send_ctrl_alt_del_with_400() {
    let (c, rx) = make_controller();
    let (socket, _handle) = start_server(c).await;
    let _drain = drain_acker(rx);

    let body = r#"{"action_type":"SendCtrlAltDel"}"#;
    let req = format!(
        "PUT /actions HTTP/1.1\r\nHost: localhost\r\nContent-Type: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let raw = http_request(&socket, &req).await;
    let (status, _headers, response_body) = parse_response(&raw);
    assert_eq!(status, 400);
    let body_str = std::str::from_utf8(&response_body).unwrap();
    assert!(
        body_str.contains("SendCtrlAltDel is x86-only"),
        "unexpected body: {body_str}"
    );
}
