//! virtio-net — network frontend.
//!
//! Per [14-virtio-and-devices.md § 4.2](../../../specs/14-virtio-and-devices.md#42-virtio-net):
//!
//! > Frontend ported from cloud-hypervisor `virtio-devices/src/net.rs`.
//! > Host backend lives in [30-networking.md](../../../specs/30-networking.md) — virtio-net
//! > is parameterised over a [`NetBackend`] trait so the frontend is host-agnostic.
//!
//! The `MmdsInterceptor` from [15-mmds.md § 3](../../../specs/15-mmds.md#3-packet-interception)
//! sits between the frontend's RX/TX queues and the host backend, peeling off
//! ARP and TCP-to-MMDS-IP frames before they hit the wire.
//!
//! ## Queue layout
//!
//! - Queue 0 — RX (host → guest).
//! - Queue 1 — TX (guest → host).
//!
//! `VIRTIO_NET_F_CTRL_VQ` (would add a third control queue) is not offered
//! and not allocated in 1.0.

use std::sync::Arc;

use parking_lot::Mutex;
use squib_core::GuestMemory;

use crate::{
    device::{ActivateError, VirtioDevice},
    device_id::VirtioDeviceType,
    interrupt::IrqLine,
    queue::Queue,
};

/// `VIRTIO_NET_F_MAC` — driver may read MAC from config-space.
pub const F_MAC: u64 = 1 << 5;
/// `VIRTIO_NET_F_STATUS` — driver may read link status.
pub const F_STATUS: u64 = 1 << 16;
/// `VIRTIO_NET_F_MTU` — driver may read MTU.
pub const F_MTU: u64 = 1 << 3;

/// RX queue index (host → guest).
pub const RX_QUEUE: usize = 0;
/// TX queue index (guest → host).
pub const TX_QUEUE: usize = 1;

const QUEUE_MAX_SIZE: u16 = 256;
const VIRTIO_NET_HDR_LEN: u32 = 12;

/// IPv4 / TCP / UDP frame as it crosses the virtio-net boundary. Pure
/// payload — the virtio-net header is consumed by the frontend before frames
/// reach the backend (and prepended on RX).
#[derive(Debug, Clone)]
pub struct Frame {
    /// Raw bytes (Ethernet header + IP + payload).
    pub bytes: Vec<u8>,
}

impl Frame {
    /// Build a frame from a slice.
    #[must_use]
    pub fn from_slice(slice: &[u8]) -> Self {
        Self {
            bytes: slice.to_vec(),
        }
    }
}

/// Host-side network backend. Production implementations live in
/// `squib-net` (vmnet, gvproxy); test code uses [`LoopbackBackend`].
pub trait NetBackend: Send + Sync + std::fmt::Debug {
    /// Send a frame from the guest to the host network.
    fn send(&self, frame: &Frame);

    /// Take any frames the host wants to deliver to the guest. The frontend
    /// drains this on every RX queue notification.
    fn recv(&self) -> Vec<Frame>;
}

/// Loopback backend — frames sent by the guest become frames the guest
/// receives on the next RX notification. Useful for tests and the
/// "shared/userspace network not yet wired" placeholder.
#[derive(Debug, Default)]
pub struct LoopbackBackend {
    pending: Mutex<Vec<Frame>>,
}

impl NetBackend for LoopbackBackend {
    fn send(&self, frame: &Frame) {
        self.pending.lock().push(frame.clone());
    }
    fn recv(&self) -> Vec<Frame> {
        std::mem::take(&mut *self.pending.lock())
    }
}

/// Frame interceptor — peels MMDS-bound frames out of the TX path and
/// injects MMDS responses into the RX path. The wire shape is documented in
/// [15-mmds.md § 3](../../../specs/15-mmds.md#3-packet-interception).
pub trait FrameInterceptor: Send + Sync + std::fmt::Debug {
    /// Inspect a TX frame from the guest. Return `true` if the interceptor
    /// consumed the frame (it MUST NOT reach the host backend).
    fn intercept_tx(&self, frame: &Frame) -> bool;

    /// Drain any RX frames the interceptor wants to inject. Called on every
    /// RX notification before the host backend's frames.
    fn drain_rx(&self) -> Vec<Frame>;
}

/// No-op interceptor — every TX frame goes to the wire, no RX is injected.
/// The default for net configs without `mmds_networks` set.
#[derive(Debug, Default)]
pub struct NoopInterceptor;

