//! PDU helpers — Ethernet, ARP, IPv4, TCP parsing and emission.
//!
//! Per [15-mmds.md § 3](../../../specs/15-mmds.md#3-packet-interception):
//!
//! > 1. Guest emits ARP for `169.254.169.254` → `MmdsInterceptor` answers
//! > with the synthetic MAC **`06:01:23:45:67:01`** (upstream
//! > `DEFAULT_MAC_ADDR` from `vendors/firecracker/src/vmm/src/mmds/ns.rs:32`;
//! > pinned byte-for-byte for compat).
//!
//! ## Modules
//!
//! - Top of file: Ethernet header + ARP packet helpers.
//! - [`ipv4`]: IPv4 header parse / build with checksum.
//! - [`tcp`]: TCP header parse / build with TCP-pseudo-header checksum.
//!
//! All checksums use RFC 1071 ones-complement folding. Headers are owned
//! by-value (`#[derive(Clone, Copy)]`); buffer reuse is the caller's
//! responsibility.

/// Ethernet header length (no 802.1Q VLAN tagging).
pub const ETHER_HDR_LEN: usize = 14;
/// `EtherType` for `IPv4`.
pub const ETHERTYPE_IPV4: u16 = 0x0800;
/// `EtherType` for ARP.
pub const ETHERTYPE_ARP: u16 = 0x0806;

/// ARP packet length on Ethernet/IPv4.
pub const ARP_PACKET_LEN: usize = 28;
/// ARP hardware type — Ethernet.
pub const ARP_HTYPE_ETHER: u16 = 1;
/// ARP protocol type — IPv4.
pub const ARP_PTYPE_IPV4: u16 = 0x0800;
/// ARP hardware-address length.
pub const ARP_HLEN: u8 = 6;
/// ARP protocol-address length (IPv4).
pub const ARP_PLEN: u8 = 4;
/// ARP operation — request.
pub const ARP_OP_REQUEST: u16 = 1;
/// ARP operation — reply.
pub const ARP_OP_REPLY: u16 = 2;

/// Errors produced by PDU parsing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PduError {
    /// Input shorter than the protocol's minimum frame length.
    #[error("PDU too short")]
    TooShort,
    /// Recognised header field carries an unsupported value (e.g. ARP for IPv6).
    #[error("PDU has unsupported field: {0}")]
    Unsupported(&'static str),
    /// Computed checksum does not match the on-wire checksum.
    #[error("PDU checksum mismatch")]
    BadChecksum,
}

pub mod ipv4;
pub mod tcp;

pub use ipv4::{IPV4_HDR_LEN, IpProtocol, Ipv4Header};
pub use tcp::{TCP_ACK, TCP_FIN, TCP_PSH, TCP_RST, TCP_SYN, TcpHeader};

/// RFC 1071 ones-complement folding checksum.
///
/// Used for IPv4, TCP (over a pseudo-header), ICMP, UDP. The caller
/// passes the bytes to be summed; partial sums are in 1's-complement
/// 16-bit arithmetic.
#[must_use]
pub fn ones_complement_sum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        let word = u16::from_be_bytes([bytes[i], bytes[i + 1]]);
        sum = sum.wrapping_add(u32::from(word));
        i += 2;
    }
    if i < bytes.len() {
        // Odd-length tail: pad with a zero byte on the right.
        let word = u16::from_be_bytes([bytes[i], 0]);
        sum = sum.wrapping_add(u32::from(word));
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Parsed Ethernet header.
#[derive(Debug, Clone, Copy)]
pub struct EthernetHeader {
    /// Destination MAC.
    pub dst: [u8; 6],
    /// Source MAC.
    pub src: [u8; 6],
    /// Frame type (`ETHERTYPE_IPV4`, `ETHERTYPE_ARP`, …).
    pub ethertype: u16,
}

impl EthernetHeader {
    /// Parse the first 14 bytes of `frame`.
    ///
    /// # Errors
    /// [`PduError::TooShort`] if `frame.len() < 14`.
    pub fn parse(frame: &[u8]) -> Result<Self, PduError> {
        if frame.len() < ETHER_HDR_LEN {
            return Err(PduError::TooShort);
        }
        let mut dst = [0u8; 6];
        let mut src = [0u8; 6];
        dst.copy_from_slice(&frame[0..6]);
        src.copy_from_slice(&frame[6..12]);
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        Ok(Self {
            dst,
            src,
            ethertype,
        })
    }

    /// Serialise into 14 bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; ETHER_HDR_LEN] {
        let mut out = [0u8; ETHER_HDR_LEN];
        out[0..6].copy_from_slice(&self.dst);
        out[6..12].copy_from_slice(&self.src);
        out[12..14].copy_from_slice(&self.ethertype.to_be_bytes());
        out
    }
}

