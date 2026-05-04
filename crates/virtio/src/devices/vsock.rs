//! virtio-vsock — host/guest socket transport.
//!
//! Per [14-virtio-and-devices.md § 4.3](../../../specs/14-virtio-and-devices.md#43-virtio-vsock).
//! Two modes:
//!
//! - **Plain mode** (default): UDS multiplex protocol bit-identical to upstream Firecracker.
//!   Host-initiated `CONNECT <port>\n` → `OK <port>\n`; guest-initiated `<uds_path>_<port>`
//!   listener.
//! - **TSI mode** (opt-in via `"squib": { "vsock_tsi": true }`): guest opens `AF_VSOCK` sockets and
//!   we transparently proxy to host `AF_INET` / `AF_UNIX`. Off by default (D8); the spec also notes
//!   a guest-kernel cooperation requirement.
//!
//! This module implements the wire-protocol layer: packet parsing,
//! per-direction queue handling, and a [`VsockMuxer`] trait that actually
//! routes packets to / from host UDS connections. The production muxer
//! lives in the VMM event loop; tests use [`InMemoryMuxer`].

use std::sync::Arc;

use parking_lot::Mutex;
use squib_core::GuestMemory;

use crate::{
    device::{ActivateError, VirtioDevice},
    device_id::VirtioDeviceType,
    interrupt::IrqLine,
    queue::Queue,
};

/// Reserved CIDs per virtio-vsock spec § 5.10.4.
pub const VMADDR_CID_HYPERVISOR: u64 = 0;
/// Local-only loopback.
pub const VMADDR_CID_LOCAL: u64 = 1;
/// Host-side context.
pub const VMADDR_CID_HOST: u64 = 2;
/// "Any" CID — wildcard, never used as a real CID.
pub const VMADDR_CID_ANY: u64 = u64::MAX;
/// Smallest valid guest CID per spec.
pub const MIN_GUEST_CID: u64 = 3;

/// `VIRTIO_VSOCK_TYPE_STREAM` packet type.
pub const TYPE_STREAM: u16 = 1;

/// Packet operations (`hdr.op`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum VsockOp {
    /// Reserved / unknown op — packet is invalid.
    Invalid = 0,
    /// `VIRTIO_VSOCK_OP_REQUEST` — open a connection.
    Request = 1,
    /// `VIRTIO_VSOCK_OP_RESPONSE` — accept a connection.
    Response = 2,
    /// `VIRTIO_VSOCK_OP_RST` — reset; close abruptly.
    Rst = 3,
    /// `VIRTIO_VSOCK_OP_SHUTDOWN` — graceful close.
    Shutdown = 4,
    /// `VIRTIO_VSOCK_OP_RW` — payload bytes.
    Rw = 5,
    /// `VIRTIO_VSOCK_OP_CREDIT_UPDATE` — flow-control update.
    CreditUpdate = 6,
    /// `VIRTIO_VSOCK_OP_CREDIT_REQUEST` — request flow-control update.
    CreditRequest = 7,
}

impl VsockOp {
    /// Parse a packet op from its u16 wire value.
    #[must_use]
    pub fn from_wire(value: u16) -> Self {
        match value {
            1 => Self::Request,
            2 => Self::Response,
            3 => Self::Rst,
            4 => Self::Shutdown,
            5 => Self::Rw,
            6 => Self::CreditUpdate,
            7 => Self::CreditRequest,
            _ => Self::Invalid,
        }
    }
}

/// Wire-format packet header (44 bytes per spec § 5.10.6).
#[derive(Debug, Clone, Copy)]
pub struct VsockHeader {
    /// Source CID.
    pub src_cid: u64,
    /// Destination CID.
    pub dst_cid: u64,
    /// Source port.
    pub src_port: u32,
    /// Destination port.
    pub dst_port: u32,
    /// Payload length.
    pub len: u32,
    /// Packet type (e.g. [`TYPE_STREAM`]).
    pub type_: u16,
    /// Operation.
    pub op: VsockOp,
    /// Flags (op-specific bitmap).
    pub flags: u32,
    /// Receiver advertised buffer size.
    pub buf_alloc: u32,
    /// Receiver forwarded byte count.
    pub fwd_cnt: u32,
}

const HDR_SIZE: usize = 44;

/// Errors produced by vsock packet parsing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum VsockParseError {
    /// Input shorter than the wire-format header (44 bytes).
    #[error("vsock packet shorter than 44-byte header")]
    HeaderTooShort,
}

