//! virtio-MMIO transport.
//!
//! Implements the v1.2 register layout and driver-init state machine on top
//! of [`squib_bus::BusDevice`]. The transport is the only crate-public
//! contact surface between the bus dispatcher and the device frontend; once
//! the driver hits `DRIVER_OK`, the transport calls [`crate::VirtioDevice::activate`]
//! exactly once, threading the [`squib_core::GuestMemory`] handle and an
//! [`crate::IrqLine`] through.
//!
//! ## Register layout (offsets, all little-endian u32)
//!
//! | Offset | Name | RW |
//! |--------|------|----|
//! | 0x00 | MagicValue (`0x7472_6976` = "virt") | R |
//! | 0x04 | Version (= 2) | R |
//! | 0x08 | DeviceID | R |
//! | 0x0C | VendorID | R |
//! | 0x10 | DeviceFeatures (page-selected) | R |
//! | 0x14 | DeviceFeaturesSel | W |
//! | 0x20 | DriverFeatures (page-selected) | W |
//! | 0x24 | DriverFeaturesSel | W |
//! | 0x30 | QueueSel | W |
//! | 0x34 | QueueNumMax | R |
//! | 0x38 | QueueNum | W |
//! | 0x44 | QueueReady | RW |
//! | 0x50 | QueueNotify | W |
//! | 0x60 | InterruptStatus | R |
//! | 0x64 | InterruptACK | W |
//! | 0x70 | Status | RW |
//! | 0x80 | QueueDescLow | W |
//! | 0x84 | QueueDescHigh | W |
//! | 0x90 | QueueDriverLow (avail) | W |
//! | 0x94 | QueueDriverHigh (avail) | W |
//! | 0xA0 | QueueDeviceLow (used) | W |
//! | 0xA4 | QueueDeviceHigh (used) | W |
//! | 0xFC | ConfigGeneration | R |
//! | 0x100..0xFFF | Device-specific config space | RW (gated on state) |

use std::sync::{Arc, atomic::Ordering};

use parking_lot::Mutex;
use squib_bus::BusDevice;
use squib_core::{GuestAddress, GuestMemory};
use tracing::{trace, warn};

use crate::{device::VirtioDevice, device_status, feature_bits, interrupt::IrqLine};

/// `MagicValue` in the MMIO config region (little-endian "virt").
const MMIO_MAGIC_VALUE: u32 = 0x7472_6976;
/// virtio-MMIO transport version (v1.0 modern).
const MMIO_VERSION: u32 = 2;
/// `VendorID` placeholder; matches Firecracker's choice.
const VENDOR_ID: u32 = 0;
/// Total bytes the transport claims on the bus per slot. Matches the
/// per-slot allocator stride in [`crate::slot::VIRTIO_MMIO_REGION_SIZE`].
pub const VIRTIO_MMIO_REGION_BYTES: u64 = 0x1000;

/// `BusDevice` implementation of the virtio-MMIO transport.
///
/// Holds the underlying `VirtioDevice` behind `Arc<Mutex<dyn VirtioDevice>>`;
/// the bus locks the transport while dispatching reads/writes, the transport
/// then locks the device when it has to forward.
pub struct VirtioMmioTransport {
    device: Arc<Mutex<dyn VirtioDevice>>,
    label: String,
    mem: Arc<dyn GuestMemory>,
    irq: IrqLine,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_select: u32,
    status: u32,
    config_generation: u32,
}

