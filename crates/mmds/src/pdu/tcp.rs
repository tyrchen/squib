//! TCP header parse / build with pseudo-header checksum.
//!
//! Squib's dumbo TCP server runs RFC 793 (the classic TCP) over the
//! IPv4 header in [`super::ipv4`]. Options: only MSS (option kind 2,
//! length 4) is honoured on parse; we don't emit options on output —
//! the link-local MMDS endpoint has a default-sane MTU and the guest
//! infers MSS from the IP layer.

use super::{PduError, ones_complement_sum};

/// Minimum TCP header length (no options).
pub const TCP_HDR_LEN: usize = 20;

/// `FIN` flag.
pub const TCP_FIN: u8 = 1 << 0;
/// `SYN` flag.
pub const TCP_SYN: u8 = 1 << 1;
/// `RST` flag.
pub const TCP_RST: u8 = 1 << 2;
/// `PSH` flag.
pub const TCP_PSH: u8 = 1 << 3;
/// `ACK` flag.
pub const TCP_ACK: u8 = 1 << 4;

/// Parsed TCP header. `data_offset` is the header length in 32-bit
/// words; the byte length is `4 * data_offset`. `flags` is the lower
/// six bits of the `flags` byte (the upper two are reserved + ECE/CWR
/// which we ignore).
#[derive(Debug, Clone, Copy)]
pub struct TcpHeader {
    /// Source port.
    pub src_port: u16,
    /// Destination port.
    pub dst_port: u16,
    /// Sequence number.
    pub seq: u32,
    /// Acknowledgement number (valid iff `ACK` flag is set).
    pub ack: u32,
    /// Header length in 32-bit words. `5` means a 20-byte header, no options.
    pub data_offset: u8,
    /// Control flags (FIN/SYN/RST/PSH/ACK/URG, lowest 6 bits).
    pub flags: u8,
    /// Receive window size (sender-advertised).
    pub window: u16,
    /// Checksum field as it appears on the wire.
    pub checksum: u16,
    /// Urgent pointer (we do not honour URG).
    pub urgent: u16,
    /// Maximum segment size, if MSS option present (option kind 2).
    pub mss: Option<u16>,
}

