//! Per-device IRQ delivery.
//!
//! Each [`crate::transport::VirtioMmioTransport`] owns one [`IrqLine`] which
//! wraps the GIC handle and the device's INTID. On a queue-completion event
//! or a config-change event the device handler calls
//! [`IrqLine::trigger_queue`] / [`IrqLine::trigger_config`]; this:
//!
//! 1. ORs the corresponding interrupt-status bit into the atomic counter that backs the MMIO
//!    `InterruptStatus` register at offset `0x60`.
//! 2. Pulses the GIC SPI line via [`squib_gic::Gic::pulse_spi`] (D24: edge-rising on virtio-MMIO,
//!    level lines are the device's responsibility if it negotiated `VIRTIO_F_NOTIFY_ON_EMPTY`).
//!
//! The pulse + status-bit pattern is what every modern virtio guest driver
//! expects. The driver reads `InterruptStatus`, finds out whether the IRQ is
//! a config-change or a vring-completion, services the queue, and writes the
//! same bits to `InterruptACK` (offset `0x64`) to clear them.

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use squib_arch::IntId;
use squib_gic::{Gic, GicError};

/// MMIO `InterruptStatus` bit set when one or more vrings have new used
/// buffers.
pub const INT_VRING: u32 = 0x01;
/// MMIO `InterruptStatus` bit set when device config-space has changed.
pub const INT_CONFIG: u32 = 0x02;

/// IRQ delivery channel for a single virtio-MMIO device.
///
/// Cloning is cheap (`Arc` and friends); the underlying GIC handle is shared
/// across every device on the bus.
#[derive(Clone)]
pub struct IrqLine {
    gic: Arc<dyn Gic + Send + Sync>,
    intid: IntId,
    status: Arc<AtomicU32>,
}

impl std::fmt::Debug for IrqLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The `gic` field is `Arc<dyn Gic>` which doesn't impl `Debug`; we
        // intentionally omit it. `finish_non_exhaustive` makes that visible.
        f.debug_struct("IrqLine")
            .field("intid", &self.intid.as_raw())
            .field("status", &self.status.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl IrqLine {
    /// Build an IRQ line from a GIC handle and the device's INTID.
    #[must_use]
    pub fn new(gic: Arc<dyn Gic + Send + Sync>, intid: IntId) -> Self {
        Self {
            gic,
            intid,
            status: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Atomic shadow of the MMIO `InterruptStatus` register. The transport
    /// `BusDevice` reads this on `0x60` reads and clears the matching bits on
    /// `0x64` writes.
    #[must_use]
    pub fn status(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.status)
    }

    /// Mark a queue-completion event and pulse the GIC line.
    ///
    /// # Errors
    /// [`GicError`] if the GIC's pulse fails.
    pub fn trigger_queue(&self) -> Result<(), GicError> {
        self.status.fetch_or(INT_VRING, Ordering::SeqCst);
        self.gic.pulse_spi(self.intid)
    }

    /// Mark a device-config-change event and pulse the GIC line.
    ///
    /// # Errors
    /// [`GicError`] if the GIC's pulse fails.
    pub fn trigger_config(&self) -> Result<(), GicError> {
        self.status.fetch_or(INT_CONFIG, Ordering::SeqCst);
        self.gic.pulse_spi(self.intid)
    }

    /// Clear the bits the driver acked on a `0x64` write.
    pub fn ack(&self, mask: u32) {
        self.status.fetch_and(!mask, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;

    #[derive(Debug, Default)]
    struct StubGic {
        pulses: Mutex<Vec<u32>>,
    }

    impl Gic for StubGic {
        fn pulse_spi(&self, intid: IntId) -> Result<(), GicError> {
            self.pulses.lock().push(intid.as_raw());
            Ok(())
        }
        fn set_spi_level(&self, _intid: IntId, _level: bool) -> Result<(), GicError> {
            Ok(())
        }
        fn save_state(&self) -> Result<Vec<u8>, GicError> {
            Ok(Vec::new())
        }
        fn restore_state(&self, _data: &[u8]) -> Result<(), GicError> {
            Ok(())
        }
    }

    fn line() -> (IrqLine, Arc<StubGic>) {
        let gic = Arc::new(StubGic::default());
        let line = IrqLine::new(gic.clone(), IntId::from_spi_cell(0).expect("intid 0"));
        (line, gic)
    }

    #[test]
    fn test_should_set_vring_bit_and_pulse_on_trigger_queue() {
        let (line, gic) = line();
        line.trigger_queue().unwrap();
        assert_eq!(line.status().load(Ordering::SeqCst), INT_VRING);
        assert_eq!(gic.pulses.lock().len(), 1);
    }

    #[test]
    fn test_should_set_config_bit_and_pulse_on_trigger_config() {
        let (line, _gic) = line();
        line.trigger_config().unwrap();
        assert_eq!(line.status().load(Ordering::SeqCst), INT_CONFIG);
    }

    #[test]
    fn test_should_clear_only_acked_bits() {
        let (line, _) = line();
        line.trigger_queue().unwrap();
        line.trigger_config().unwrap();
        assert_eq!(line.status().load(Ordering::SeqCst), INT_VRING | INT_CONFIG);
        line.ack(INT_VRING);
        assert_eq!(line.status().load(Ordering::SeqCst), INT_CONFIG);
    }
}
