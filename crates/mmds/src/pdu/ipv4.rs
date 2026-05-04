//! IPv4 header parse / build with RFC 791 checksum.
//!
//! Squib's MMDS endpoint terminates a single IPv4 flow at
//! `169.254.169.254`. We do not implement options, fragmentation, or
//! IGMP — the dumbo TCP server runs RFC 793 over plain RFC 791 IPv4 with
//! a fixed 20-byte header.

use super::{PduError, ones_complement_sum};

/// IPv4 header length without options (the only shape squib emits).
pub const IPV4_HDR_LEN: usize = 20;

/// Default TTL for synthesised packets. Link-local replies don't need to
/// traverse routers; `64` is the typical Linux default and what
/// upstream Firecracker uses.
pub const DEFAULT_TTL: u8 = 64;

/// IPv4 protocol numbers — only the ones squib actually inspects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IpProtocol {
    /// Internet Control Message Protocol (ICMP).
    Icmp = 1,
    /// Transmission Control Protocol (TCP).
    Tcp = 6,
    /// User Datagram Protocol (UDP) — passed through but not handled by
    /// dumbo.
    Udp = 17,
}

impl IpProtocol {
    /// Convert from the on-wire byte. Unknown protocols surface as `None`.
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Icmp),
            6 => Some(Self::Tcp),
            17 => Some(Self::Udp),
            _ => None,
        }
    }
}

/// Parsed IPv4 header.
#[derive(Debug, Clone, Copy)]
pub struct Ipv4Header {
    /// `version << 4 | ihl` — squib emits version=4, ihl=5 (no options).
    pub version_ihl: u8,
    /// `dscp << 2 | ecn` — DSCP and ECN. We emit zero.
    pub tos: u8,
    /// Total length (header + payload), in bytes.
    pub total_len: u16,
    /// Identification field.
    pub id: u16,
    /// `flags << 13 | fragment_offset`. We emit DF (`0x4000`) and no fragmentation.
    pub flags_frag: u16,
    /// Time-to-live.
    pub ttl: u8,
    /// Upper-layer protocol (TCP / ICMP / UDP).
    pub protocol: u8,
    /// Header checksum (zero on input to checksum computation).
    pub checksum: u16,
    /// Source IPv4 address (network-byte-order rendered as `[u8; 4]`).
    pub src: [u8; 4],
    /// Destination IPv4 address.
    pub dst: [u8; 4],
}

impl Ipv4Header {
    /// Parse the first 20 bytes of `bytes`. Validates the header
    /// checksum against the wire bytes; rejects options and short
    /// packets.
    ///
    /// # Errors
    /// - [`PduError::TooShort`] if `bytes.len() < 20`.
    /// - [`PduError::Unsupported`] for IPv6, options-bearing IHL > 5, fragmentation enabled.
    /// - [`PduError::BadChecksum`] for a wire checksum mismatch.
    pub fn parse(bytes: &[u8]) -> Result<Self, PduError> {
        if bytes.len() < IPV4_HDR_LEN {
            return Err(PduError::TooShort);
        }
        let version = bytes[0] >> 4;
        let ihl = bytes[0] & 0x0F;
        if version != 4 {
            return Err(PduError::Unsupported("IPv4: version != 4"));
        }
        if ihl != 5 {
            return Err(PduError::Unsupported("IPv4: header has options (IHL > 5)"));
        }
        let total_len = u16::from_be_bytes([bytes[2], bytes[3]]);
        let flags_frag = u16::from_be_bytes([bytes[6], bytes[7]]);
        // Reject fragmented packets: the `MF` flag (bit 13) or non-zero
        // fragment offset (bottom 13 bits) means we'd need to reassemble.
        if (flags_frag & 0x2000) != 0 || (flags_frag & 0x1FFF) != 0 {
            return Err(PduError::Unsupported("IPv4: fragmentation"));
        }
        let mut hdr = Self {
            version_ihl: bytes[0],
            tos: bytes[1],
            total_len,
            id: u16::from_be_bytes([bytes[4], bytes[5]]),
            flags_frag,
            ttl: bytes[8],
            protocol: bytes[9],
            checksum: u16::from_be_bytes([bytes[10], bytes[11]]),
            src: [bytes[12], bytes[13], bytes[14], bytes[15]],
            dst: [bytes[16], bytes[17], bytes[18], bytes[19]],
        };
        // Verify checksum: zero the field, recompute, compare.
        let mut hdr_zero = [0u8; IPV4_HDR_LEN];
        hdr_zero.copy_from_slice(&bytes[..IPV4_HDR_LEN]);
        hdr_zero[10] = 0;
        hdr_zero[11] = 0;
        let computed = ones_complement_sum(&hdr_zero);
        if computed != hdr.checksum {
            return Err(PduError::BadChecksum);
        }
        // Normalize: callers don't need the on-wire field anymore.
        hdr.checksum = 0;
        Ok(hdr)
    }