impl VsockHeader {
    /// Parse a 44-byte header.
    ///
    /// # Errors
    /// [`VsockParseError::HeaderTooShort`] if `bytes.len() < 44`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, VsockParseError> {
        if bytes.len() < HDR_SIZE {
            return Err(VsockParseError::HeaderTooShort);
        }
        // Closed-form helpers without `unwrap`. `bytes.len() >= HDR_SIZE`
        // bounds every slice index below; the panicky `try_into` of the
        // earlier draft is replaced with explicit byte-by-byte reads.
        let u64 = |i: usize| {
            u64::from_le_bytes([
                bytes[i],
                bytes[i + 1],
                bytes[i + 2],
                bytes[i + 3],
                bytes[i + 4],
                bytes[i + 5],
                bytes[i + 6],
                bytes[i + 7],
            ])
        };
        let u32 =
            |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let u16 = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        Ok(Self {
            src_cid: u64(0),
            dst_cid: u64(8),
            src_port: u32(16),
            dst_port: u32(20),
            len: u32(24),
            type_: u16(28),
            op: VsockOp::from_wire(u16(30)),
            flags: u32(32),
            buf_alloc: u32(36),
            fwd_cnt: u32(40),
        })
    }

    /// Serialise into 44 bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; HDR_SIZE] {
        let mut out = [0u8; HDR_SIZE];
        out[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        out[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        out[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        out[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        out[24..28].copy_from_slice(&self.len.to_le_bytes());
        out[28..30].copy_from_slice(&self.type_.to_le_bytes());
        out[30..32].copy_from_slice(&(self.op as u16).to_le_bytes());
        out[32..36].copy_from_slice(&self.flags.to_le_bytes());
        out[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        out[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        out
    }
}

/// One vsock packet — header plus payload.
#[derive(Debug, Clone)]
pub struct VsockPacket {
    /// Wire header.
    pub hdr: VsockHeader,
    /// Payload bytes (length matches `hdr.len`).
    pub payload: Vec<u8>,
}

/// Trait for the host-side multiplexer: routes guest-originated packets to
/// host UDS connections, and presents host-originated packets back to the
/// device for delivery to the guest.
pub trait VsockMuxer: Send + Sync + std::fmt::Debug {
    /// Process a packet sent by the guest. Returns any host-originated
    /// packet that should be sent back as a direct reply (e.g. a
    /// `Response` to a `Request`).
    fn handle_tx(&self, pkt: VsockPacket) -> Vec<VsockPacket>;

    /// Drain any host-originated packets the muxer wants delivered to the
    /// guest.
    fn drain_rx(&self) -> Vec<VsockPacket>;
}

/// Test muxer that records every TX packet and lets the test inject RX
/// packets.
#[derive(Debug, Default)]
pub struct InMemoryMuxer {
    /// Log of every packet routed through `handle_tx`.
    pub tx_log: Mutex<Vec<VsockPacket>>,
    /// Queue of packets the next `drain_rx` will return.
    pub rx_queue: Mutex<Vec<VsockPacket>>,
    /// If true, every Request triggers an automatic Response.
    pub auto_respond: bool,
}

impl VsockMuxer for InMemoryMuxer {
    fn handle_tx(&self, pkt: VsockPacket) -> Vec<VsockPacket> {
        let mut replies = Vec::new();
        if self.auto_respond && pkt.hdr.op == VsockOp::Request {
            let mut hdr = pkt.hdr;
            std::mem::swap(&mut hdr.src_cid, &mut hdr.dst_cid);
            std::mem::swap(&mut hdr.src_port, &mut hdr.dst_port);
            hdr.op = VsockOp::Response;
            hdr.len = 0;
            replies.push(VsockPacket {
                hdr,
                payload: Vec::new(),
            });
        }
        self.tx_log.lock().push(pkt);
        replies
    }
    fn drain_rx(&self) -> Vec<VsockPacket> {
        std::mem::take(&mut *self.rx_queue.lock())
    }
}

/// virtio-vsock configuration.
#[derive(Debug, Clone)]
pub struct VsockConfig {
    /// Operator-supplied identifier.
    pub vsock_id: String,
    /// Guest CID. Must be ≥ 3.
    pub guest_cid: u64,
    /// Host UDS path (informational; the production muxer reads this).
    pub uds_path: String,
    /// `true` to enable TSI mode (D8 — off by default).
    pub tsi: bool,
}

const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;
/// Event queue index. virtio-vsock allocates an event queue alongside the
/// RX/TX queues per spec § 5.10.5; v1.0 plain mode never produces events
/// (the host-side muxer triggers vrings directly), so the queue is parked.
const _EVENT_QUEUE: usize = 2;
const QUEUE_MAX_SIZE: u16 = 256;

/// virtio-vsock frontend.
#[derive(Debug)]
pub struct VsockDevice {
    avail: u64,
    acked: u64,
    queues: Vec<Queue>,
    config: VsockConfig,
    muxer: Arc<dyn VsockMuxer>,
    state: Arc<Mutex<ActiveState>>,
    /// Buffered RX packets that didn't fit into the previous RX drain.
    rx_buffer: Arc<Mutex<Vec<VsockPacket>>>,
}

#[derive(Debug, Default)]
struct ActiveState {
    mem: Option<Arc<dyn GuestMemory>>,
    irq: Option<IrqLine>,
    activated: bool,
}

impl VsockDevice {
    /// Build a virtio-vsock.
    ///
    /// # Errors
    /// [`std::io::Error`] (kind `InvalidInput`) if `guest_cid < 3`.
    pub fn new(config: VsockConfig, muxer: Arc<dyn VsockMuxer>) -> Result<Self, std::io::Error> {
        if config.guest_cid < MIN_GUEST_CID {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("guest_cid must be >= {MIN_GUEST_CID}"),
            ));
        }
        // TSI startup warning per spec note in 14 § 4.3.
        if config.tsi {
            tracing::warn!(
                vsock_id = %config.vsock_id,
                "vsock_tsi=true requires a libkrun-patched guest kernel; \
                 stock guest kernels treat AF_VSOCK as plain vsock and the \
                 TSI proxy is inactive (see docs/macos-setup.md)"
            );
        }
        Ok(Self {
            avail: 0,
            acked: 0,
            queues: vec![
                Queue::new(QUEUE_MAX_SIZE),
                Queue::new(QUEUE_MAX_SIZE),
                Queue::new(QUEUE_MAX_SIZE),
            ],
            config,
            muxer,
            state: Arc::new(Mutex::new(ActiveState::default())),
            rx_buffer: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn drain_tx(&mut self) {
        let (mem, irq) = {
            let state = self.state.lock();
            match (state.mem.clone(), state.irq.clone()) {
                (Some(m), Some(i)) => (m, i),
                _ => return,
            }
        };
        let muxer = Arc::clone(&self.muxer);
        let rx_buffer = Arc::clone(&self.rx_buffer);
        let queue = &mut self.queues[TX_QUEUE];
        let mut completed = false;
        loop {
            let chain = match queue.pop_avail(mem.as_ref()) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "vsock: tx walk failed");
                    break;
                }
            };
            let head = chain.head_index();
            let descs = match chain.collect(mem.as_ref()) {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!(error = %err, "vsock: tx chain collect failed");
                    break;
                }
            };
            // Concatenate device-read descriptors into a single buffer.
            let mut buf = Vec::new();
            for desc in &descs {
                if desc.is_write_only() {
                    continue;
                }
                let len = desc.len as usize;
                let mut piece = vec![0u8; len];
                if mem.read(desc.addr, &mut piece).is_err() {
                    continue;
                }
                buf.extend_from_slice(&piece);
            }
            if buf.len() < HDR_SIZE {
                let _ = queue.push_used(mem.as_ref(), head, 0);
                completed = true;
                continue;
            }
            let Ok(hdr) = VsockHeader::from_bytes(&buf[..HDR_SIZE]) else {
                continue;
            };
            let payload_len = (hdr.len as usize).min(buf.len() - HDR_SIZE);
            let payload = buf[HDR_SIZE..HDR_SIZE + payload_len].to_vec();
            let pkt = VsockPacket { hdr, payload };
            let replies = muxer.handle_tx(pkt);
            if !replies.is_empty() {
                rx_buffer.lock().extend(replies);
            }
            if let Err(err) = queue.push_used(mem.as_ref(), head, 0) {
                tracing::warn!(error = %err, "vsock: tx push_used failed");
                break;
            }
            completed = true;
        }
        if completed {
            let _ = irq.trigger_queue();
        }
    }

    fn drain_rx(&mut self) {
        let (mem, irq) = {
            let state = self.state.lock();
            match (state.mem.clone(), state.irq.clone()) {
                (Some(m), Some(i)) => (m, i),
                _ => return,
            }
        };
        // Pull both pre-buffered replies from TX-handling and any new packets
        // the muxer wants to push.
        let muxer = Arc::clone(&self.muxer);
        let mut packets: Vec<VsockPacket> = std::mem::take(&mut *self.rx_buffer.lock());
        packets.extend(muxer.drain_rx());
        if packets.is_empty() {
            return;
        }
        let queue = &mut self.queues[RX_QUEUE];
        let mut completed = false;
        for pkt in packets {
            let chain = match queue.pop_avail(mem.as_ref()) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    // Re-buffer for the next RX drain.
                    self.rx_buffer.lock().push(pkt);
                    break;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "vsock: rx walk failed");
                    break;
                }
            };
            let head = chain.head_index();
            let descs = match chain.collect(mem.as_ref()) {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!(error = %err, "vsock: rx chain collect failed");
                    break;
                }
            };
            let mut wire = pkt.hdr.to_bytes().to_vec();
            wire.extend_from_slice(&pkt.payload);
            let mut written: u32 = 0;
            let mut wire_off = 0usize;
            for desc in descs {
                if !desc.is_write_only() {
                    continue;
                }
                let len = (desc.len as usize).min(wire.len() - wire_off);
                if len == 0 {
                    continue;
                }
                if mem
                    .write(desc.addr, &wire[wire_off..wire_off + len])
                    .is_err()
                {
                    break;
                }
                wire_off += len;
                written = written.saturating_add(len as u32);
                if wire_off >= wire.len() {
                    break;
                }
            }
            if let Err(err) = queue.push_used(mem.as_ref(), head, written) {
                tracing::warn!(error = %err, "vsock: rx push_used failed");
                break;
            }
            completed = true;
        }
        if completed {
            let _ = irq.trigger_queue();
        }
    }
}

