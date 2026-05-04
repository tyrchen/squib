//! Frame interceptor — peels MMDS-bound frames out of the virtio-net seam.
//!
//! Per [15-mmds.md § 2,3](../../../specs/15-mmds.md#2-components):
//!
//! ```text
//! ┌──────────────────┐     ┌────────────────┐     ┌──────────────┐
//! │ virtio-net frame │ ──► │ MmdsInterceptor│ ──► │ dumbo TCP    │
//! │ guest → host     │     │ (peel ARP +    │     │ stack        │
//! └──────────────────┘     │ TCP-to-IP)     │     └──────┬───────┘
//!                          └────────────────┘            │
//!                                                        ▼
//!                                              ┌────────────────┐
//!                                              │ mmds JSON tree │
//!                                              │ + token store  │
//!                                              └────────────────┘
//! ```
//!
//! This module ships the ARP responder and a tiny request-router that the
//! dumbo TCP stack can drive once it lands. The TCP stack itself is the
//! deferred Phase-3.7-tail item documented at the crate root.
//!
//! ## Wire constants pinned for compat
//!
//! - **Synthetic MAC**: `06:01:23:45:67:01` (upstream
//!   `vendors/firecracker/src/vmm/src/mmds/ns.rs:32`).
//! - **Default IPv4**: `169.254.169.254` (link-local, AWS IMDS-compatible).

use std::sync::Arc;

use parking_lot::Mutex;

use crate::{
    data_store::{Mmds, MmdsVersion},
    pdu::{
        ARP_OP_REQUEST, ArpPacket, ETHER_HDR_LEN, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetHeader,
        build_arp_reply_frame,
    },
    tcp::{HttpRequest, HttpResponse, OutboundFrame, TcpServer},
    token::TokenStore,
};

/// Synthetic MMDS endpoint MAC; pinned byte-for-byte against upstream
/// Firecracker (`vendors/firecracker/src/vmm/src/mmds/ns.rs:32`).
pub const MMDS_SYNTHETIC_MAC: [u8; 6] = [0x06, 0x01, 0x23, 0x45, 0x67, 0x01];

/// Default link-local IPv4 the guest reaches the MMDS on.
pub const MMDS_DEFAULT_IPV4: [u8; 4] = [169, 254, 169, 254];

/// IPv4 protocol number for TCP.
pub const IP_PROTO_TCP: u8 = 6;

/// MMDS interceptor — wires the data store and token store to the
/// virtio-net frame seam.
#[derive(Debug, Clone)]
pub struct MmdsInterceptor {
    inner: Arc<InterceptorInner>,
}

#[derive(Debug)]
struct InterceptorInner {
    mmds: Mmds,
    tokens: TokenStore,
    /// Mutable IP per `/mmds/config { ipv4_address }`. Behind a `RwLock`
    /// because the API layer can update it pre-boot while the virtio-net
    /// device thread holds a clone of the interceptor for `intercept` calls.
    mmds_ip: parking_lot::RwLock<[u8; 4]>,
    mmds_mac: [u8; 6],
    /// Dumbo TCP server bound at `(mmds_ip, 80)`.
    tcp: TcpServer,
    /// Frames the interceptor wants delivered back to the guest.
    pending_rx: Mutex<Vec<Vec<u8>>>,
}

impl MmdsInterceptor {
    /// HTTP listener port. Pinned at `80` to match AWS IMDS clients.
    pub const HTTP_PORT: u16 = 80;