    /// Build a fresh IPv4 header for a `payload_len`-byte payload from
    /// `src` to `dst` carrying `protocol`. The `id` field rotates;
    /// callers pass a counter.
    #[must_use]
    pub fn build(
        src: [u8; 4],
        dst: [u8; 4],
        protocol: IpProtocol,
        payload_len: u16,
        id: u16,
    ) -> Self {
        Self {
            version_ihl: (4 << 4) | 5,
            tos: 0,
            total_len: u16::saturating_add(IPV4_HDR_LEN as u16, payload_len),
            id,
            flags_frag: 0x4000, // DF set, no fragmentation
            ttl: DEFAULT_TTL,
            protocol: protocol as u8,
            checksum: 0,
            src,
            dst,
        }
    }

    /// Serialise into 20 bytes with checksum filled in.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; IPV4_HDR_LEN] {
        let mut out = [0u8; IPV4_HDR_LEN];
        out[0] = self.version_ihl;
        out[1] = self.tos;
        out[2..4].copy_from_slice(&self.total_len.to_be_bytes());
        out[4..6].copy_from_slice(&self.id.to_be_bytes());
        out[6..8].copy_from_slice(&self.flags_frag.to_be_bytes());
        out[8] = self.ttl;
        out[9] = self.protocol;
        out[10..12].copy_from_slice(&[0, 0]); // checksum field zeroed for the calc
        out[12..16].copy_from_slice(&self.src);
        out[16..20].copy_from_slice(&self.dst);
        let checksum = ones_complement_sum(&out);
        out[10..12].copy_from_slice(&checksum.to_be_bytes());
        out
    }

    /// Total length minus the 20-byte header. Saturates at zero for
    /// malformed inputs.
    #[must_use]
    pub fn payload_len(&self) -> u16 {
        self.total_len.saturating_sub(IPV4_HDR_LEN as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_round_trip_a_well_formed_ipv4_header() {
        let h = Ipv4Header::build(
            [10, 0, 0, 1],
            [169, 254, 169, 254],
            IpProtocol::Tcp,
            40,
            0xBEEF,
        );
        let bytes = h.to_bytes();
        let parsed = Ipv4Header::parse(&bytes).unwrap();
        assert_eq!(parsed.src, [10, 0, 0, 1]);
        assert_eq!(parsed.dst, [169, 254, 169, 254]);
        assert_eq!(parsed.protocol, IpProtocol::Tcp as u8);
        assert_eq!(parsed.id, 0xBEEF);
        assert_eq!(parsed.total_len, 60);
    }

    #[test]
    fn test_should_reject_short_header() {
        assert!(matches!(
            Ipv4Header::parse(&[0u8; 19]),
            Err(PduError::TooShort)
        ));
    }

    #[test]
    fn test_should_reject_ipv6_version() {
        let mut h = Ipv4Header::build([0; 4], [0; 4], IpProtocol::Tcp, 0, 0);
        h.version_ihl = (6 << 4) | 5;
        // Manually serialise without the checksum recomputation by walking
        // through `to_bytes` then patching the version byte.
        let mut bytes = h.to_bytes();
        bytes[0] = (6 << 4) | 5;
        bytes[10] = 0;
        bytes[11] = 0;
        let csum = ones_complement_sum(&bytes);
        bytes[10..12].copy_from_slice(&csum.to_be_bytes());
        assert!(matches!(
            Ipv4Header::parse(&bytes),
            Err(PduError::Unsupported(_))
        ));
    }

    #[test]
    fn test_should_reject_bad_checksum() {
        let h = Ipv4Header::build([1; 4], [2; 4], IpProtocol::Tcp, 20, 1);
        let mut bytes = h.to_bytes();
        bytes[10] ^= 0xFF;
        assert!(matches!(
            Ipv4Header::parse(&bytes),
            Err(PduError::BadChecksum)
        ));
    }

    #[test]
    fn test_should_reject_fragmented_packets() {
        let h = Ipv4Header::build([0; 4], [0; 4], IpProtocol::Tcp, 0, 0);
        let mut bytes = h.to_bytes();
        // Set MF flag.
        bytes[6] = 0x20;
        bytes[7] = 0;
        bytes[10] = 0;
        bytes[11] = 0;
        let csum = ones_complement_sum(&bytes);
        bytes[10..12].copy_from_slice(&csum.to_be_bytes());
        assert!(matches!(
            Ipv4Header::parse(&bytes),
            Err(PduError::Unsupported(_))
        ));
    }

    #[test]
    fn test_payload_len_subtracts_header() {
        let h = Ipv4Header::build([0; 4], [0; 4], IpProtocol::Tcp, 100, 0);
        assert_eq!(h.payload_len(), 100);
    }
}
