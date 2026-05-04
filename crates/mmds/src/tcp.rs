//! TCP server for the MMDS endpoint.
//!
//! Per [15-mmds.md § 3](../../../specs/15-mmds.md#3-packet-interception):
//! the dumbo TCP stack accepts guest-initiated connections to
//! `169.254.169.254:80` and serves HTTP/1.1 requests against the MMDS
//! data store. This module is a focused implementation of RFC 793
//! handshake + ESTABLISHED + graceful close, sized for the MMDS
//! request/response pattern (single connection at a time per
//! `(src_ip, src_port)`, no streaming, no out-of-order reassembly).
//!
//! ## State machine
//!
//! ```text
//!     CLOSED ───SYN───► SYN_RECEIVED ───ACK───► ESTABLISHED
//!                                                    │
//!                          ┌──────FIN/PSH+data───────┤
//!                          ▼                          ▼
//!                       CLOSE_WAIT                receive HTTP req
//!                          │                          │
//!                          │                          ▼
//!                          ▼                       send HTTP resp + FIN
//!                       LAST_ACK ───ACK───►       (LAST_ACK)
//!                                              ───ACK───► CLOSED
//! ```
//!
//! Squib's MMDS use case is request-then-response; we send our FIN on
//! the same segment as the response payload's last byte. Out-of-order
//! data segments and retransmit are not implemented — the guest's TCP
//! stack handles retransmission, and at link-local distance loss is
//! near-zero.

use std::collections::HashMap;

use parking_lot::Mutex;
use tracing::{debug, trace};

use crate::pdu::{
    ETHER_HDR_LEN,
    ipv4::{IPV4_HDR_LEN, IpProtocol, Ipv4Header},
    tcp::{TCP_ACK, TCP_FIN, TCP_PSH, TCP_RST, TCP_SYN, TcpHeader},
};

/// Per-connection state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TcpState {
    /// SYN seen, SYN-ACK sent. Waiting for the driver's ACK.
    SynReceived,
    /// Handshake complete; ready to receive an HTTP request.
    Established,
    /// HTTP response sent with FIN; waiting for the driver's ACK of our
    /// FIN.
    LastAck,
}

/// Connection key — `(client_ip, client_port)`. The MMDS endpoint is a
/// single `(server_ip, 80)` so the key uniquely identifies a flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ConnKey {
    src_ip: [u8; 4],
    src_port: u16,
}

/// Per-connection bookkeeping.
#[derive(Debug)]
struct Conn {
    state: TcpState,
    /// Server's send-next sequence number.
    snd_nxt: u32,
    /// Next byte we expect from the client (sequence number).
    rcv_nxt: u32,
    /// Receive-side accumulator: HTTP request bytes seen so far.
    rcv_buf: Vec<u8>,
}

impl Conn {
    fn new(client_seq: u32, server_iss: u32) -> Self {
        Self {
            state: TcpState::SynReceived,
            snd_nxt: server_iss + 1, // SYN consumes 1
            rcv_nxt: client_seq + 1,
            rcv_buf: Vec::new(),
        }
    }
}

/// HTTP response built by the data-store handler.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Status code (200, 401, 404, ...).
    pub status: u16,
    /// Body bytes — either JSON or a `fault_message` payload.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Render the response into the wire-format HTTP/1.1 message,
    /// including `Content-Length` and `Connection: close` so the guest's
    /// HTTP parser knows the body bound and that we'll FIN.
    #[must_use]
    pub fn to_wire(&self) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            414 => "URI Too Long",
            500 => "Internal Server Error",
            _ => "Unknown",
        };
        let header = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: \
             application/json\r\nConnection: close\r\n\r\n",
            self.status,
            reason,
            self.body.len(),
        );
        let mut out = Vec::with_capacity(header.len() + self.body.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&self.body);
        out
    }
}

/// HTTP request line + headers parsed from `rcv_buf`.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// `GET`, `PUT`, …
    pub method: String,
    /// URL path including the query string. Length is bounded by the
    /// caller (we slice the buffer down at parse time).
    pub path: String,
    /// `X-aws-ec2-metadata-token` header value, if present.
    pub token_header: Option<String>,
    /// `X-aws-ec2-metadata-token-ttl-seconds`, if present and parseable.
    pub ttl_header: Option<u32>,
    /// Body bytes (`Content-Length`-many bytes after the blank line).
    pub body: Vec<u8>,
}

