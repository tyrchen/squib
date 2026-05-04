//! Soak test pinning I-API-7: `GET /` must complete in microseconds even while a
//! multi-second `PUT /snapshot/load` is in flight.
//!
//! Per [20-firecracker-api.md §
//! 5.1](../../../specs/20-firecracker-api.md#51-read-only-fast-path--no-channel-round-trip):
//! the read-only handlers go through `ArcSwap`, never the VMM channel — a long-running
//! mutating action on the channel must not block liveness GETs.
//!
//! The test interleaves a stalled `PUT /snapshot/load` (the stub VMM holds the action
//! without responding) with rapid `GET /` calls and asserts every GET completes well
//! inside the spec's 5 ms p99 budget. We use a 200 ms hard cap rather than 5 ms to
//! tolerate macOS-host scheduler jitter under contention; the *shape* of the assertion
//! — that mutating actions do not stall reads — is what matters.

use std::{
    path::PathBuf,
    process,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use squib_api::{
    ApiResponse, ControllerSnapshot, RuntimeApiController, ServeOptions, TimeoutTable, serve,
    unlink_socket_if_exists,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};

fn unique_socket_path() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = process::id();
    std::env::temp_dir().join(format!("squib-api-soak-{pid}-{n}.sock"))
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

fn parse_status(buf: &[u8]) -> u16 {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    response.parse(buf).expect("parse").unwrap();
    response.code.expect("status")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_should_keep_get_root_responsive_during_long_snapshot_load() {
    let snap = ControllerSnapshot::new("anonymous", "1.16.0", "1.16.0 (squib soak)");
    let (controller, mut rx) = RuntimeApiController::new(
        snap,
        // Generous timeout so the stalling mutating action never trips a 504 in the
        // test window — the point is that the GET path is unaffected by the stall,
        // not that the timeout fires.
        TimeoutTable::from_spec(),
        128,
    );
    let controller = Arc::new(controller);

    // Stub VMM: drain the channel but stall on the *first* mutating action it
    // sees, holding the oneshot for ~1 second before replying. All other actions
    // are acked immediately. The test only ever sends one mutating action so
    // this models the realistic "long action in flight" shape.
    let stub = tokio::spawn(async move {
        let mut stalled_once = false;
        while let Some((_action, ack)) = rx.recv().await {
            if !stalled_once {
                stalled_once = true;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let _ = ack.send(ApiResponse::NoContent);
        }
    });

    let socket = unique_socket_path();
    unlink_socket_if_exists(&socket).await.unwrap();
    let opts = ServeOptions::new(&socket);
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        let _ = serve(opts, controller).await;
    });

    // Wait for bind.
    for _ in 0..100 {
        if server_socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(server_socket.exists(), "server failed to bind");

    // Kick off the stalling PUT in the background.
    let stalling_socket = server_socket.clone();
    let mutating = tokio::spawn(async move {
        let body = r#"{"snapshot_path":"/tmp/x.snap","mem_backend":{"backend_type":"File","backend_path":"/tmp/x.mem"}}"#;
        let req = format!(
            "PUT /snapshot/load HTTP/1.1\r\nHost: localhost\r\nContent-Type: \
             application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        http_request(&stalling_socket, &req).await
    });

    // Hammer GET / in parallel and record latencies.
    let mut latencies = Vec::with_capacity(50);
    for _ in 0..50 {
        let start = Instant::now();
        let raw = http_request(
            &server_socket,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        let elapsed = start.elapsed();
        let status = parse_status(&raw);
        assert_eq!(status, 200);
        latencies.push(elapsed);
        // Brief breath so the loop isn't 100% CPU.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // The stalling mutating action must still complete around the 1s mark.
    let mutating_raw = mutating.await.unwrap();
    let mutating_status = parse_status(&mutating_raw);
    assert_eq!(mutating_status, 204);

    latencies.sort();
    let p99 = latencies[(latencies.len() * 99) / 100];
    // Spec target is 5 ms p99; we relax to 200 ms in the test to tolerate macOS-host
    // scheduler jitter on busy CI runners. Anything above this would mean the GET is
    // serializing on the same channel as PUT, which is exactly what I-API-7 forbids.
    assert!(
        p99 < Duration::from_millis(200),
        "GET / p99={p99:?} during long PUT — read fast path is blocked"
    );

    // Tear down.
    server.abort();
    stub.abort();
    let _ = unlink_socket_if_exists(&server_socket).await;
}