impl std::fmt::Debug for VirtioMmioTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioMmioTransport")
            .field("label", &self.label)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl VirtioMmioTransport {
    /// Build a transport over `device`. The transport takes a strong ref to
    /// the guest memory and a clone of the IRQ line; both are forwarded to
    /// the device on `activate`.
    #[must_use]
    pub fn new(
        device: Arc<Mutex<dyn VirtioDevice>>,
        mem: Arc<dyn GuestMemory>,
        irq: IrqLine,
    ) -> Self {
        let label = {
            let dev = device.lock();
            format!("virtio-{}", dev.device_type().label())
        };
        Self {
            device,
            label,
            mem,
            irq,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_select: 0,
            status: device_status::INIT,
            config_generation: 0,
        }
    }

    /// Current driver-init state.
    #[must_use]
    pub fn status(&self) -> u32 {
        self.status
    }

    /// `true` if device-config writes are allowed in the current state. Per
    /// virtio spec § 2.4.2, the driver MAY write config-space pre-`DRIVER_OK`
    /// and SHOULD NOT after. Squib enforces SHOULD as MUST: post-`DRIVER_OK`
    /// writes are silently dropped, matching Firecracker behaviour.
    #[must_use]
    pub fn accept_config_write(&self) -> bool {
        (self.status & device_status::DRIVER_OK) == 0
    }

    fn read_u32_register(&self, offset: u64) -> u32 {
        match offset {
            0x00 => MMIO_MAGIC_VALUE,
            0x04 => MMIO_VERSION,
            0x08 => self.device.lock().device_type().const_id(),
            0x0C => VENDOR_ID,
            0x10 => self.device_features_page(),
            0x34 => self.queue_field(|q| u32::from(q.max_size)),
            0x38 => self.queue_field(|q| u32::from(q.size)),
            0x44 => self.queue_field(|q| u32::from(q.ready)),
            0x60 => self.irq.status().load(Ordering::SeqCst),
            0x70 => self.status,
            0xFC => self.config_generation,
            _ => {
                warn!(label = %self.label, offset = format!("{offset:#x}"), "unknown virtio mmio read");
                0
            }
        }
    }

    fn device_features_page(&self) -> u32 {
        let dev = self.device.lock();
        let mut features = dev.avail_features();
        // VIRTIO_F_VERSION_1 is always offered; we never speak the legacy
        // protocol.
        features |= feature_bits::VERSION_1;
        match self.device_features_sel {
            0 => features as u32,
            1 => (features >> 32) as u32,
            other => {
                warn!(label = %self.label, page = other, "DeviceFeatures page out of range");
                0
            }
        }
    }

    fn queue_field<F: FnOnce(&crate::queue::Queue) -> u32>(&self, f: F) -> u32 {
        let dev = self.device.lock();
        dev.queues()
            .get(self.queue_select as usize)
            .map(f)
            .unwrap_or_default()
    }

    fn write_register(&mut self, offset: u64, value: u32) {
        match offset {
            0x14 => self.device_features_sel = value,
            0x20 => self.handle_driver_features_write(value),
            0x24 => self.driver_features_sel = value,
            0x30 => self.queue_select = value,
            0x38 => self.update_queue_field(|q| q.set_size(value as u16)),
            0x44 => self.update_queue_field(|q| q.ready = value == 1),
            0x50 => self.handle_queue_notify(value as u16),
            0x64 => self.irq.ack(value),
            0x70 => self.set_status(value),
            0x80 => self.update_queue_field(|q| {
                let lo = u64::from(value);
                let hi = q.desc_table_addr.raw() & !0xFFFF_FFFF;
                q.desc_table_addr = GuestAddress(hi | lo);
            }),
            0x84 => self.update_queue_field(|q| {
                let hi = u64::from(value) << 32;
                let lo = q.desc_table_addr.raw() & 0xFFFF_FFFF;
                q.desc_table_addr = GuestAddress(hi | lo);
            }),
            0x90 => self.update_queue_field(|q| {
                let lo = u64::from(value);
                let hi = q.avail_ring_addr.raw() & !0xFFFF_FFFF;
                q.avail_ring_addr = GuestAddress(hi | lo);
            }),
            0x94 => self.update_queue_field(|q| {
                let hi = u64::from(value) << 32;
                let lo = q.avail_ring_addr.raw() & 0xFFFF_FFFF;
                q.avail_ring_addr = GuestAddress(hi | lo);
            }),
            0xA0 => self.update_queue_field(|q| {
                let lo = u64::from(value);
                let hi = q.used_ring_addr.raw() & !0xFFFF_FFFF;
                q.used_ring_addr = GuestAddress(hi | lo);
            }),
            0xA4 => self.update_queue_field(|q| {
                let hi = u64::from(value) << 32;
                let lo = q.used_ring_addr.raw() & 0xFFFF_FFFF;
                q.used_ring_addr = GuestAddress(hi | lo);
            }),
            _ => {
                warn!(label = %self.label, offset = format!("{offset:#x}"), value = format!("{value:#x}"), "unknown virtio mmio write");
            }
        }
    }

    fn handle_driver_features_write(&mut self, value: u32) {
        // The driver writes one page (32 bits) at a time; OR each into the
        // negotiated set, masked by what the device actually offers.
        let dev = self.device.lock();
        let avail = dev.avail_features() | feature_bits::VERSION_1;
        drop(dev);
        let shifted = match self.driver_features_sel {
            0 => u64::from(value),
            1 => u64::from(value) << 32,
            _ => return,
        };
        let masked = shifted & avail;
        self.driver_features |= masked;
        let mut dev = self.device.lock();
        dev.set_acked_features(self.driver_features);
    }

    fn update_queue_field<F: FnOnce(&mut crate::queue::Queue)>(&self, f: F) {
        // Per virtio spec § 4.2.2.2, queue config writes after DRIVER_OK
        // are invalid. Squib silently drops them — Firecracker behaviour.
        if (self.status & device_status::DRIVER_OK) != 0 {
            warn!(label = %self.label, "queue-config write after DRIVER_OK ignored");
            return;
        }
        let mut dev = self.device.lock();
        if let Some(q) = dev.queues_mut().get_mut(self.queue_select as usize) {
            f(q);
        }
    }

    fn handle_queue_notify(&self, queue_index: u16) {
        if (self.status & device_status::DRIVER_OK) == 0 {
            warn!(label = %self.label, queue = queue_index, "QueueNotify before DRIVER_OK ignored");
            return;
        }
        trace!(label = %self.label, queue = queue_index, "QueueNotify");
        let mut dev = self.device.lock();
        dev.process_queue(queue_index);
    }

    fn set_status(&mut self, value: u32) {
        if value == device_status::INIT {
            self.reset();
            return;
        }
        if value & device_status::FAILED != 0 {
            self.status |= device_status::FAILED;
            return;
        }
        if !is_valid_transition(self.status, value) {
            warn!(label = %self.label, from = self.status, to = value, "invalid driver-init transition ignored");
            return;
        }
        self.status = value;
        if (value & device_status::DRIVER_OK) != 0 {
            self.activate_device();
        }
    }

    fn activate_device(&mut self) {
        let mut dev = self.device.lock();
        if dev.is_activated() {
            return;
        }
        if let Err(err) = dev.activate(Arc::clone(&self.mem), self.irq.clone()) {
            warn!(label = %self.label, error = %err, "device activation failed");
            self.status |= device_status::DEVICE_NEEDS_RESET;
        }
    }

    fn reset(&mut self) {
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.queue_select = 0;
        self.status = device_status::INIT;
        // `config_generation` is only bumped when device config-space
        // contents change (per virtio v1.2 § 4.2.2.4). A driver reset does
        // not invalidate config-space coherency on its own — the next
        // device-side change does.
        let mut dev = self.device.lock();
        for q in dev.queues_mut() {
            q.reset();
        }
        dev.set_acked_features(0);
    }
}