/// Maximum request size we will accumulate before responding with 413.
/// Anything beyond a few KiB on the MMDS endpoint is malformed.
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// HTTP parser outcome.
#[derive(Debug)]
enum HttpParseOutcome {
    /// Need more bytes.
    Incomplete,
    /// Request fully parsed.
    Complete(HttpRequest),
    /// Malformed — return a 400 response and close.
    Malformed,
    /// Body exceeds [`MAX_REQUEST_BYTES`] — return 413 and close.
    TooLarge,
}

/// Parse an HTTP/1.1 request from `buf`. Returns
/// [`HttpParseOutcome::Incomplete`] if we haven't yet seen all bytes.
fn try_parse_http(buf: &[u8]) -> HttpParseOutcome {
    if buf.len() > MAX_REQUEST_BYTES {
        return HttpParseOutcome::TooLarge;
    }
    let Some(header_end) = find_subsequence(buf, b"\r\n\r\n") else {
        return HttpParseOutcome::Incomplete;
    };
    let header_bytes = &buf[..header_end];
    let Ok(header_str) = std::str::from_utf8(header_bytes) else {
        return HttpParseOutcome::Malformed;
    };
    let mut lines = header_str.split("\r\n");
    let Some(request_line) = lines.next() else {
        return HttpParseOutcome::Malformed;
    };
    let mut parts = request_line.split_whitespace();
    let method = match parts.next() {
        Some(m) => m.to_string(),
        None => return HttpParseOutcome::Malformed,
    };
    let path = match parts.next() {
        Some(p) => p.to_string(),
        None => return HttpParseOutcome::Malformed,
    };
    // We don't enforce HTTP/1.1 vs 1.0 — the MMDS endpoint speaks both.
    let mut content_length: usize = 0;
    let mut token_header: Option<String> = None;
    let mut ttl_header: Option<u32> = None;
    for line in lines {
        let Some((name, value)) = split_header(line) else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("x-aws-ec2-metadata-token") {
            token_header = Some(value.trim().to_string());
        } else if name.eq_ignore_ascii_case("x-aws-ec2-metadata-token-ttl-seconds") {
            ttl_header = value.trim().parse::<u32>().ok();
        }
    }
    let body_start = header_end + 4;
    let body_end = body_start + content_length;
    if body_end > MAX_REQUEST_BYTES {
        return HttpParseOutcome::TooLarge;
    }
    if buf.len() < body_end {
        return HttpParseOutcome::Incomplete;
    }
    HttpParseOutcome::Complete(HttpRequest {
        method,
        path,
        token_header,
        ttl_header,
        body: buf[body_start..body_end].to_vec(),
    })
}