impl FrameInterceptor for NoopInterceptor {
    fn intercept_tx(&self, _frame: &Frame) -> bool {
        false
    }
    fn drain_rx(&self) -> Vec<Frame> {
        Vec::new()
    }
}

/// Adapter so [`squib_mmds::MmdsInterceptor`] can plug straight into the
/// virtio-net frontend's [`FrameInterceptor`] seam. The interceptor's
/// own API works on raw byte slices; we wrap that as `Frame`-shaped
/// inputs / outputs.
impl FrameInterceptor for squib_mmds::MmdsInterceptor {
    fn intercept_tx(&self, frame: &Frame) -> bool {
        // The MMDS interceptor expects a complete Ethernet frame; the
        // virtio-net frontend hands us the same shape, so this is a
        // direct passthrough.
        squib_mmds::MmdsInterceptor::intercept(self, &frame.bytes)
    }
    fn drain_rx(&self) -> Vec<Frame> {
        squib_mmds::MmdsInterceptor::drain_rx(self)
            .into_iter()
            .map(|bytes| Frame { bytes })
            .collect()
    }
}

/// virtio-net configuration.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// Operator-supplied identifier.
    pub iface_id: String,
    /// Host-side device name (`eth0`, `tap0`, etc.).
    pub host_dev_name: String,
    /// Guest-side MAC; if `None` the driver picks one (we don't offer `F_MAC`).
    pub guest_mac: Option<[u8; 6]>,
    /// MTU; if `None` we don't offer `VIRTIO_NET_F_MTU`.
    pub mtu: Option<u16>,
}

/// virtio-net frontend.
#[derive(Debug)]
pub struct NetDevice {
    avail: u64,
    acked: u64,
    queues: Vec<Queue>,
    config: NetConfig,
    backend: Arc<dyn NetBackend>,
    interceptor: Arc<dyn FrameInterceptor>,
    state: Arc<Mutex<ActiveState>>,
}

#[derive(Debug, Default)]
struct ActiveState {
    mem: Option<Arc<dyn GuestMemory>>,
    irq: Option<IrqLine>,
    activated: bool,
}

impl NetDevice {
    /// Build a virtio-net.
    #[must_use]
    pub fn new(
        config: NetConfig,
        backend: Arc<dyn NetBackend>,
        interceptor: Arc<dyn FrameInterceptor>,
    ) -> Self {
        let mut avail = 0;
        if config.guest_mac.is_some() {
            avail |= F_MAC;
        }
        if config.mtu.is_some() {
            avail |= F_MTU;
        }
        Self {
            avail,
            acked: 0,
            queues: vec![Queue::new(QUEUE_MAX_SIZE), Queue::new(QUEUE_MAX_SIZE)],
            config,
            backend,
            interceptor,
            state: Arc::new(Mutex::new(ActiveState::default())),
        }
    }