/// Parsed ARP packet (Ethernet/IPv4 only).
#[derive(Debug, Clone, Copy)]
pub struct ArpPacket {
    /// `ARP_OP_REQUEST` or `ARP_OP_REPLY`.
    pub op: u16,
    /// Sender hardware address.
    pub sender_mac: [u8; 6],
    /// Sender protocol address (IPv4).
    pub sender_ip: [u8; 4],
    /// Target hardware address.
    pub target_mac: [u8; 6],
    /// Target protocol address (IPv4).
    pub target_ip: [u8; 4],
}

impl ArpPacket {
    /// Parse a 28-byte ARP body sitting after the Ethernet header.
    ///
    /// # Errors
    /// - [`PduError::TooShort`] if the input is shorter than 28 bytes.
    /// - [`PduError::Unsupported`] if the hardware/protocol type is not Ethernet/IPv4.
    pub fn parse(arp_body: &[u8]) -> Result<Self, PduError> {
        if arp_body.len() < ARP_PACKET_LEN {
            return Err(PduError::TooShort);
        }
        let htype = u16::from_be_bytes([arp_body[0], arp_body[1]]);
        let ptype = u16::from_be_bytes([arp_body[2], arp_body[3]]);
        let hlen = arp_body[4];
        let plen = arp_body[5];
        let op = u16::from_be_bytes([arp_body[6], arp_body[7]]);
        if htype != ARP_HTYPE_ETHER {
            return Err(PduError::Unsupported("ARP htype is not Ethernet"));
        }
        if ptype != ARP_PTYPE_IPV4 {
            return Err(PduError::Unsupported("ARP ptype is not IPv4"));
        }
        if hlen != ARP_HLEN || plen != ARP_PLEN {
            return Err(PduError::Unsupported("ARP hlen/plen mismatch"));
        }
        let mut sender_mac = [0u8; 6];
        sender_mac.copy_from_slice(&arp_body[8..14]);
        let mut sender_ip = [0u8; 4];
        sender_ip.copy_from_slice(&arp_body[14..18]);
        let mut target_mac = [0u8; 6];
        target_mac.copy_from_slice(&arp_body[18..24]);
        let mut target_ip = [0u8; 4];
        target_ip.copy_from_slice(&arp_body[24..28]);
        Ok(Self {
            op,
            sender_mac,
            sender_ip,
            target_mac,
            target_ip,
        })
    }

    /// Serialise the ARP body (28 bytes, no Ethernet header).
    #[must_use]
    pub fn to_bytes(&self) -> [u8; ARP_PACKET_LEN] {
        let mut out = [0u8; ARP_PACKET_LEN];
        out[0..2].copy_from_slice(&ARP_HTYPE_ETHER.to_be_bytes());
        out[2..4].copy_from_slice(&ARP_PTYPE_IPV4.to_be_bytes());
        out[4] = ARP_HLEN;
        out[5] = ARP_PLEN;
        out[6..8].copy_from_slice(&self.op.to_be_bytes());
        out[8..14].copy_from_slice(&self.sender_mac);
        out[14..18].copy_from_slice(&self.sender_ip);
        out[18..24].copy_from_slice(&self.target_mac);
        out[24..28].copy_from_slice(&self.target_ip);
        out
    }
}