fn split_header(line: &str) -> Option<(&str, &str)> {
    line.find(':').map(|i| (&line[..i], &line[i + 1..]))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Frame to be sent to the guest. `eth_payload` is the IPv4-and-up
/// bytes; the interceptor builds the Ethernet header outside.
#[derive(Debug, Clone)]
pub struct OutboundFrame {
    /// Destination MAC (the guest, learned from incoming Ethernet src).
    pub dst_mac: [u8; 6],
    /// IPv4 + TCP + payload.
    pub eth_payload: Vec<u8>,
}

/// Service that owns the MMDS TCP listener at `(server_ip, 80)`.
///
/// Hands an `HttpRequest` off to a user-supplied `serve_http` callback
/// when a complete request arrives, expects an [`HttpResponse`] back,
/// and writes the bytes onto the wire as one or more TCP segments.
pub struct TcpServer {
    inner: Mutex<TcpServerInner>,
    /// HTTP handler — invoked with a complete request, returns the
    /// response. Lives behind an `Arc` so the server is `Clone`.
    handler: std::sync::Arc<dyn Fn(HttpRequest) -> HttpResponse + Send + Sync>,
}

struct TcpServerInner {
    /// Server-side listening port (always 80 for MMDS).
    server_port: u16,
    /// Server IP (set from the interceptor's MMDS IP at construction).
    server_ip: [u8; 4],
    /// Synthetic MAC the server uses for Ethernet replies (static across
    /// connections). Stored for future use when the dumbo TCP server
    /// learns to emit gratuitous ARPs of its own; currently the
    /// interceptor's Ethernet wrapper supplies the source MAC.
    #[allow(dead_code)]
    server_mac: [u8; 6],
    /// MAC of the most recently-seen guest source — used as the
    /// destination MAC for our reply frames.
    guest_mac: Option<[u8; 6]>,
    /// Active connections.
    conns: HashMap<ConnKey, Conn>,
    /// Initial-sequence-number generator — RFC 793 says ISN should be
    /// time-rotating; we use a counter so tests are deterministic.
    next_iss: u32,
    /// Counter for IPv4 `id` field rotation.
    ip_id: u16,
    /// Frames to be delivered to the guest.
    out_queue: Vec<OutboundFrame>,
}

impl std::fmt::Debug for TcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock();
        f.debug_struct("TcpServer")
            .field("server_port", &inner.server_port)
            .field("server_ip", &inner.server_ip)
            .field("conns_open", &inner.conns.len())
            .finish_non_exhaustive()
    }
}

impl TcpServer {
    /// Build a server listening on `(server_ip, server_port)` with the
    /// supplied HTTP handler.
    #[must_use]
    pub fn new<F>(server_ip: [u8; 4], server_port: u16, server_mac: [u8; 6], handler: F) -> Self
    where
        F: Fn(HttpRequest) -> HttpResponse + Send + Sync + 'static,
    {
        Self {
            inner: Mutex::new(TcpServerInner {
                server_port,
                server_ip,
                server_mac,
                guest_mac: None,
                conns: HashMap::new(),
                next_iss: 0x4000_0000, // RFC 793 ISN can be anything; we pick a fixed start.
                ip_id: 1,
                out_queue: Vec::new(),
            }),
            handler: std::sync::Arc::new(handler),
        }
    }

    /// Update the server's IP. Called when `/mmds/config` patches the
    /// link-local address.
    pub fn set_ip(&self, ip: [u8; 4]) {
        self.inner.lock().server_ip = ip;
    }

    /// Cache the guest MAC address — the dumbo Ethernet builder uses it
    /// as the destination of every reply.
    pub fn set_guest_mac(&self, mac: [u8; 6]) {
        self.inner.lock().guest_mac = Some(mac);
    }

    /// Handle an inbound IPv4 frame from the guest. `payload` is the
    /// IPv4-and-up bytes (no Ethernet).
    ///
    /// Frames addressed to the wrong server IP / port pass through
    /// silently (`false`); frames we processed return `true`.
    pub fn handle_ip_frame(&self, eth_src_mac: [u8; 6], payload: &[u8]) -> bool {
        let Ok(ip) = Ipv4Header::parse(payload) else {
            trace!("dumbo: bad IPv4 header");
            return false;
        };
        if ip.protocol != IpProtocol::Tcp as u8 {
            return false;
        }
        let inner_server_ip = self.inner.lock().server_ip;
        if ip.dst != inner_server_ip {
            return false;
        }
        let tcp_start = IPV4_HDR_LEN;
        let tcp_end = ip.total_len as usize;
        if tcp_end > payload.len() || tcp_start >= tcp_end {
            return false;
        }
        // First parse the data offset from byte 12 of the TCP header to
        // know how many header bytes precede the payload.
        if payload.len() < tcp_start + 13 {
            return false;
        }
        let data_offset = payload[tcp_start + 12] >> 4;
        let tcp_hdr_end = tcp_start + 4 * data_offset as usize;
        if tcp_hdr_end > tcp_end {
            return false;
        }
        let tcp_payload = &payload[tcp_hdr_end..tcp_end];
        let Ok(tcp) = TcpHeader::parse(
            &payload[tcp_start..tcp_hdr_end],
            tcp_payload,
            ip.src,
            ip.dst,
        ) else {
            trace!("dumbo: bad TCP segment");
            return false;
        };
        if tcp.dst_port != self.inner.lock().server_port {
            return false;
        }
        self.inner.lock().guest_mac = Some(eth_src_mac);
        let key = ConnKey {
            src_ip: ip.src,
            src_port: tcp.src_port,
        };
        self.dispatch(key, ip, tcp, tcp_payload);
        true
    }