/// `true` if the driver-init state machine accepts a transition from `from`
/// to `to`. Per virtio spec § 2.1.1.
fn is_valid_transition(from: u32, to: u32) -> bool {
    use device_status::{ACKNOWLEDGE, DRIVER, DRIVER_OK, FEATURES_OK, INIT};
    const VALID: &[(u32, u32)] = &[
        (INIT, ACKNOWLEDGE),
        (ACKNOWLEDGE, ACKNOWLEDGE | DRIVER),
        (ACKNOWLEDGE | DRIVER, ACKNOWLEDGE | DRIVER | FEATURES_OK),
        (
            ACKNOWLEDGE | DRIVER | FEATURES_OK,
            ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK,
        ),
    ];
    VALID.iter().any(|&(f, t)| f == from && t == to)
}

impl BusDevice for VirtioMmioTransport {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        match offset {
            0x00..=0xFF if data.len() == 4 => {
                let v = self.read_u32_register(offset);
                data.copy_from_slice(&v.to_le_bytes());
            }
            0x100..=0xFFF => {
                let dev = self.device.lock();
                dev.read_config(offset - 0x100, data);
            }
            _ => {
                warn!(label = %self.label, offset = format!("{offset:#x}"), len = data.len(), "invalid virtio mmio read shape");
                for b in data.iter_mut() {
                    *b = 0;
                }
            }
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        match offset {
            0x00..=0xFF if data.len() == 4 => {
                let v = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                self.write_register(offset, v);
            }
            0x100..=0xFFF => {
                if self.accept_config_write() {
                    let mut dev = self.device.lock();
                    dev.write_config(offset - 0x100, data);
                } else {
                    warn!(label = %self.label, offset = format!("{offset:#x}"), "config-space write after DRIVER_OK ignored");
                }
            }
            _ => {
                warn!(label = %self.label, offset = format!("{offset:#x}"), len = data.len(), "invalid virtio mmio write shape");
            }
        }
    }