/// Compose a complete Ethernet frame for an ARP reply.
///
/// `from_mac` / `from_ip` is the synthetic MMDS endpoint (6,1,23,45,67,01 /
/// 169.254.169.254 by default); `to_mac` / `to_ip` is the requesting guest.
#[must_use]
pub fn build_arp_reply_frame(
    from_mac: [u8; 6],
    from_ip: [u8; 4],
    to_mac: [u8; 6],
    to_ip: [u8; 4],
) -> Vec<u8> {
    let eth = EthernetHeader {
        dst: to_mac,
        src: from_mac,
        ethertype: ETHERTYPE_ARP,
    };
    let arp = ArpPacket {
        op: ARP_OP_REPLY,
        sender_mac: from_mac,
        sender_ip: from_ip,
        target_mac: to_mac,
        target_ip: to_ip,
    };
    let mut out = Vec::with_capacity(ETHER_HDR_LEN + ARP_PACKET_LEN);
    out.extend_from_slice(&eth.to_bytes());
    out.extend_from_slice(&arp.to_bytes());
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
            dst: [0xFF; 6], // broadcast
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

    #[test]
    fn test_should_round_trip_ethernet_header() {
        let h = EthernetHeader {
            dst: [0x06, 0x01, 0x23, 0x45, 0x67, 0x01],
            src: [0x06, 0x00, 0xAA, 0xBB, 0xCC, 0xDD],
            ethertype: ETHERTYPE_ARP,
        };
        let parsed = EthernetHeader::parse(&h.to_bytes()).unwrap();
        assert_eq!(parsed.dst, h.dst);
        assert_eq!(parsed.src, h.src);
        assert_eq!(parsed.ethertype, h.ethertype);
    }

    #[test]
    fn test_should_reject_short_frame_for_eth_parse() {
        assert!(EthernetHeader::parse(&[0u8; 13]).is_err());
    }

    #[test]
    fn test_should_round_trip_arp_packet() {
        let a = ArpPacket {
            op: ARP_OP_REQUEST,
            sender_mac: [1, 2, 3, 4, 5, 6],
            sender_ip: [169, 254, 1, 2],
            target_mac: [0; 6],
            target_ip: [169, 254, 169, 254],
        };
        let parsed = ArpPacket::parse(&a.to_bytes()).unwrap();
        assert_eq!(parsed.op, a.op);
        assert_eq!(parsed.sender_mac, a.sender_mac);
        assert_eq!(parsed.target_ip, a.target_ip);
    }

    #[test]
    fn test_should_reject_arp_with_unexpected_htype() {
        let mut bytes = ArpPacket {
            op: ARP_OP_REQUEST,
            sender_mac: [0; 6],
            sender_ip: [0; 4],
            target_mac: [0; 6],
            target_ip: [0; 4],
        }
        .to_bytes();
        bytes[1] = 0xFF; // bogus htype
        assert!(ArpPacket::parse(&bytes).is_err());
    }

    #[test]
    fn test_should_build_arp_reply_with_swapped_addresses() {
        let req = build_arp_request_frame(
            [0x06, 0x00, 0xAB, 0xCD, 0xEF, 0x01],
            [192, 168, 1, 2],
            [169, 254, 169, 254],
        );
        let req_eth = EthernetHeader::parse(&req).unwrap();
        let req_arp = ArpPacket::parse(&req[ETHER_HDR_LEN..]).unwrap();
        let reply = build_arp_reply_frame(
            [0x06, 0x01, 0x23, 0x45, 0x67, 0x01],
            req_arp.target_ip,
            req_eth.src,
            req_arp.sender_ip,
        );
        let reply_eth = EthernetHeader::parse(&reply).unwrap();
        let reply_arp = ArpPacket::parse(&reply[ETHER_HDR_LEN..]).unwrap();
        assert_eq!(reply_eth.dst, req_eth.src);
        assert_eq!(reply_arp.op, ARP_OP_REPLY);
        assert_eq!(reply_arp.sender_mac, [0x06, 0x01, 0x23, 0x45, 0x67, 0x01]);
        assert_eq!(reply_arp.sender_ip, [169, 254, 169, 254]);
        assert_eq!(reply_arp.target_mac, req_eth.src);
    }
}