    fn dispatch(&self, key: ConnKey, ip: Ipv4Header, tcp: TcpHeader, payload: &[u8]) {
        // RST: drop the connection on either side.
        if tcp.is_rst() {
            self.inner.lock().conns.remove(&key);
            return;
        }
        // SYN with no ACK: open a fresh connection.
        if tcp.is_syn() && !tcp.is_ack() {
            self.handle_syn(key, ip, tcp);
            return;
        }
        // For everything else we need an existing connection.
        let mut inner = self.inner.lock();
        let Some(conn) = inner.conns.get_mut(&key) else {
            trace!("dumbo: segment for unknown connection — sending RST");
            // Synthesise a RST so the guest knows.
            let rst = TcpHeader::build(
                inner.server_port,
                tcp.src_port,
                tcp.ack,
                tcp.seq.wrapping_add(payload.len() as u32),
                TCP_RST | TCP_ACK,
                0,
            );
            let frame = build_outbound(&mut inner, rst, key.src_ip, key.src_port, &[]);
            inner.out_queue.push(frame);
            return;
        };
        // Drop sequenced ahead-of-time / out-of-order: we don't reassemble.
        if tcp.seq != conn.rcv_nxt && !payload.is_empty() {
            trace!(
                expected = conn.rcv_nxt,
                got = tcp.seq,
                "dumbo: out-of-order segment dropped"
            );
            return;
        }
        match conn.state {
            TcpState::SynReceived => {
                if tcp.is_ack() && tcp.ack == conn.snd_nxt {
                    conn.state = TcpState::Established;
                    debug!(?key, "dumbo: connection established");
                    if !payload.is_empty() {
                        Self::on_data(&self.handler, &mut inner, key, ip, tcp, payload);
                    }
                }
            }
            TcpState::Established => {
                if !payload.is_empty() {
                    Self::on_data(&self.handler, &mut inner, key, ip, tcp, payload);
                }
                if tcp.is_fin() {
                    Self::on_fin(&mut inner, key, ip, tcp);
                }
            }
            TcpState::LastAck => {
                if tcp.is_ack() && tcp.ack == conn.snd_nxt {
                    inner.conns.remove(&key);
                    debug!(?key, "dumbo: connection closed");
                }
            }
        }
    }

    fn handle_syn(&self, key: ConnKey, ip: Ipv4Header, tcp: TcpHeader) {
        let mut inner = self.inner.lock();
        let server_iss = inner.next_iss;
        inner.next_iss = inner.next_iss.wrapping_add(0x10000);
        let conn = Conn::new(tcp.seq, server_iss);
        // Build the SYN-ACK.
        let syn_ack = TcpHeader::build(
            inner.server_port,
            tcp.src_port,
            server_iss,
            conn.rcv_nxt,
            TCP_SYN | TCP_ACK,
            u16::MAX, // generous receive window
        );
        let frame = build_outbound(&mut inner, syn_ack, key.src_ip, key.src_port, &[]);
        inner.out_queue.push(frame);
        inner.conns.insert(key, conn);
        let _ = ip; // ip is captured above but useful for tracing in future
    }

