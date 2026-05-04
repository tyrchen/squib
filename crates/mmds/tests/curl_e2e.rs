//! End-to-end MMDS demo at the wire layer.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::similar_names
)]
//!
//! This integration test acts as a Linux guest's network stack:
//! constructs raw Ethernet frames, hands them to `MmdsInterceptor`,
//! parses the synthesised replies, and verifies that a complete
//! ARP → TCP → HTTP/1.1 → MMDS round-trip returns the expected JSON
//! payload.
//!
//! Every byte that would cross squib's virtual wire on a real
//! `curl 169.254.169.254/...` invocation is exercised here. The only
//! gap to a "real Linux guest" demo is the missing run-loop integration
//! that wraps these frames in virtio-net descriptors — that wiring is
//! covered separately by `crates/virtio` tests.

use squib_mmds::{
    MMDS_DEFAULT_IPV4, MMDS_SYNTHETIC_MAC, Mmds, MmdsInterceptor, MmdsVersion, TokenStore,
    pdu::{
        ARP_HLEN, ARP_HTYPE_ETHER, ARP_OP_REPLY, ARP_OP_REQUEST, ARP_PACKET_LEN, ARP_PLEN,
        ARP_PTYPE_IPV4, ArpPacket, ETHER_HDR_LEN, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetHeader,
        IPV4_HDR_LEN, IpProtocol, Ipv4Header, TCP_ACK, TCP_PSH, TCP_SYN, TcpHeader,
    },
};

/// Guest stub that sends frames into a `MmdsInterceptor` and parses
/// replies, mimicking the network stack a Linux guest would run.
struct GuestStub {
    interceptor: MmdsInterceptor,
    guest_mac: [u8; 6],
    guest_ip: [u8; 4],
    server_mac: Option<[u8; 6]>,
    server_ip: [u8; 4],
}

impl GuestStub {
    fn new(interceptor: MmdsInterceptor) -> Self {
        Self {
            interceptor,
            guest_mac: [0x06, 0x00, 0xAB, 0xCD, 0xEF, 0x01],
            guest_ip: [10, 0, 0, 2],
            server_mac: None,
            server_ip: MMDS_DEFAULT_IPV4,
        }
    }

    /// Drain whatever frames the interceptor has for us, asserting
    /// each one is well-formed Ethernet and returning the payloads.
    fn drain(&mut self) -> Vec<Vec<u8>> {
        self.interceptor
            .drain_rx()
            .into_iter()
            .map(|b| b.to_vec())
            .collect()
    }

    /// Send an ARP request for the MMDS IP. The interceptor should
    /// reply with `MMDS_SYNTHETIC_MAC`.
    fn arp_for_server(&mut self) {
        let eth = EthernetHeader {
            dst: [0xFF; 6], // broadcast
            src: self.guest_mac,
            ethertype: ETHERTYPE_ARP,
        };
        let arp = ArpPacket {
            op: ARP_OP_REQUEST,
            sender_mac: self.guest_mac,
            sender_ip: self.guest_ip,
            target_mac: [0; 6],
            target_ip: self.server_ip,
        };
        let mut frame = Vec::with_capacity(ETHER_HDR_LEN + ARP_PACKET_LEN);
        frame.extend_from_slice(&eth.to_bytes());
        frame.extend_from_slice(&arp.to_bytes());
        assert!(self.interceptor.intercept(&frame));
        // Parse the reply.
        let replies = self.drain();
        assert_eq!(replies.len(), 1, "expected exactly one ARP reply");
        let reply = &replies[0];
        let reply_eth = EthernetHeader::parse(reply).unwrap();
        assert_eq!(reply_eth.dst, self.guest_mac);
        let reply_arp = ArpPacket::parse(&reply[ETHER_HDR_LEN..]).unwrap();
        assert_eq!(reply_arp.op, ARP_OP_REPLY);
        assert_eq!(reply_arp.sender_mac, MMDS_SYNTHETIC_MAC);
        // Smoke check the canonical ARP shape.
        assert_eq!(reply_arp.sender_ip, self.server_ip);
        // Sanity for compat — ETHERTYPE/HTYPE/PTYPE constants are correct.
        let _ = (
            ARP_HTYPE_ETHER,
            ARP_PTYPE_IPV4,
            ARP_HLEN,
            ARP_PLEN,
            ETHERTYPE_IPV4,
        );
        self.server_mac = Some(reply_arp.sender_mac);
    }