impl VirtioDevice for VsockDevice {
    fn device_type(&self) -> VirtioDeviceType {
        VirtioDeviceType::Vsock
    }
    fn avail_features(&self) -> u64 {
        self.avail
    }
    fn acked_features(&self) -> u64 {
        self.acked
    }
    fn set_acked_features(&mut self, value: u64) {
        self.acked = value;
    }
    fn queue_max_sizes(&self) -> &[u16] {
        const SIZES: &[u16] = &[QUEUE_MAX_SIZE, QUEUE_MAX_SIZE, QUEUE_MAX_SIZE];
        SIZES
    }
    fn queues(&self) -> &[Queue] {
        &self.queues
    }
    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // Config layout: 8-byte little-endian guest_cid.
        let bytes = self.config.guest_cid.to_le_bytes();
        let off = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = bytes.get(off + i).copied().unwrap_or(0);
        }
    }
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    fn activate(&mut self, mem: Arc<dyn GuestMemory>, irq: IrqLine) -> Result<(), ActivateError> {
        let mut state = self.state.lock();
        state.mem = Some(mem);
        state.irq = Some(irq);
        state.activated = true;
        Ok(())
    }
    fn is_activated(&self) -> bool {
        self.state.lock().activated
    }
    fn process_queue(&mut self, queue_index: u16) {
        match queue_index as usize {
            TX_QUEUE => {
                self.drain_tx();
                // After each TX flush, drain RX so muxer-injected replies
                // reach the guest immediately.
                self.drain_rx();
            }
            RX_QUEUE => self.drain_rx(),
            // Event queue (v1.0 plain mode never produces events) and any
            // unknown queue index are silently dropped.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use squib_arch::IntId;
    use squib_core::{GuestAddress, SliceGuestMemory};
    use squib_gic::Gic;

    use super::*;
    use crate::queue::VIRTQ_DESC_F_WRITE;

    #[derive(Debug, Default)]
    struct StubGic;
    impl Gic for StubGic {
        fn pulse_spi(&self, _: IntId) -> Result<(), squib_gic::GicError> {
            Ok(())
        }
        fn set_spi_level(&self, _: IntId, _: bool) -> Result<(), squib_gic::GicError> {
            Ok(())
        }
        fn save_state(&self) -> Result<Vec<u8>, squib_gic::GicError> {
            Ok(Vec::new())
        }
        fn restore_state(&self, _data: &[u8]) -> Result<(), squib_gic::GicError> {
            Ok(())
        }
    }

    fn line() -> IrqLine {
        let gic: Arc<dyn Gic + Send + Sync> = Arc::new(StubGic);
        IrqLine::new(gic, IntId::from_spi_cell(18).unwrap())
    }

    fn config(cid: u64, tsi: bool) -> VsockConfig {
        VsockConfig {
            vsock_id: "vsock0".into(),
            guest_cid: cid,
            uds_path: "/var/run/squib.vsock".into(),
            tsi,
        }
    }

    #[test]
    fn test_should_reject_guest_cid_below_3() {
        let muxer = Arc::new(InMemoryMuxer::default());
        let err = VsockDevice::new(config(2, false), muxer).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn test_should_publish_guest_cid_in_config() {
        let muxer = Arc::new(InMemoryMuxer::default());
        let dev = VsockDevice::new(config(42, false), muxer).unwrap();
        let mut got = [0u8; 8];
        dev.read_config(0, &mut got);
        assert_eq!(u64::from_le_bytes(got), 42);
    }

    #[test]
    fn test_should_round_trip_header_through_to_bytes_and_back() {
        let h = VsockHeader {
            src_cid: 3,
            dst_cid: 2,
            src_port: 1024,
            dst_port: 80,
            len: 7,
            type_: TYPE_STREAM,
            op: VsockOp::Request,
            flags: 0,
            buf_alloc: 4096,
            fwd_cnt: 0,
        };
        let bytes = h.to_bytes();
        let parsed = VsockHeader::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.src_cid, 3);
        assert_eq!(parsed.dst_port, 80);
        assert_eq!(parsed.op, VsockOp::Request);
    }

    #[test]
    fn test_should_route_tx_packet_to_muxer_and_buffer_replies() {
        let muxer = Arc::new(InMemoryMuxer {
            auto_respond: true,
            ..Default::default()
        });
        let mut dev = VsockDevice::new(config(3, false), muxer.clone()).unwrap();
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        let q = &mut dev.queues_mut()[TX_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        // Build a CONNECT request: src=guest cid=3 port=1024, dst=host cid=2 port=80.
        let hdr = VsockHeader {
            src_cid: 3,
            dst_cid: VMADDR_CID_HOST,
            src_port: 1024,
            dst_port: 80,
            len: 0,
            type_: TYPE_STREAM,
            op: VsockOp::Request,
            flags: 0,
            buf_alloc: 4096,
            fwd_cnt: 0,
        };
        mem.write(GuestAddress(0x4000_2000), &hdr.to_bytes())
            .unwrap();
        let base = 0x4000_0000u64;
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), HDR_SIZE as u32)
            .unwrap();
        mem.write_u16_le(GuestAddress(base + 12), 0).unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();

        // Set up an RX descriptor too.
        let q = &mut dev.queues_mut()[RX_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0100);
        q.avail_ring_addr = GuestAddress(0x4000_0900);
        q.used_ring_addr = GuestAddress(0x4000_1100);
        q.ready = true;
        let rxbase = 0x4000_0100u64;
        mem.write_u32_le(GuestAddress(rxbase), 0x4000_3000).unwrap();
        mem.write_u32_le(GuestAddress(rxbase + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(rxbase + 8), 64).unwrap();
        mem.write_u16_le(GuestAddress(rxbase + 12), VIRTQ_DESC_F_WRITE)
            .unwrap();
        mem.write_u16_le(GuestAddress(rxbase + 14), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0904), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0902), 1).unwrap();

        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(TX_QUEUE as u16);
        // Muxer logged the TX request.
        assert_eq!(muxer.tx_log.lock().len(), 1);
        // Auto-response landed in RX queue: parse and verify op=Response.
        let mut wire = [0u8; HDR_SIZE];
        mem.read(GuestAddress(0x4000_3000), &mut wire).unwrap();
        let parsed = VsockHeader::from_bytes(&wire).unwrap();
        assert_eq!(parsed.op, VsockOp::Response);
        // Source / dest swapped.
        assert_eq!(parsed.src_port, 80);
        assert_eq!(parsed.dst_port, 1024);
    }

    #[test]
    fn test_should_log_tsi_warning_when_tsi_enabled() {
        // Just exercise the constructor with TSI=true; the warning side-effect
        // is observable in tracing logs (not asserted here).
        let muxer = Arc::new(InMemoryMuxer::default());
        let dev = VsockDevice::new(config(3, true), muxer).unwrap();
        assert!(dev.config.tsi);
    }
}
