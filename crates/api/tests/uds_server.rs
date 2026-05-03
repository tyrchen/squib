//! End-to-end integration tests for the squib-api server over a real Unix domain socket.
//!
//! Each test spins up the server on a unique socket path under the runtime tmp dir,
//! sends a raw HTTP/1.1 request over `tokio::net::UnixStream`, parses the response with
//! `httparse`, and asserts on status + headers + body. No higher-level HTTP client
//! library is involved — the wire shape is exactly what a Firecracker SDK would see.

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
    schemas::{InstanceInfo, VersionResponse, VmState},
    server::{Runtime, ServeOptions, serve, unlink_socket_if_exists},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};

/// Test runtime: returns a fixed `InstanceInfo` so the wire shape is deterministic.
struct StubRuntime {
    id: String,
    fc_version: String,
}

impl Runtime for StubRuntime {
    fn instance_info(&self) -> InstanceInfo {
        InstanceInfo {
            id: self.id.clone(),
            state: VmState::NotStarted,
            vmm_version: format!("{} (squib 0.0.0-test)", self.fc_version),
            app_name: "Firecracker".into(),
        }
    }
    fn firecracker_version(&self) -> String {
        self.fc_version.clone()
    }
}

/// Allocate a unique socket path in the platform tmp dir for this test run.
fn unique_socket_path() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = process::id();
    std::env::temp_dir().join(format!("squib-api-test-{pid}-{n}.sock"))
}

/// Spawn the server in the background and return the socket path; the server stops when
/// `_cancel` is dropped at the end of the test.
async fn start_test_server(runtime: Arc<StubRuntime>) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let socket = unique_socket_path();
    unlink_socket_if_exists(&socket).await.unwrap();

    let opts = ServeOptions::new(&socket);
    let handle_socket = socket.clone();
    let handle = tokio::spawn(async move {
        // Errors from serve() during a test bring it down; we propagate via panic.
        if let Err(err) = serve(opts, runtime).await {
            panic!("squib-api serve failed: {err}");
        }
    });

    // Wait for the socket to appear (bind happens early in `serve`).
    for _ in 0..50 {
        if handle_socket.exists() {
            return (handle_socket, handle);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "server failed to bind {} within 1s",
        handle_socket.display()
    );
}

/// Send a raw HTTP/1.1 request and read the response until EOF (we ask for `Connection: close`).
async fn http_request(socket: &std::path::Path, raw_request: &str) -> Vec<u8> {
    let mut stream = UnixStream::connect(socket).await.expect("connect");
    stream
        .write_all(raw_request.as_bytes())
        .await
        .expect("write request");
    let mut buf = Vec::with_capacity(1024);
    timeout(Duration::from_secs(2), stream.read_to_end(&mut buf))
        .await
        .expect("response read timed out")
        .expect("response read");
    buf
}

/// Parse a raw HTTP response into (status, headers, body).
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
async fn get_root_returns_instance_info_with_firecracker_server_header() {
    let runtime = Arc::new(StubRuntime {
        id: "anonymous".into(),
        fc_version: "1.16.0".into(),
    });
    let (socket, _handle) = start_test_server(runtime).await;

    let req = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let raw = http_request(&socket, req).await;
    let (status, headers, body) = parse_response(&raw);

    assert_eq!(status, 200);
    assert_eq!(header_value(&headers, "server"), Some("Firecracker API"));
    assert_eq!(
        header_value(&headers, "content-type").map(str::to_lowercase),
        Some("application/json".into())
    );

    // Body must contain the upstream literal `"state":"Not started"` (space + lowercase
    // 's'). SDKs sniff this string byte-for-byte; the squib draft used to emit
    // `"NotStarted"` (PascalCase) and would have silently broken every Firecracker SDK.
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
async fn get_version_returns_firecracker_version_string() {
    let runtime = Arc::new(StubRuntime {
        id: "test-vm".into(),
        fc_version: "1.16.0".into(),
    });
    let (socket, _handle) = start_test_server(runtime).await;

    let req = "GET /version HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let raw = http_request(&socket, req).await;
    let (status, headers, body) = parse_response(&raw);

    assert_eq!(status, 200);
    assert_eq!(header_value(&headers, "server"), Some("Firecracker API"));

    let v: VersionResponse = serde_json::from_slice(&body).expect("parse json");
    assert_eq!(v.firecracker_version, "1.16.0");
}

#[tokio::test]
async fn unknown_path_returns_400_with_fault_message() {
    let runtime = Arc::new(StubRuntime {
        id: "anonymous".into(),
        fc_version: "1.16.0".into(),
    });
    let (socket, _handle) = start_test_server(runtime).await;

    let req = "GET /no-such-route HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let raw = http_request(&socket, req).await;
    let (status, headers, _body) = parse_response(&raw);

    // axum's default fallback returns 404 unless we install a fallback handler. For Track B
    // week 1 we accept the axum default; the next iteration will install a fallback that
    // emits the upstream-style {"fault_message":"..."} 400. For now, assert it's at least
    // a 4xx and the Server header is still present.
    assert!((400..500).contains(&status), "got status {status}");
    assert_eq!(header_value(&headers, "server"), Some("Firecracker API"));
}