impl TcpHeader {
    /// Parse a TCP header from `bytes`, validating the pseudo-header
    /// checksum against the IPv4 source / destination and protocol byte.
    ///
    /// `payload` is the TCP payload (no header) — used only for the
    /// checksum.
    ///
    /// # Errors
    /// - [`PduError::TooShort`] if `bytes.len() < 20` or `bytes.len() < 4 * data_offset`.
    /// - [`PduError::BadChecksum`] for a checksum mismatch.
    pub fn parse(
        bytes: &[u8],
        payload: &[u8],
        src_ip: [u8; 4],
        dst_ip: [u8; 4],
    ) -> Result<Self, PduError> {
        if bytes.len() < TCP_HDR_LEN {
            return Err(PduError::TooShort);
        }
        let data_offset = bytes[12] >> 4;
        let hdr_len = 4 * data_offset as usize;
        if hdr_len < TCP_HDR_LEN || bytes.len() < hdr_len {
            return Err(PduError::TooShort);
        }
        let mut hdr = Self {
            src_port: u16::from_be_bytes([bytes[0], bytes[1]]),
            dst_port: u16::from_be_bytes([bytes[2], bytes[3]]),
            seq: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            ack: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            data_offset,
            flags: bytes[13] & 0x3F,
            window: u16::from_be_bytes([bytes[14], bytes[15]]),
            checksum: u16::from_be_bytes([bytes[16], bytes[17]]),
            urgent: u16::from_be_bytes([bytes[18], bytes[19]]),
            mss: None,
        };
        // Walk the options for MSS (kind 2, length 4).
        let mut i = TCP_HDR_LEN;
        while i + 1 < hdr_len {
            let kind = bytes[i];
            if kind == 0 {
                // EOL
                break;
            }
            if kind == 1 {
                // NOP
                i += 1;
                continue;
            }
            if i + 1 >= hdr_len {
                break;
            }
            let len = bytes[i + 1] as usize;
            if len < 2 || i + len > hdr_len {
                break;
            }
            if kind == 2 && len == 4 && i + 4 <= hdr_len {
                hdr.mss = Some(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
            }
            i += len;
        }
        // Verify pseudo-header checksum.
        let mut sum_buf = Vec::with_capacity(12 + hdr_len + payload.len() + (payload.len() & 1));
        sum_buf.extend_from_slice(&src_ip);
        sum_buf.extend_from_slice(&dst_ip);
        sum_buf.push(0);
        sum_buf.push(super::IpProtocol::Tcp as u8);
        let tcp_len = u16::try_from(hdr_len + payload.len()).unwrap_or(u16::MAX);
        sum_buf.extend_from_slice(&tcp_len.to_be_bytes());
        // Header bytes with checksum field zeroed.
        sum_buf.extend_from_slice(&bytes[..16]);
        sum_buf.extend_from_slice(&[0, 0]);
        sum_buf.extend_from_slice(&bytes[18..hdr_len]);
        sum_buf.extend_from_slice(payload);
        let computed = ones_complement_sum(&sum_buf);
        if computed != hdr.checksum {
            return Err(PduError::BadChecksum);
        }
        hdr.checksum = 0;
        Ok(hdr)
    }

    /// Build a TCP header carrying no options. Caller fills in flags,
    /// seq, ack, window. Checksum is computed in [`Self::to_bytes_with_payload`].
    #[must_use]
    pub fn build(src_port: u16, dst_port: u16, seq: u32, ack: u32, flags: u8, window: u16) -> Self {
        Self {
            src_port,
            dst_port,
            seq,
            ack,
            data_offset: 5,
            flags,
            window,
            checksum: 0,
            urgent: 0,
            mss: None,
        }
    }

    /// Serialise the header + a payload into a single buffer with the
    /// TCP checksum filled in over the IPv4 pseudo-header.
    #[must_use]
    pub fn to_bytes_with_payload(
        &self,
        src_ip: [u8; 4],
        dst_ip: [u8; 4],
        payload: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(TCP_HDR_LEN + payload.len());
        out.extend_from_slice(&self.src_port.to_be_bytes());
        out.extend_from_slice(&self.dst_port.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.ack.to_be_bytes());
        out.push(self.data_offset << 4); // reserved bits = 0
        out.push(self.flags);
        out.extend_from_slice(&self.window.to_be_bytes());
        out.extend_from_slice(&[0, 0]); // checksum placeholder
        out.extend_from_slice(&self.urgent.to_be_bytes());
        out.extend_from_slice(payload);
        // Compute checksum over pseudo-header + segment.
        let tcp_len = u16::try_from(out.len()).unwrap_or(u16::MAX);
        let mut sum_buf = Vec::with_capacity(12 + out.len() + (out.len() & 1));
        sum_buf.extend_from_slice(&src_ip);
        sum_buf.extend_from_slice(&dst_ip);
        sum_buf.push(0);
        sum_buf.push(super::IpProtocol::Tcp as u8);
        sum_buf.extend_from_slice(&tcp_len.to_be_bytes());
        sum_buf.extend_from_slice(&out);
        let checksum = ones_complement_sum(&sum_buf);
        out[16..18].copy_from_slice(&checksum.to_be_bytes());
        out
    }

    /// Convenience: header only (no payload), checksum filled.
    #[must_use]
    pub fn to_bytes_header_only(&self, src_ip: [u8; 4], dst_ip: [u8; 4]) -> Vec<u8> {
        self.to_bytes_with_payload(src_ip, dst_ip, &[])
    }

    /// `true` if the SYN flag is set.
    #[must_use]
    pub fn is_syn(&self) -> bool {
        self.flags & TCP_SYN != 0
    }
    /// `true` if the FIN flag is set.
    #[must_use]
    pub fn is_fin(&self) -> bool {
        self.flags & TCP_FIN != 0
    }
    /// `true` if the RST flag is set.
    #[must_use]
    pub fn is_rst(&self) -> bool {
        self.flags & TCP_RST != 0
    }
    /// `true` if the ACK flag is set.
    #[must_use]
    pub fn is_ack(&self) -> bool {
        self.flags & TCP_ACK != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syn_segment() -> TcpHeader {
        TcpHeader::build(40000, 80, 0x1234_5678, 0, TCP_SYN, 65535)
    }

    #[test]
    fn test_should_round_trip_a_syn_segment() {
        let src = [10, 0, 0, 1];
        let dst = [169, 254, 169, 254];
        let h = syn_segment();
        let bytes = h.to_bytes_header_only(src, dst);
        let parsed = TcpHeader::parse(&bytes, &[], src, dst).unwrap();
        assert_eq!(parsed.src_port, 40000);
        assert_eq!(parsed.dst_port, 80);
        assert!(parsed.is_syn());
        assert_eq!(parsed.seq, 0x1234_5678);
    }

    #[test]
    fn test_should_round_trip_segment_with_payload() {
        let src = [192, 168, 1, 1];
        let dst = [192, 168, 1, 2];
        let h = TcpHeader::build(1234, 5678, 100, 200, TCP_ACK | TCP_PSH, 1024);
        let payload = b"GET / HTTP/1.1\r\nHost: a\r\n\r\n";
        let bytes = h.to_bytes_with_payload(src, dst, payload);
        let parsed = TcpHeader::parse(&bytes[..TCP_HDR_LEN], payload, src, dst).unwrap();
        assert!(parsed.is_ack());
        assert_eq!(parsed.flags & TCP_PSH, TCP_PSH);
        assert_eq!(parsed.window, 1024);
    }

    #[test]
    fn test_should_reject_bad_checksum() {
        let src = [10, 0, 0, 1];
        let dst = [169, 254, 169, 254];
        let h = syn_segment();
        let mut bytes = h.to_bytes_header_only(src, dst);
        bytes[16] ^= 0xFF;
        assert!(matches!(
            TcpHeader::parse(&bytes, &[], src, dst),
            Err(PduError::BadChecksum)
        ));
    }

    #[test]
    fn test_should_parse_mss_option() {
        let src = [1, 1, 1, 1];
        let dst = [2, 2, 2, 2];
        // Hand-build a SYN with MSS = 1460 option (kind=2, len=4, value=1460).
        let mut bytes = TcpHeader::build(1, 2, 0, 0, TCP_SYN, 0).to_bytes_header_only(src, dst);
        // Replace data_offset to 6 (24 bytes), append MSS option.
        bytes[12] = 6 << 4;
        bytes.extend_from_slice(&[2, 4, 0x05, 0xB4]); // MSS=1460
        // Recompute checksum.
        let payload = &[];
        bytes[16] = 0;
        bytes[17] = 0;
        let mut sum_buf = Vec::new();
        sum_buf.extend_from_slice(&src);
        sum_buf.extend_from_slice(&dst);
        sum_buf.extend_from_slice(&[0, super::super::IpProtocol::Tcp as u8]);
        let tcp_len = u16::try_from(bytes.len()).unwrap();
        sum_buf.extend_from_slice(&tcp_len.to_be_bytes());
        sum_buf.extend_from_slice(&bytes);
        let csum = ones_complement_sum(&sum_buf);
        bytes[16..18].copy_from_slice(&csum.to_be_bytes());

        let parsed = TcpHeader::parse(&bytes, payload, src, dst).unwrap();
        assert_eq!(parsed.mss, Some(1460));
    }

    #[test]
    fn test_should_reject_short_header() {
        assert!(matches!(
            TcpHeader::parse(&[0u8; 10], &[], [0; 4], [0; 4]),
            Err(PduError::TooShort)
        ));
    }
}