    fn debug_label(&self) -> &str {
        &self.label
    }
}

#[cfg(test)]
mod tests {
    use squib_arch::IntId;
    use squib_core::SliceGuestMemory;
    use squib_gic::Gic;

    use super::*;
    use crate::{
        device::ActivateError,
        device_id::VirtioDeviceType,
        device_status::{ACKNOWLEDGE, DRIVER, DRIVER_OK, FEATURES_OK, INIT},
        queue::Queue,
    };

    #[derive(Debug)]
    struct StubDevice {
        avail: u64,
        acked: u64,
        queues: Vec<Queue>,
        config: Vec<u8>,
        activated: bool,
        notifications: Vec<u16>,
        /// When set, `activate` returns an error — used to drive the
        /// `DEVICE_NEEDS_RESET` test below.
        fail_activate: bool,
    }

    impl StubDevice {
        fn new() -> Self {
            Self {
                avail: feature_bits::ANY_LAYOUT,
                acked: 0,
                queues: vec![Queue::new(64), Queue::new(64)],
                config: vec![0xAA; 16],
                activated: false,
                notifications: Vec::new(),
                fail_activate: false,
            }
        }
    }

    impl VirtioDevice for StubDevice {
        fn device_type(&self) -> VirtioDeviceType {
            VirtioDeviceType::Rng
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
            &[64, 64]
        }
        fn queues(&self) -> &[Queue] {
            &self.queues
        }
        fn queues_mut(&mut self) -> &mut [Queue] {
            &mut self.queues
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            let off = offset as usize;
            for (i, b) in data.iter_mut().enumerate() {
                *b = self.config.get(off + i).copied().unwrap_or(0);
            }
        }
        fn write_config(&mut self, offset: u64, data: &[u8]) {
            let off = offset as usize;
            for (i, b) in data.iter().enumerate() {
                if let Some(slot) = self.config.get_mut(off + i) {
                    *slot = *b;
                }
            }
        }
        fn activate(
            &mut self,
            _mem: Arc<dyn GuestMemory>,
            _irq: IrqLine,
        ) -> Result<(), ActivateError> {
            if self.fail_activate {
                return Err(ActivateError::Other(
                    "stub-injected activation failure".to_string(),
                ));
            }
            self.activated = true;
            Ok(())
        }
        fn is_activated(&self) -> bool {
            self.activated
        }
        fn process_queue(&mut self, queue_index: u16) {
            self.notifications.push(queue_index);
        }
    }