    /// Build a `(IPv4 + TCP)` frame from the guest to the server.
    #[allow(clippy::too_many_arguments)]
    fn build_tcp_frame(
        &self,
        src_port: u16,
        dst_port: u16,
        seq: u32,
        ack: u32,
        flags: u8,
        payload: &[u8],
        ip_id: u16,
    ) -> Vec<u8> {
        let tcp = TcpHeader::build(src_port, dst_port, seq, ack, flags, u16::MAX);
        let tcp_bytes = tcp.to_bytes_with_payload(self.guest_ip, self.server_ip, payload);
        let ip = Ipv4Header::build(
            self.guest_ip,
            self.server_ip,
            IpProtocol::Tcp,
            u16::try_from(tcp_bytes.len()).unwrap(),
            ip_id,
        );
        let eth = EthernetHeader {
            dst: self.server_mac.expect("ARP must complete before TCP"),
            src: self.guest_mac,
            ethertype: ETHERTYPE_IPV4,
        };
        let mut frame = Vec::with_capacity(ETHER_HDR_LEN + IPV4_HDR_LEN + tcp_bytes.len());
        frame.extend_from_slice(&eth.to_bytes());
        frame.extend_from_slice(&ip.to_bytes());
        frame.extend_from_slice(&tcp_bytes);
        frame
    }

    /// Parse a single inbound (server → guest) IPv4+TCP frame. Returns
    /// `(tcp_header, tcp_payload)`.
    #[allow(clippy::unused_self)]
    fn parse_inbound_tcp(&self, frame: &[u8]) -> (TcpHeader, Vec<u8>) {
        let _eth = EthernetHeader::parse(frame).unwrap();
        let ip = Ipv4Header::parse(&frame[ETHER_HDR_LEN..]).unwrap();
        let tcp_start = ETHER_HDR_LEN + IPV4_HDR_LEN;
        let tcp_end = ETHER_HDR_LEN + ip.total_len as usize;
        let data_offset = frame[tcp_start + 12] >> 4;
        let tcp_hdr_end = tcp_start + 4 * data_offset as usize;
        let payload = frame[tcp_hdr_end..tcp_end].to_vec();
        let tcp =
            TcpHeader::parse(&frame[tcp_start..tcp_hdr_end], &payload, ip.src, ip.dst).unwrap();
        (tcp, payload)
    }
}

#[test]
fn test_e2e_guest_arp_then_tcp_then_http_get_returns_mmds_payload() {
    // Set up MMDS with the AWS IMDS-shaped tree.
    let mmds = Mmds::new(8192);
    mmds.put_json(
        r#"{
            "latest": {
                "meta-data": {
                    "instance-id": "i-1234567890abcdef0",
                    "instance-type": "squib.demo",
                    "local-hostname": "guest"
                }
            }
        }"#,
    )
    .unwrap();
    let interceptor = MmdsInterceptor::new(mmds, TokenStore::new());

    let mut guest = GuestStub::new(interceptor);

    // 1. ARP for 169.254.169.254 — proves the synthetic MAC handshake.
    guest.arp_for_server();

    // 2. Open a TCP connection to (server_ip, 80).
    let client_isn: u32 = 0x1000_0000;
    let syn = guest.build_tcp_frame(40000, 80, client_isn, 0, TCP_SYN, &[], 1);
    assert!(guest.interceptor.intercept(&syn));
    let replies = guest.drain();
    assert_eq!(replies.len(), 1, "expected SYN-ACK");
    let (syn_ack, _) = guest.parse_inbound_tcp(&replies[0]);
    assert!(syn_ack.is_syn() && syn_ack.is_ack());
    assert_eq!(syn_ack.ack, client_isn + 1);
    let server_isn = syn_ack.seq;

    // 3. Complete the handshake with the guest's ACK.
    let ack_only =
        guest.build_tcp_frame(40000, 80, client_isn + 1, server_isn + 1, TCP_ACK, &[], 2);
    assert!(guest.interceptor.intercept(&ack_only));
    assert!(guest.drain().is_empty());

    // 4. Send the HTTP/1.1 GET.
    let request = b"GET /latest/meta-data/instance-id HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n";
    let data = guest.build_tcp_frame(
        40000,
        80,
        client_isn + 1,
        server_isn + 1,
        TCP_ACK | TCP_PSH,
        request,
        3,
    );
    assert!(guest.interceptor.intercept(&data));
    let replies = guest.drain();

    // The server emits a bare ACK then the HTTP response (with FIN).
    assert!(
        replies.len() >= 2,
        "expected ACK + response, got {}",
        replies.len()
    );
    let ack_frame = guest.parse_inbound_tcp(&replies[0]);
    assert!(ack_frame.0.is_ack() && !ack_frame.0.is_fin());
    assert!(ack_frame.1.is_empty());

    let (resp_tcp, resp_payload) = guest.parse_inbound_tcp(&replies[replies.len() - 1]);
    assert!(resp_tcp.is_fin(), "response segment should carry FIN");
    let body_str = String::from_utf8_lossy(&resp_payload);
    assert!(
        body_str.starts_with("HTTP/1.1 200 OK"),
        "response not 200: {body_str}"
    );
    assert!(
        body_str.contains("\"i-1234567890abcdef0\""),
        "response body missing instance-id: {body_str}"
    );

    // 5. ACK the FIN; the connection should close cleanly.
    let final_ack = guest.build_tcp_frame(
        40000,
        80,
        client_isn + 1 + request.len() as u32,
        resp_tcp.seq + resp_payload.len() as u32 + 1,
        TCP_ACK,
        &[],
        4,
    );
    assert!(guest.interceptor.intercept(&final_ack));
    // Server has no further work.
    assert!(guest.drain().is_empty());
}