    fn drain_tx(&mut self) {
        let (mem, irq) = {
            let state = self.state.lock();
            match (state.mem.clone(), state.irq.clone()) {
                (Some(m), Some(i)) => (m, i),
                _ => return,
            }
        };
        let backend = Arc::clone(&self.backend);
        let interceptor = Arc::clone(&self.interceptor);
        let queue = &mut self.queues[TX_QUEUE];
        let mut completed = false;
        loop {
            let chain = match queue.pop_avail(mem.as_ref()) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "net: tx walk failed");
                    break;
                }
            };
            let head = chain.head_index();
            let descs = match chain.collect(mem.as_ref()) {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!(error = %err, "net: tx chain collect failed");
                    break;
                }
            };
            // Concatenate the device-read descriptors into a single frame
            // buffer; strip the 12-byte virtio-net header per spec § 5.1.6.
            let mut frame_bytes = Vec::new();
            for desc in &descs {
                if desc.is_write_only() {
                    continue;
                }
                let mut buf = vec![0u8; desc.len as usize];
                if let Err(err) = mem.read(desc.addr, &mut buf) {
                    tracing::warn!(error = %err, "net: tx frame read failed");
                    continue;
                }
                frame_bytes.extend_from_slice(&buf);
            }
            // Strip header.
            let payload = if frame_bytes.len() > VIRTIO_NET_HDR_LEN as usize {
                Frame {
                    bytes: frame_bytes[VIRTIO_NET_HDR_LEN as usize..].to_vec(),
                }
            } else {
                continue;
            };
            if !interceptor.intercept_tx(&payload) {
                backend.send(&payload);
            }
            if let Err(err) = queue.push_used(mem.as_ref(), head, 0) {
                tracing::warn!(error = %err, "net: tx push_used failed");
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
        let backend = Arc::clone(&self.backend);
        let interceptor = Arc::clone(&self.interceptor);
        // Combine interceptor + backend frames; interceptor first so MMDS
        // responses precede any host-network traffic the guest may consume.
        let mut frames = interceptor.drain_rx();
        frames.extend(backend.recv());
        if frames.is_empty() {
            return;
        }
        let queue = &mut self.queues[RX_QUEUE];
        let mut completed = false;
        for frame in frames {
            let chain = match queue.pop_avail(mem.as_ref()) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "net: rx walk failed");
                    break;
                }
            };
            let head = chain.head_index();
            let descs = match chain.collect(mem.as_ref()) {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!(error = %err, "net: rx chain collect failed");
                    break;
                }
            };
            // Build the wire payload: 12-byte virtio-net header + frame.
            let mut wire = vec![0u8; VIRTIO_NET_HDR_LEN as usize];
            wire.extend_from_slice(&frame.bytes);
            let mut written: u32 = 0;
            let mut wire_off: usize = 0;
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
                tracing::warn!(error = %err, "net: rx push_used failed");
                break;
            }
            completed = true;
        }
        if completed && let Err(e) = irq.trigger_queue() {
            tracing::warn!(error = ?e, "net: rx irq trigger failed");
        }
    }
}