    #[derive(Debug, Default)]
    struct StubGic;
    impl Gic for StubGic {
        fn pulse_spi(&self, _intid: IntId) -> Result<(), squib_gic::GicError> {
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

    fn build() -> (
        VirtioMmioTransport,
        Arc<Mutex<StubDevice>>,
        Arc<dyn GuestMemory>,
    ) {
        let dev = Arc::new(Mutex::new(StubDevice::new()));
        let dev_dyn: Arc<Mutex<dyn VirtioDevice>> = dev.clone();
        let mem: Arc<dyn GuestMemory> = Arc::new(SliceGuestMemory::new(GuestAddress(0), 0x1_0000));
        let gic: Arc<dyn Gic + Send + Sync> = Arc::new(StubGic);
        let irq = IrqLine::new(gic, IntId::from_spi_cell(16).unwrap());
        let transport = VirtioMmioTransport::new(dev_dyn, mem.clone(), irq);
        (transport, dev, mem)
    }

    fn read32(t: &mut VirtioMmioTransport, off: u64) -> u32 {
        let mut buf = [0u8; 4];
        t.read(off, &mut buf);
        u32::from_le_bytes(buf)
    }

    fn write32(t: &mut VirtioMmioTransport, off: u64, v: u32) {
        t.write(off, &v.to_le_bytes());
    }

    #[test]
    fn test_should_expose_magic_value_at_offset_0() {
        let (mut t, _, _) = build();
        assert_eq!(read32(&mut t, 0x00), 0x7472_6976);
    }

    #[test]
    fn test_should_expose_version_2_at_offset_4() {
        let (mut t, _, _) = build();
        assert_eq!(read32(&mut t, 0x04), 2);
    }

    #[test]
    fn test_should_expose_device_id_from_device() {
        let (mut t, _, _) = build();
        assert_eq!(read32(&mut t, 0x08), VirtioDeviceType::Rng.const_id());
    }

    #[test]
    fn test_should_offer_version_1_in_features_page_1() {
        let (mut t, _, _) = build();
        // Page 1 must include VERSION_1 even if the device doesn't.
        write32(&mut t, 0x14, 1);
        assert_eq!(read32(&mut t, 0x10), 1); // bit 32 → bit 0 of page 1.
    }

    /// Per virtio v1.2 § 2.1.6 — when the device cannot complete activation, the
    /// transport surfaces this to the driver by OR-ing `DEVICE_NEEDS_RESET` into
    /// the status register. The driver-init state-machine bits remain set so the
    /// driver can distinguish "I drove the state machine correctly but the device
    /// rejected activation" from "I made an invalid transition".
    #[test]
    fn test_should_or_device_needs_reset_when_activation_fails() {
        let dev = Arc::new(Mutex::new({
            let mut d = StubDevice::new();
            d.fail_activate = true;
            d
        }));
        let dev_dyn: Arc<Mutex<dyn VirtioDevice>> = dev.clone();
        let mem: Arc<dyn GuestMemory> = Arc::new(SliceGuestMemory::new(GuestAddress(0), 0x1_0000));
        let gic: Arc<dyn Gic + Send + Sync> = Arc::new(StubGic);
        let irq = IrqLine::new(gic, IntId::from_spi_cell(16).unwrap());
        let mut t = VirtioMmioTransport::new(dev_dyn, mem, irq);

        write32(&mut t, 0x70, ACKNOWLEDGE);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);

        // Activation was attempted: stub returned Err, so the device is not
        // marked activated and the status register OR's in DEVICE_NEEDS_RESET.
        assert!(!dev.lock().activated, "stub-Err must keep activated=false");
        let status = read32(&mut t, 0x70);
        assert!(
            status & device_status::DEVICE_NEEDS_RESET != 0,
            "DEVICE_NEEDS_RESET must be set on activation failure, got {status:#x}"
        );
        // The driver-init bits remain set so the driver sees the full
        // `(ACK | DRIVER | FEATURES_OK | DRIVER_OK | DEVICE_NEEDS_RESET)` shape
        // virtio v1.2 § 2.1 § 2.1.6 prescribes.
        assert_eq!(
            status,
            ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK | device_status::DEVICE_NEEDS_RESET,
            "status must carry init-machine bits + DEVICE_NEEDS_RESET",
        );
    }