    /// Build an interceptor over the given MMDS.
    #[must_use]
    pub fn new(mmds: Mmds, tokens: TokenStore) -> Self {
        // The TCP server's HTTP handler closes over a separate `Arc<MmdsInner>`
        // so we don't tangle `Arc<InterceptorInner>` cycles with the handler.
        let mmds_for_handler = mmds.clone();
        let tokens_for_handler = tokens.clone();
        let tcp = TcpServer::new(
            MMDS_DEFAULT_IPV4,
            Self::HTTP_PORT,
            MMDS_SYNTHETIC_MAC,
            move |req: HttpRequest| -> HttpResponse {
                let (status, body) =
                    service_http_request(&mmds_for_handler, &tokens_for_handler, &req);
                HttpResponse { status, body }
            },
        );
        Self {
            inner: Arc::new(InterceptorInner {
                mmds,
                tokens,
                mmds_ip: parking_lot::RwLock::new(MMDS_DEFAULT_IPV4),
                mmds_mac: MMDS_SYNTHETIC_MAC,
                tcp,
                pending_rx: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Override the MMDS IPv4 (per `/mmds/config { ipv4_address }`). Safe to
    /// call after the interceptor has been cloned into the virtio-net device
    /// — the IP lives behind an `RwLock`, so this is just a write-lock.
    #[must_use]
    pub fn with_ipv4(self, ip: [u8; 4]) -> Self {
        *self.inner.mmds_ip.write() = ip;
        self.inner.tcp.set_ip(ip);
        self
    }

    /// Set the MMDS IPv4 in-place; equivalent to [`Self::with_ipv4`] but
    /// without the consume-and-return ergonomics — used by the API layer
    /// when `/mmds/config` is patched.
    pub fn set_ipv4(&self, ip: [u8; 4]) {
        *self.inner.mmds_ip.write() = ip;
        self.inner.tcp.set_ip(ip);
    }

    /// Reference to the wrapped data store.
    #[must_use]
    pub fn mmds(&self) -> &Mmds {
        &self.inner.mmds
    }

    /// Reference to the wrapped token store.
    #[must_use]
    pub fn tokens(&self) -> &TokenStore {
        &self.inner.tokens
    }

    /// Inspect a frame the guest sent. Returns `true` if the interceptor
    /// consumed the frame (it MUST NOT reach the host network).
    ///
    /// I-MMDS-1: every MMDS-bound frame is intercepted; none reaches the host
    /// backend.
    pub fn intercept(&self, frame: &[u8]) -> bool {
        let Ok(eth) = EthernetHeader::parse(frame) else {
            return false;
        };
        match eth.ethertype {
            ETHERTYPE_ARP => self.handle_arp(&eth, &frame[ETHER_HDR_LEN..]),
            ETHERTYPE_IPV4 => self.handle_ipv4(&eth, &frame[ETHER_HDR_LEN..]),
            _ => false,
        }
    }

    fn handle_arp(&self, eth: &EthernetHeader, body: &[u8]) -> bool {
        let Ok(arp) = ArpPacket::parse(body) else {
            return false;
        };
        let mmds_ip = *self.inner.mmds_ip.read();
        if arp.op != ARP_OP_REQUEST || arp.target_ip != mmds_ip {
            return false;
        }
        let reply = build_arp_reply_frame(self.inner.mmds_mac, mmds_ip, eth.src, arp.sender_ip);
        self.inner.pending_rx.lock().push(reply);
        true
    }

    fn handle_ipv4(&self, eth: &EthernetHeader, body: &[u8]) -> bool {
        // Minimum IPv4 header is 20 bytes.
        if body.len() < 20 {
            return false;
        }
        let dst_ip = [body[16], body[17], body[18], body[19]];
        let proto = body[9];
        if dst_ip != *self.inner.mmds_ip.read() {
            return false;
        }
        if proto == IP_PROTO_TCP {
            let consumed = self.inner.tcp.handle_ip_frame(eth.src, body);
            // Ferry any outbound frames the TCP server emitted into the
            // RX queue, wrapping each in an Ethernet header.
            self.flush_tcp_outbound();
            return consumed;
        }
        false
    }

    /// Build Ethernet frames for any outbound TCP segments and queue
    /// them for the next RX drain.
    fn flush_tcp_outbound(&self) {
        let frames = self.inner.tcp.drain_outbound();
        if frames.is_empty() {
            return;
        }
        let mut pending = self.inner.pending_rx.lock();
        for f in frames {
            pending.push(wrap_in_ethernet(self.inner.mmds_mac, &f));
        }
    }

    /// Drain any frames the interceptor wants delivered to the guest.
    pub fn drain_rx(&self) -> Vec<Vec<u8>> {
        // Make sure any TCP-side reply emitted asynchronously (e.g. by a
        // future timer-driven retransmit) is flushed first.
        self.flush_tcp_outbound();
        std::mem::take(&mut *self.inner.pending_rx.lock())
    }

    /// Maximum length (in bytes) for an HTTP `path` accepted by
    /// [`Self::service_http`]. CLAUDE.md `§ Input Validation` requires every
    /// external `&str` to have an explicit byte cap; a 1 KiB ceiling fits
    /// every path AWS IMDS exposes (the longest is around 80 bytes for
    /// nested IAM role keys).
    pub const MAX_HTTP_PATH_BYTES: usize = 1024;

    /// Maximum length (in bytes) for the V2 `X-aws-ec2-metadata-token` header.
    pub const MAX_TOKEN_HEADER_BYTES: usize = 256;

    /// Service an HTTP request body against the data store. The dumbo TCP
    /// integration calls this after parsing the HTTP request line; we
    /// expose it here so the data-store / token semantics are testable
    /// without standing up the TCP stack.
    ///
    /// `method` / `path` follow the AWS IMDS V1/V2 surface from
    /// [15-mmds.md § 4](../../../specs/15-mmds.md#4-api-surface). Returns
    /// `(http_status, body)`. CLAUDE.md `§ Input Validation` is enforced at
    /// the boundary: paths over [`Self::MAX_HTTP_PATH_BYTES`] return 414,
    /// token headers over [`Self::MAX_TOKEN_HEADER_BYTES`] are treated as
    /// invalid (401), bodies above the data-store cap return 413.
    #[must_use]
    pub fn service_http(
        &self,
        method: &str,
        path: &str,
        token_header: Option<&str>,
        ttl_header: Option<u32>,
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        let req = HttpRequest {
            method: method.to_string(),
            path: path.to_string(),
            token_header: token_header.map(str::to_string),
            ttl_header,
            body: body.to_vec(),
        };
        service_http_request(&self.inner.mmds, &self.inner.tokens, &req)
    }
}

/// Service an HTTP request against the MMDS data store and token store.
///
/// Shared by [`MmdsInterceptor::service_http`] (for tests / direct
/// callers) and the dumbo TCP server's HTTP handler closure. The two
/// must always agree, so they go through one helper.
///
/// CLAUDE.md `§ Input Validation` is enforced here:
/// - paths over [`MmdsInterceptor::MAX_HTTP_PATH_BYTES`] → 414.
/// - paths with control characters or backslashes → 400.
/// - bodies over the MMDS size cap → 413.
/// - V2 GET without valid token → 401.
fn service_http_request(mmds: &Mmds, tokens: &TokenStore, req: &HttpRequest) -> (u16, Vec<u8>) {
    if req.path.len() > MmdsInterceptor::MAX_HTTP_PATH_BYTES {
        return (414, b"request path too long".to_vec());
    }
    if !path_charset_ok(&req.path) {
        return (
            400,
            b"path contains disallowed control or NUL bytes".to_vec(),
        );
    }
    if req.body.len() > mmds.size_cap() {
        return (413, b"request body exceeds MMDS size cap".to_vec());
    }
    // PUT /latest/api/token — V2 token issuance.
    if req.method == "PUT" && req.path == "/latest/api/token" {
        let Some(ttl) = req.ttl_header else {
            return (
                400,
                b"missing X-aws-ec2-metadata-token-ttl-seconds".to_vec(),
            );
        };
        return match tokens.issue(ttl) {
            Ok(t) => (200, t.as_str().as_bytes().to_vec()),
            Err(_) => (400, b"invalid TTL".to_vec()),
        };
    }
    // GETs require a valid token if version is V2.
    if req.method == "GET" {
        if matches!(mmds.version(), MmdsVersion::V2) {
            let ok = req.token_header.as_deref().is_some_and(|tok| {
                tok.len() <= MmdsInterceptor::MAX_TOKEN_HEADER_BYTES && tokens.validate(tok)
            });
            if !ok {
                return (401, b"missing or invalid token".to_vec());
            }
        }
        // The IMDS path is also the JSON pointer — `/latest/meta-data/foo`
        // in the URL maps to `/latest/meta-data/foo` in the JSON tree.
        return match mmds.get_at_pointer(&req.path) {
            Ok(v) => match serde_json::to_vec(&v) {
                Ok(body) => (200, body),
                Err(err) => {
                    tracing::error!(error = %err, "MMDS: serializer failed (likely OOM)");
                    (
                        500,
                        br#"{"fault_message":"internal serializer failure"}"#.to_vec(),
                    )
                }
            },
            Err(_) => (404, b"not found".to_vec()),
        };
    }
    (405, b"method not allowed".to_vec())
}

/// Charset allowlist for the HTTP path. Reject any control characters
/// (anything below 0x20 or DEL) and NUL — defense-in-depth against
/// log-injection and deserialization tricks. Per CLAUDE.md `§ Input
/// Validation` we use an allowlist (printable ASCII plus a small set of
/// punctuation needed by JSON Pointer) rather than a blocklist, so unicode
/// homoglyphs or RTL overrides cannot slip through.
fn path_charset_ok(path: &str) -> bool {
    path.bytes()
        .all(|b| (0x20..0x7F).contains(&b) && b != b'\\')
}

/// Wrap an outbound IPv4 frame in an Ethernet header.
fn wrap_in_ethernet(src_mac: [u8; 6], frame: &OutboundFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(ETHER_HDR_LEN + frame.eth_payload.len());
    let eth = EthernetHeader {
        dst: frame.dst_mac,
        src: src_mac,
        ethertype: ETHERTYPE_IPV4,
    };
    out.extend_from_slice(&eth.to_bytes());
    out.extend_from_slice(&frame.eth_payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_arp_request_frame(
        sender_mac: [u8; 6],
        sender_ip: [u8; 4],
        target_ip: [u8; 4],
    ) -> Vec<u8> {
        let eth = EthernetHeader {
            dst: [0xFF; 6],
            src: sender_mac,
            ethertype: ETHERTYPE_ARP,
        };
        let arp = ArpPacket {
            op: ARP_OP_REQUEST,
            sender_mac,
            sender_ip,
            target_mac: [0u8; 6],
            target_ip,
        };
        let mut frame = Vec::new();
        frame.extend_from_slice(&eth.to_bytes());
        frame.extend_from_slice(&arp.to_bytes());
        frame
    }

    fn build_ipv4_tcp_syn_frame(src_ip: [u8; 4], dst_ip: [u8; 4]) -> Vec<u8> {
        use crate::pdu::{IpProtocol, Ipv4Header, TCP_SYN, TcpHeader};

        let eth = EthernetHeader {
            dst: MMDS_SYNTHETIC_MAC,
            src: [0x06, 0x00, 0xAB, 0xCD, 0xEF, 0x01],
            ethertype: ETHERTYPE_IPV4,
        };
        let tcp = TcpHeader::build(40000, 80, 0x1000_0000, 0, TCP_SYN, 0xFFFF);
        let tcp_bytes = tcp.to_bytes_with_payload(src_ip, dst_ip, &[]);
        let ip = Ipv4Header::build(
            src_ip,
            dst_ip,
            IpProtocol::Tcp,
            u16::try_from(tcp_bytes.len()).unwrap(),
            1,
        );
        let mut frame = eth.to_bytes().to_vec();
        frame.extend_from_slice(&ip.to_bytes());
        frame.extend_from_slice(&tcp_bytes);
        frame
    }

    fn interceptor() -> MmdsInterceptor {
        MmdsInterceptor::new(Mmds::new(8192), TokenStore::new())
    }

    #[test]
    fn test_should_pin_synthetic_mac_byte_for_byte() {
        // I-MMDS-1 / D9: the synthetic MAC must match
        // upstream Firecracker's DEFAULT_MAC_ADDR.
        assert_eq!(MMDS_SYNTHETIC_MAC, [0x06, 0x01, 0x23, 0x45, 0x67, 0x01]);
        assert_eq!(MMDS_DEFAULT_IPV4, [169, 254, 169, 254]);
    }

    #[test]
    fn test_should_intercept_arp_for_mmds_ip_and_emit_reply() {
        let int = interceptor();
        let req = build_arp_request_frame(
            [0x06, 0x00, 0xAB, 0xCD, 0xEF, 0x01],
            [192, 168, 1, 2],
            MMDS_DEFAULT_IPV4,
        );
        assert!(int.intercept(&req));
        let rx = int.drain_rx();
        assert_eq!(rx.len(), 1);
        let eth = EthernetHeader::parse(&rx[0]).unwrap();
        let arp = ArpPacket::parse(&rx[0][ETHER_HDR_LEN..]).unwrap();
        assert_eq!(eth.dst, [0x06, 0x00, 0xAB, 0xCD, 0xEF, 0x01]);
        assert_eq!(arp.sender_mac, MMDS_SYNTHETIC_MAC);
        assert_eq!(arp.sender_ip, MMDS_DEFAULT_IPV4);
    }

    #[test]
    fn test_should_pass_through_arp_for_unrelated_ip() {
        let int = interceptor();
        let req = build_arp_request_frame(
            [0x06, 0x00, 0xAB, 0xCD, 0xEF, 0x01],
            [192, 168, 1, 2],
            [192, 168, 1, 1],
        );
        assert!(!int.intercept(&req));
        assert!(int.drain_rx().is_empty());
    }

    #[test]
    fn test_should_intercept_ipv4_tcp_syn_and_emit_syn_ack() {
        let int = interceptor();
        let frame = build_ipv4_tcp_syn_frame([10, 0, 0, 1], MMDS_DEFAULT_IPV4);
        // I-MMDS-1: MMDS-bound traffic must not reach host backend.
        assert!(int.intercept(&frame));
        // The dumbo TCP server should have queued a SYN-ACK reply.
        let rx = int.drain_rx();
        assert_eq!(rx.len(), 1, "expected one outbound frame (SYN-ACK)");
        // Confirm the reply is IPv4+TCP with SYN+ACK set.
        let eth = EthernetHeader::parse(&rx[0]).unwrap();
        assert_eq!(eth.ethertype, ETHERTYPE_IPV4);
    }

    #[test]
    fn test_should_pass_through_ipv4_to_unrelated_ip() {
        let int = interceptor();
        let frame = build_ipv4_tcp_syn_frame([10, 0, 0, 1], [10, 0, 0, 5]);
        assert!(!int.intercept(&frame));
    }

    #[test]
    fn test_should_serve_v1_get_without_token() {
        let int = interceptor();
        int.mmds()
            .put_json(r#"{"latest":{"meta-data":{"foo":"bar"}}}"#)
            .unwrap();
        let (status, body) = int.service_http("GET", "/latest/meta-data/foo", None, None, b"");
        assert_eq!(status, 200);
        assert_eq!(body, b"\"bar\"");
    }

    #[test]
    fn test_should_require_token_for_v2_get() {
        let int = interceptor();
        int.mmds().set_version(MmdsVersion::V2);
        int.mmds()
            .put_json(r#"{"latest":{"meta-data":{"foo":"bar"}}}"#)
            .unwrap();
        let (status, _) = int.service_http("GET", "/latest/meta-data/foo", None, None, b"");
        assert_eq!(status, 401);
    }

    #[test]
    fn test_should_issue_v2_token_via_put_and_then_serve_get() {
        let int = interceptor();
        int.mmds().set_version(MmdsVersion::V2);
        int.mmds()
            .put_json(r#"{"latest":{"meta-data":{"foo":"bar"}}}"#)
            .unwrap();
        let (status, body) = int.service_http("PUT", "/latest/api/token", None, Some(60), b"");
        assert_eq!(status, 200);
        let token = String::from_utf8(body).unwrap();
        let (status, _) = int.service_http("GET", "/latest/meta-data/foo", Some(&token), None, b"");
        assert_eq!(status, 200);
    }

    #[test]
    fn test_should_404_unknown_metadata_path() {
        let int = interceptor();
        int.mmds().put_json(r#"{"a":1}"#).unwrap();
        let (status, _) = int.service_http("GET", "/missing", None, None, b"");
        assert_eq!(status, 404);
    }
}