    fn on_data(
        handler: &std::sync::Arc<dyn Fn(HttpRequest) -> HttpResponse + Send + Sync>,
        inner: &mut TcpServerInner,
        key: ConnKey,
        _ip: Ipv4Header,
        _tcp: TcpHeader,
        payload: &[u8],
    ) {
        let Some(conn) = inner.conns.get_mut(&key) else {
            return;
        };
        // Advance receive-next then bump the buffer.
        conn.rcv_nxt = conn.rcv_nxt.wrapping_add(payload.len() as u32);
        conn.rcv_buf.extend_from_slice(payload);
        // Send a bare ACK so the guest knows we received this segment.
        let ack = TcpHeader::build(
            inner.server_port,
            key.src_port,
            conn.snd_nxt,
            conn.rcv_nxt,
            TCP_ACK,
            u16::MAX,
        );
        let ack_frame = build_outbound(inner, ack, key.src_ip, key.src_port, &[]);
        inner.out_queue.push(ack_frame);
        // Try to parse a request.
        let buf = inner.conns[&key].rcv_buf.clone();
        match try_parse_http(&buf) {
            HttpParseOutcome::Incomplete => {}
            HttpParseOutcome::Malformed => {
                send_response(
                    inner,
                    key,
                    &HttpResponse {
                        status: 400,
                        body: br#"{"fault_message":"malformed request"}"#.to_vec(),
                    },
                );
            }
            HttpParseOutcome::TooLarge => {
                send_response(
                    inner,
                    key,
                    &HttpResponse {
                        status: 413,
                        body: br#"{"fault_message":"request too large"}"#.to_vec(),
                    },
                );
            }
            HttpParseOutcome::Complete(req) => {
                let response = (handler)(req);
                send_response(inner, key, &response);
            }
        }
    }

    fn on_fin(inner: &mut TcpServerInner, key: ConnKey, _ip: Ipv4Header, _tcp: TcpHeader) {
        // The driver is closing without waiting for our response — accept
        // the FIN, send our own FIN-ACK, transition to LAST_ACK.
        let Some(conn) = inner.conns.get_mut(&key) else {
            return;
        };
        conn.rcv_nxt = conn.rcv_nxt.wrapping_add(1); // FIN consumes 1
        let server_port = inner.server_port;
        let snd_nxt = conn.snd_nxt;
        let rcv_nxt = conn.rcv_nxt;
        conn.snd_nxt = snd_nxt.wrapping_add(1); // our FIN consumes 1
        conn.state = TcpState::LastAck;
        let fin_ack = TcpHeader::build(
            server_port,
            key.src_port,
            snd_nxt,
            rcv_nxt,
            TCP_FIN | TCP_ACK,
            u16::MAX,
        );
        let frame = build_outbound(inner, fin_ack, key.src_ip, key.src_port, &[]);
        inner.out_queue.push(frame);
    }

    /// Drain any frames the server wants delivered to the guest. Each
    /// `OutboundFrame` carries the IPv4-and-up bytes; the caller wraps
    /// it in Ethernet using the `dst_mac` field.
    pub fn drain_outbound(&self) -> Vec<OutboundFrame> {
        std::mem::take(&mut self.inner.lock().out_queue)
    }
}

fn send_response(inner: &mut TcpServerInner, key: ConnKey, response: &HttpResponse) {
    let wire = response.to_wire();
    // Send the response as a single segment (MTU on link-local is large
    // enough; if the guest indicated a smaller MSS we'd segment, but the
    // MMDS payload is always small).
    let server_port = inner.server_port;
    let (snd_seq, rcv_nxt) = {
        let Some(conn) = inner.conns.get_mut(&key) else {
            return;
        };
        let snd_seq = conn.snd_nxt;
        let payload_len = wire.len() as u32;
        conn.snd_nxt = snd_seq.wrapping_add(payload_len).wrapping_add(1); // +1 for our FIN
        conn.state = TcpState::LastAck;
        (snd_seq, conn.rcv_nxt)
    };
    let resp_seg = TcpHeader::build(
        server_port,
        key.src_port,
        snd_seq,
        rcv_nxt,
        TCP_ACK | TCP_PSH | TCP_FIN,
        u16::MAX,
    );
    let frame = build_outbound(inner, resp_seg, key.src_ip, key.src_port, &wire);
    inner.out_queue.push(frame);
}