    #[test]
    fn test_should_advance_through_full_driver_init_state_machine() {
        let (mut t, dev, _) = build();
        write32(&mut t, 0x70, ACKNOWLEDGE);
        assert_eq!(t.status(), ACKNOWLEDGE);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER);
        assert_eq!(t.status(), ACKNOWLEDGE | DRIVER);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        assert_eq!(t.status(), ACKNOWLEDGE | DRIVER | FEATURES_OK);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
        assert!(dev.lock().is_activated());
    }

    #[test]
    fn test_should_reject_transition_to_features_ok_without_driver() {
        let (mut t, _, _) = build();
        write32(&mut t, 0x70, ACKNOWLEDGE);
        // INIT-friendly transitions only allow ACKNOWLEDGE | DRIVER next.
        write32(&mut t, 0x70, FEATURES_OK);
        assert_eq!(t.status(), ACKNOWLEDGE);
    }

    #[test]
    fn test_should_drop_config_writes_after_driver_ok() {
        let (mut t, dev, _) = build();
        // Pre-DRIVER_OK config write goes through.
        t.write(0x100, &[0x55]);
        assert_eq!(dev.lock().config[0], 0x55);
        // Drive to DRIVER_OK.
        write32(&mut t, 0x70, ACKNOWLEDGE);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
        // Post-DRIVER_OK config write is dropped.
        t.write(0x100, &[0x99]);
        assert_eq!(dev.lock().config[0], 0x55);
    }

    #[test]
    fn test_should_compose_64bit_descriptor_address_from_two_writes() {
        let (mut t, dev, _) = build();
        write32(&mut t, 0x30, 0); // queue 0
        write32(&mut t, 0x80, 0xCAFE_BABE); // desc lo
        write32(&mut t, 0x84, 0x1234_5678); // desc hi
        let addr = dev.lock().queues[0].desc_table_addr.raw();
        assert_eq!(addr, 0x1234_5678_CAFE_BABE);
    }

    #[test]
    fn test_should_dispatch_queue_notify_to_device_handler_only_after_driver_ok() {
        let (mut t, dev, _) = build();
        write32(&mut t, 0x50, 0);
        assert_eq!(dev.lock().notifications.len(), 0);
        write32(&mut t, 0x70, ACKNOWLEDGE);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        write32(&mut t, 0x70, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
        write32(&mut t, 0x50, 1);
        assert_eq!(dev.lock().notifications, vec![1]);
    }

    #[test]
    fn test_should_reset_state_on_status_init_write() {
        let (mut t, _, _) = build();
        write32(&mut t, 0x70, ACKNOWLEDGE);
        write32(&mut t, 0x70, INIT);
        assert_eq!(t.status(), INIT);
    }

    #[test]
    fn test_should_ack_interrupt_status_bits_on_write_to_0x64() {
        let (mut t, _, _) = build();
        // Manually trip a status bit then ack it.
        t.irq.status().store(0x03, Ordering::SeqCst);
        write32(&mut t, 0x64, 0x01);
        assert_eq!(t.irq.status().load(Ordering::SeqCst), 0x02);
    }

    #[test]
    fn test_should_mask_driver_features_to_avail_features() {
        let (mut t, dev, _) = build();
        // Driver acks page 0 with all bits set; device only offers
        // ANY_LAYOUT (bit 27).
        write32(&mut t, 0x24, 0); // DriverFeaturesSel = 0
        write32(&mut t, 0x20, 0xFFFF_FFFF);
        let acked = dev.lock().acked_features();
        assert_eq!(acked, feature_bits::ANY_LAYOUT);
    }

    #[test]
    fn test_should_treat_4byte_register_reads_outside_known_offsets_as_zero() {
        let (mut t, _, _) = build();
        // 0xF8 has no defined register in v2.
        assert_eq!(read32(&mut t, 0xF8), 0);
    }
}