impl VirtioDevice for NetDevice {
    fn device_type(&self) -> VirtioDeviceType {
        VirtioDeviceType::Net
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
        const SIZES: &[u16] = &[QUEUE_MAX_SIZE, QUEUE_MAX_SIZE];
        SIZES
    }
    fn queues(&self) -> &[Queue] {
        &self.queues
    }
    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // Config layout (virtio v1.2 § 5.1.4):
        //   0x00 u8[6] mac      (only valid if F_MAC negotiated)
        //   0x06 u16   status   (only valid if F_STATUS — we don't offer)
        //   0x0A u16   max_virtqueue_pairs (only valid if F_MQ — we don't offer)
        //   0x0C u16   mtu      (only valid if F_MTU)
        let mut full = [0u8; 16];
        if let Some(mac) = self.config.guest_mac {
            full[0..6].copy_from_slice(&mac);
        }
        if let Some(mtu) = self.config.mtu {
            full[12..14].copy_from_slice(&mtu.to_le_bytes());
        }
        let off = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = full.get(off + i).copied().unwrap_or(0);
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
                // After every TX flush, also drain RX so MMDS responses
                // injected by the interceptor reach the guest immediately
                // (rather than waiting for the next host packet to wake the
                // RX queue).
                self.drain_rx();
            }
            RX_QUEUE => self.drain_rx(),
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
        IrqLine::new(gic, IntId::from_spi_cell(17).unwrap())
    }

    fn config() -> NetConfig {
        NetConfig {
            iface_id: "eth0".into(),
            host_dev_name: "tap0".into(),
            guest_mac: Some([0x06, 0x00, 0xAC, 0x10, 0x00, 0x02]),
            mtu: Some(1500),
        }
    }

    /// Interceptor that records every TX frame it intercepts and drains
    /// from a pre-loaded RX queue.
    #[derive(Debug, Default)]
    struct ReplayInterceptor {
        intercepted: Mutex<Vec<Frame>>,
        rx_queue: Mutex<Vec<Frame>>,
    }
    impl FrameInterceptor for ReplayInterceptor {
        fn intercept_tx(&self, frame: &Frame) -> bool {
            // Anything addressed to MAC 06:01:23:45:67:01 (MMDS synthetic).
            if frame.bytes.len() >= 6
                && &frame.bytes[..6] == [0x06, 0x01, 0x23, 0x45, 0x67, 0x01].as_slice()
            {
                self.intercepted.lock().push(frame.clone());
                true
            } else {
                false
            }
        }
        fn drain_rx(&self) -> Vec<Frame> {
            std::mem::take(&mut *self.rx_queue.lock())
        }
    }

    #[test]
    fn test_should_offer_mac_feature_when_config_supplies_one() {
        let dev = NetDevice::new(
            config(),
            Arc::new(LoopbackBackend::default()),
            Arc::new(NoopInterceptor),
        );
        assert_ne!(dev.avail_features() & F_MAC, 0);
    }

    #[test]
    fn test_should_publish_mac_in_config_space() {
        let dev = NetDevice::new(
            config(),
            Arc::new(LoopbackBackend::default()),
            Arc::new(NoopInterceptor),
        );
        let mut got = [0u8; 6];
        dev.read_config(0, &mut got);
        assert_eq!(got, [0x06, 0x00, 0xAC, 0x10, 0x00, 0x02]);
    }

    /// One-way sink backend: records every `send()` for assertions. `recv()`
    /// always returns empty so the tx-then-rx drain in `process_queue` does
    /// not recycle frames back into the test.
    #[derive(Debug, Default)]
    struct CapturedBackend {
        sent: Mutex<Vec<Frame>>,
    }
    impl NetBackend for CapturedBackend {
        fn send(&self, frame: &Frame) {
            self.sent.lock().push(frame.clone());
        }
        fn recv(&self) -> Vec<Frame> {
            Vec::new()
        }
    }

    #[test]
    fn test_should_send_tx_frames_to_backend_when_no_interception() {
        let backend = Arc::new(CapturedBackend::default());
        let mut dev = NetDevice::new(config(), backend.clone(), Arc::new(NoopInterceptor));
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        let q = &mut dev.queues_mut()[TX_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        // Frame buffer at 0x4000_2000: 12-byte virtio header + 8-byte payload.
        let mut payload = vec![0u8; 12];
        payload.extend_from_slice(b"helloeth");
        mem.write(GuestAddress(0x4000_2000), &payload).unwrap();
        let base = 0x4000_0000u64;
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), payload.len() as u32)
            .unwrap();
        mem.write_u16_le(GuestAddress(base + 12), 0).unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();
        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(TX_QUEUE as u16);
        let sent = backend.sent.lock().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(&sent[0].bytes, b"helloeth");
    }

    #[test]
    fn test_should_intercept_tx_frames_when_interceptor_claims_them() {
        let backend = Arc::new(CapturedBackend::default());
        let interceptor = Arc::new(ReplayInterceptor::default());
        let mut dev = NetDevice::new(config(), backend.clone(), interceptor.clone());
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        let q = &mut dev.queues_mut()[TX_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        // Header + frame whose first 6 bytes are the MMDS synthetic MAC.
        let mut payload = vec![0u8; 12];
        payload.extend_from_slice(&[0x06, 0x01, 0x23, 0x45, 0x67, 0x01]);
        payload.extend_from_slice(b"rest");
        mem.write(GuestAddress(0x4000_2000), &payload).unwrap();
        let base = 0x4000_0000u64;
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), payload.len() as u32)
            .unwrap();
        mem.write_u16_le(GuestAddress(base + 12), 0).unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();
        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(TX_QUEUE as u16);
        // I-MMDS-1: intercepted frames MUST NOT reach the host backend.
        assert!(backend.sent.lock().is_empty());
        assert_eq!(interceptor.intercepted.lock().len(), 1);
    }

    #[test]
    fn test_should_inject_rx_frames_from_interceptor_with_virtio_header_prepended() {
        let backend = Arc::new(LoopbackBackend::default());
        let interceptor = Arc::new(ReplayInterceptor::default());
        interceptor.rx_queue.lock().push(Frame {
            bytes: b"hello-rx".to_vec(),
        });
        let mut dev = NetDevice::new(config(), backend.clone(), interceptor.clone());
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        let q = &mut dev.queues_mut()[RX_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        // Single device-write descriptor of 32 bytes at 0x4000_2000.
        let base = 0x4000_0000u64;
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), 32).unwrap();
        mem.write_u16_le(GuestAddress(base + 12), VIRTQ_DESC_F_WRITE)
            .unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();
        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(RX_QUEUE as u16);
        // First 12 bytes are the (zeroed) virtio-net header; payload follows.
        let mut got = [0u8; 20];
        mem.read(GuestAddress(0x4000_2000), &mut got).unwrap();
        assert_eq!(&got[0..12], &[0u8; 12]); // header zeroed
        assert_eq!(&got[12..20], b"hello-rx");
    }
}