fn build_outbound(
    inner: &mut TcpServerInner,
    tcp: TcpHeader,
    dst_ip: [u8; 4],
    _dst_port: u16,
    payload: &[u8],
) -> OutboundFrame {
    let server_ip = inner.server_ip;
    inner.ip_id = inner.ip_id.wrapping_add(1);
    let tcp_bytes = tcp.to_bytes_with_payload(server_ip, dst_ip, payload);
    let ip = Ipv4Header::build(
        server_ip,
        dst_ip,
        IpProtocol::Tcp,
        u16::try_from(tcp_bytes.len()).unwrap_or(u16::MAX),
        inner.ip_id,
    );
    let mut eth_payload = Vec::with_capacity(IPV4_HDR_LEN + tcp_bytes.len());
    eth_payload.extend_from_slice(&ip.to_bytes());
    eth_payload.extend_from_slice(&tcp_bytes);
    let dst_mac = inner.guest_mac.unwrap_or([0xFFu8; 6]);
    let _ = ETHER_HDR_LEN;
    OutboundFrame {
        dst_mac,
        eth_payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_server() -> TcpServer {
        TcpServer::new(
            [169, 254, 169, 254],
            80,
            [0x06, 0x01, 0x23, 0x45, 0x67, 0x01],
            |req| HttpResponse {
                status: 200,
                body: format!("hello {} {}", req.method, req.path).into_bytes(),
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_ip_tcp(
        src: [u8; 4],
        dst: [u8; 4],
        src_port: u16,
        dst_port: u16,
        seq: u32,
        ack: u32,
        flags: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let tcp = TcpHeader::build(src_port, dst_port, seq, ack, flags, u16::MAX);
        let tcp_bytes = tcp.to_bytes_with_payload(src, dst, payload);
        let ip = Ipv4Header::build(
            src,
            dst,
            IpProtocol::Tcp,
            u16::try_from(tcp_bytes.len()).unwrap(),
            42,
        );
        let mut frame = Vec::new();
        frame.extend_from_slice(&ip.to_bytes());
        frame.extend_from_slice(&tcp_bytes);
        frame
    }

    fn parse_outbound_tcp(
        frame: &OutboundFrame,
        server_ip: [u8; 4],
    ) -> (Ipv4Header, TcpHeader, Vec<u8>) {
        let ip = Ipv4Header::parse(&frame.eth_payload).unwrap();
        let tcp_start = IPV4_HDR_LEN;
        let tcp_end = ip.total_len as usize;
        let data_offset = frame.eth_payload[tcp_start + 12] >> 4;
        let tcp_hdr_end = tcp_start + 4 * data_offset as usize;
        let payload = frame.eth_payload[tcp_hdr_end..tcp_end].to_vec();
        let tcp = TcpHeader::parse(
            &frame.eth_payload[tcp_start..tcp_hdr_end],
            &payload,
            ip.src,
            server_ip,
        )
        .unwrap();
        (ip, tcp, payload)
    }

    #[test]
    fn test_should_send_syn_ack_on_inbound_syn() {
        let s = make_server();
        let frame = build_ip_tcp(
            [10, 0, 0, 1],
            [169, 254, 169, 254],
            40000,
            80,
            1000,
            0,
            TCP_SYN,
            &[],
        );
        assert!(s.handle_ip_frame([0x06, 0x00, 0xAB, 0xCD, 0xEF, 0x01], &frame));
        let out = s.drain_outbound();
        assert_eq!(out.len(), 1);
        let (_, tcp, _) = parse_outbound_tcp(&out[0], [10, 0, 0, 1]);
        assert!(tcp.is_syn());
        assert!(tcp.is_ack());
        assert_eq!(tcp.ack, 1001);
        assert_eq!(tcp.dst_port, 40000);
        assert_eq!(tcp.src_port, 80);
    }

    #[test]
    fn test_should_complete_handshake_then_serve_http_request() {
        let s = make_server();
        // SYN
        let syn = build_ip_tcp(
            [10, 0, 0, 1],
            [169, 254, 169, 254],
            40000,
            80,
            1000,
            0,
            TCP_SYN,
            &[],
        );
        assert!(s.handle_ip_frame([0x06, 0, 0, 0, 0, 0], &syn));
        let out = s.drain_outbound();
        let (_, syn_ack, _) = parse_outbound_tcp(&out[0], [10, 0, 0, 1]);
        let server_iss = syn_ack.seq;
        // ACK to complete handshake
        let ack = build_ip_tcp(
            [10, 0, 0, 1],
            [169, 254, 169, 254],
            40000,
            80,
            1001,
            server_iss + 1,
            TCP_ACK,
            &[],
        );
        s.handle_ip_frame([0u8; 6], &ack);
        // PSH+ACK with HTTP GET
        let req = b"GET /latest/meta-data/foo HTTP/1.1\r\nHost: m\r\n\r\n";
        let data = build_ip_tcp(
            [10, 0, 0, 1],
            [169, 254, 169, 254],
            40000,
            80,
            1001,
            server_iss + 1,
            TCP_ACK | TCP_PSH,
            req,
        );
        s.handle_ip_frame([0u8; 6], &data);
        let out = s.drain_outbound();
        // Two frames: bare ACK then response.
        assert!(out.len() >= 2);
        // The last frame should carry the HTTP response with FIN set.
        let last = &out[out.len() - 1];
        let (_, tcp, payload) = parse_outbound_tcp(last, [10, 0, 0, 1]);
        assert!(tcp.is_fin());
        let body_str = String::from_utf8_lossy(&payload);
        assert!(
            body_str.contains("HTTP/1.1 200 OK"),
            "expected HTTP/1.1 200, got {body_str:?}"
        );
        assert!(body_str.contains("hello GET /latest/meta-data/foo"));
    }

    #[test]
    fn test_should_reject_unknown_connection_with_rst() {
        let s = make_server();
        let stray = build_ip_tcp(
            [10, 0, 0, 1],
            [169, 254, 169, 254],
            40000,
            80,
            5000,
            6000,
            TCP_ACK,
            &[],
        );
        s.handle_ip_frame([0u8; 6], &stray);
        let out = s.drain_outbound();
        assert_eq!(out.len(), 1);
        let (_, tcp, _) = parse_outbound_tcp(&out[0], [10, 0, 0, 1]);
        assert!(tcp.is_rst());
    }

    #[test]
    fn test_should_drop_segment_when_dst_ip_is_not_server() {
        let s = make_server();
        let frame = build_ip_tcp(
            [10, 0, 0, 1],
            [10, 0, 0, 2],
            40000,
            80,
            1000,
            0,
            TCP_SYN,
            &[],
        );
        assert!(!s.handle_ip_frame([0u8; 6], &frame));
        assert!(s.drain_outbound().is_empty());
    }

    #[test]
    fn test_should_close_connection_on_rst() {
        let s = make_server();
        let syn = build_ip_tcp(
            [10, 0, 0, 1],
            [169, 254, 169, 254],
            40000,
            80,
            1000,
            0,
            TCP_SYN,
            &[],
        );
        s.handle_ip_frame([0u8; 6], &syn);
        let _ = s.drain_outbound();
        // RST closes the conn — subsequent ACKs should be answered with RST.
        let rst = build_ip_tcp(
            [10, 0, 0, 1],
            [169, 254, 169, 254],
            40000,
            80,
            1001,
            0,
            TCP_RST,
            &[],
        );
        s.handle_ip_frame([0u8; 6], &rst);
        // Another segment for the now-removed connection should produce RST.
        let stray = build_ip_tcp(
            [10, 0, 0, 1],
            [169, 254, 169, 254],
            40000,
            80,
            1001,
            1,
            TCP_ACK,
            &[],
        );
        s.handle_ip_frame([0u8; 6], &stray);
        let out = s.drain_outbound();
        let (_, tcp, _) = parse_outbound_tcp(&out[0], [10, 0, 0, 1]);
        assert!(tcp.is_rst());
    }

    #[test]
    fn test_http_parser_handles_chunked_arrival() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"GET / HTTP/1.1\r\n");
        assert!(matches!(try_parse_http(&buf), HttpParseOutcome::Incomplete));
        buf.extend_from_slice(b"Host: x\r\n\r\n");
        let outcome = try_parse_http(&buf);
        assert!(matches!(outcome, HttpParseOutcome::Complete(_)));
    }

    #[test]
    fn test_http_parser_rejects_oversized_request() {
        let mut buf = vec![b'A'; MAX_REQUEST_BYTES + 10];
        buf[..3].copy_from_slice(b"GET");
        assert!(matches!(try_parse_http(&buf), HttpParseOutcome::TooLarge));
    }
}
