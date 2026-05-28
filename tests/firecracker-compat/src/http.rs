//! Raw HTTP/1.1 client over a Unix domain socket.
//!
//! Built directly on `tokio::net::UnixStream` + `httparse` so the bytes on the wire
//! are exactly what an SDK / `firectl` would observe — no client-library massaging.

use std::{fmt::Write as _, io::ErrorKind, path::Path, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};

/// Parsed response: status code, headers (preserving original case), body bytes.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP status code (200, 204, 400, 413, 504, …).
    pub status: u16,
    /// Header pairs as the server emitted them.
    pub headers: Vec<(String, String)>,
    /// Body bytes — utf-8 only when the response is JSON, but the raw bytes are
    /// preserved for sniffing.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Look up a header value, case-insensitively.
    pub fn header<'a>(&'a self, name: &str) -> Option<&'a str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Body as a string slice, if utf-8.
    pub fn body_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.body).ok()
    }

    /// Body as parsed JSON.
    pub fn body_json(&self) -> serde_json::Result<serde_json::Value> {
        serde_json::from_slice(&self.body)
    }
}

/// Fire a raw HTTP/1.1 request over a UDS, read the response to EOF, parse it.
///
/// `raw_request` must be a fully-formed HTTP/1.1 request with `Connection: close` so
/// the server hangs up after responding (otherwise the read-to-EOF blocks until the
/// keepalive timeout).
///
/// On large bodies the server may close the write half before we finish writing
/// (e.g. body-cap rejection at 413). Treat a `BrokenPipe`/`UnexpectedEof` during
/// write as a normal early-rejection — what matters is the response we read back.
pub async fn http_request(socket: &Path, raw_request: &str) -> HttpResponse {
    let mut stream = UnixStream::connect(socket).await.expect("connect uds");
    if let Err(err) = stream.write_all(raw_request.as_bytes()).await {
        // Servers may hang up early (413 bodies, oversized headers). The response
        // shape is still in the read half — fall through to read it.
        match err.kind() {
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof => {}
            _ => panic!("write request: {err}"),
        }
    }
    let mut buf = Vec::with_capacity(2048);
    match timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .expect("response read timed out")
    {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::ConnectionReset && !buf.is_empty() => {}
        Err(err) => panic!("response read: {err}"),
    }
    parse_response(&buf)
}

/// Parse a raw HTTP/1.1 response buffer into [`HttpResponse`].
pub fn parse_response(buf: &[u8]) -> HttpResponse {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    let parsed = match response.parse(buf).expect("parse http response") {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => panic!(
            "incomplete http response (need more bytes); buffered:\n{:?}",
            String::from_utf8_lossy(buf)
        ),
    };
    let status = response.code.expect("status code");
    let header_vec: Vec<(String, String)> = response
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect();
    let body = buf[parsed..].to_vec();
    HttpResponse {
        status,
        headers: header_vec,
        body,
    }
}

/// Build a raw HTTP/1.1 request line + headers + body. The harness uses this rather
/// than `format!`-everywhere to keep transcripts readable.
pub fn build_request(method: &str, path: &str, body: Option<&str>) -> String {
    let mut s = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some(b) = body {
        s.push_str("Content-Type: application/json\r\n");
        write!(s, "Content-Length: {}\r\n\r\n", b.len()).expect("write to string");
        s.push_str(b);
    } else {
        s.push_str("Content-Length: 0\r\n\r\n");
    }
    s
}