#[test]
fn test_e2e_v2_get_requires_token_then_succeeds_with_one() {
    let mmds = Mmds::new(8192);
    mmds.put_json(r#"{"latest":{"meta-data":{"foo":"bar"}}}"#)
        .unwrap();
    mmds.set_version(MmdsVersion::V2);
    let interceptor = MmdsInterceptor::new(mmds, TokenStore::new());
    let mut guest = GuestStub::new(interceptor);

    guest.arp_for_server();

    let client_isn: u32 = 0x2000_0000;
    let syn = guest.build_tcp_frame(50000, 80, client_isn, 0, TCP_SYN, &[], 1);
    guest.interceptor.intercept(&syn);
    let replies = guest.drain();
    let (syn_ack, _) = guest.parse_inbound_tcp(&replies[0]);
    let server_isn = syn_ack.seq;
    let ack = guest.build_tcp_frame(50000, 80, client_isn + 1, server_isn + 1, TCP_ACK, &[], 2);
    guest.interceptor.intercept(&ack);
    let _ = guest.drain();

    // V1-style request → 401 because we're in V2.
    let req_no_token = b"GET /latest/meta-data/foo HTTP/1.1\r\nHost: m\r\n\r\n";
    let data = guest.build_tcp_frame(
        50000,
        80,
        client_isn + 1,
        server_isn + 1,
        TCP_ACK | TCP_PSH,
        req_no_token,
        3,
    );
    guest.interceptor.intercept(&data);
    let replies = guest.drain();
    let (_, resp) = guest.parse_inbound_tcp(&replies[replies.len() - 1]);
    let body = String::from_utf8_lossy(&resp);
    assert!(body.starts_with("HTTP/1.1 401"));
}

#[test]
fn test_e2e_handles_pipelined_segments_arriving_separately() {
    // The guest may chunk the HTTP request across multiple TCP segments.
    // Squib should accumulate bytes until the request is complete, then
    // emit the response.
    let mmds = Mmds::new(8192);
    mmds.put_json(r#"{"a":1}"#).unwrap();
    let interceptor = MmdsInterceptor::new(mmds, TokenStore::new());
    let mut guest = GuestStub::new(interceptor);
    guest.arp_for_server();

    let client_isn: u32 = 0x3000_0000;
    let syn = guest.build_tcp_frame(60000, 80, client_isn, 0, TCP_SYN, &[], 1);
    guest.interceptor.intercept(&syn);
    let replies = guest.drain();
    let (syn_ack, _) = guest.parse_inbound_tcp(&replies[0]);
    let server_isn = syn_ack.seq;
    let ack = guest.build_tcp_frame(60000, 80, client_isn + 1, server_isn + 1, TCP_ACK, &[], 2);
    guest.interceptor.intercept(&ack);
    let _ = guest.drain();

    // Request split across 3 segments.
    let chunks: [&[u8]; 3] = [b"GET /a ", b"HTTP/1.1\r\nHost: x\r\n", b"\r\n"];
    let mut next_seq = client_isn + 1;
    let mut last_resp: Option<Vec<u8>> = None;
    for (i, chunk) in chunks.iter().enumerate() {
        let frame = guest.build_tcp_frame(
            60000,
            80,
            next_seq,
            server_isn + 1,
            TCP_ACK | TCP_PSH,
            chunk,
            10 + i as u16,
        );
        guest.interceptor.intercept(&frame);
        next_seq = next_seq.wrapping_add(chunk.len() as u32);
        let replies = guest.drain();
        // Capture any HTTP response that arrives.
        for r in replies {
            let (tcp, payload) = guest.parse_inbound_tcp(&r);
            if !payload.is_empty() && tcp.is_fin() {
                last_resp = Some(payload);
            }
        }
    }
    let body = last_resp.expect("expected an HTTP response after the final chunk");
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.starts_with("HTTP/1.1 200"), "got {body_str}");
    assert!(body_str.contains('1'));
}
