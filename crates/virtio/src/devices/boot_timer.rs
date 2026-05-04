//! Boot-timer — squib-internal latency device.
//!
//! Per [14-virtio-and-devices.md § 4.8](../../../specs/14-virtio-and-devices.md#48-boot-timer):
//! a trivial virtio device with no queues. The device exposes a 4-byte
//! config-space register at offset `0x100`; on guest read, the register
//! returns `(now_monotonic_ns − boot_start_ns)` truncated to `u32`
//! microseconds (saturating at `u32::MAX` ≈ 71 minutes).
//!
//! `boot_start_ns` is captured at the same point the kernel-load completes,
//! so the value the guest reads after `init` runs is "time from the moment
//! the kernel got control to the moment Linux first scheduled userspace."
//! Used by the `boot.rs` criterion benchmark in
//! [71-performance-budgets.md § 7](../../../specs/71-performance-budgets.md#7-bench-harness).

use std::{sync::Arc, time::Instant};

use parking_lot::Mutex;
use squib_core::GuestMemory;

use crate::{
    device::{ActivateError, VirtioDevice},
    device_id::VirtioDeviceType,
    interrupt::IrqLine,
    queue::Queue,
};

/// virtio-boot-timer device.
#[derive(Debug)]
pub struct BootTimerDevice {
    avail: u64,
    acked: u64,
    queues: Vec<Queue>,
    /// Captured at construction; this is the closest meaningful approximation
    /// to "kernel load completed". The VMM creates the device immediately
    /// after writing the kernel into guest RAM so the value is accurate to
    /// well under a millisecond.
    boot_start: Instant,
    state: Arc<Mutex<ActiveState>>,
}

#[derive(Debug, Default)]
struct ActiveState {
    activated: bool,
}

impl BootTimerDevice {
    /// Build a boot-timer with the start instant captured now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            avail: 0,
            acked: 0,
            queues: Vec::new(),
            boot_start: Instant::now(),
            state: Arc::new(Mutex::new(ActiveState::default())),
        }
    }

    /// Microseconds since `boot_start`, saturated at `u32::MAX`.
    fn elapsed_us(&self) -> u32 {
        let elapsed_us = self.boot_start.elapsed().as_micros();
        u32::try_from(elapsed_us.min(u128::from(u32::MAX))).unwrap_or(u32::MAX)
    }
}

impl Default for BootTimerDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioDevice for BootTimerDevice {
    fn device_type(&self) -> VirtioDeviceType {
        VirtioDeviceType::BootTimer
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
        &[]
    }
    fn queues(&self) -> &[Queue] {
        &self.queues
    }
    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // Single 4-byte register at offset 0 of config-space (== MMIO 0x100).
        // Reads at any other offset return zero so the guest driver's probe
        // doesn't spuriously panic on a wider read.
        if offset == 0 && data.len() == 4 {
            data.copy_from_slice(&self.elapsed_us().to_le_bytes());
            return;
        }
        for b in data.iter_mut() {
            *b = 0;
        }
    }
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // Read-only register; the spec wording in 14 § 4.8 doesn't define
        // a write side, so we silently drop.
    }
    fn activate(&mut self, _mem: Arc<dyn GuestMemory>, _irq: IrqLine) -> Result<(), ActivateError> {
        self.state.lock().activated = true;
        Ok(())
    }
    fn is_activated(&self) -> bool {
        self.state.lock().activated
    }
    fn process_queue(&mut self, _queue_index: u16) {
        // No queues; nothing to process.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_have_zero_queues() {
        let dev = BootTimerDevice::new();
        assert_eq!(dev.queue_max_sizes().len(), 0);
        assert_eq!(dev.queues().len(), 0);
    }

    #[test]
    fn test_should_return_increasing_microseconds_on_consecutive_reads() {
        let dev = BootTimerDevice::new();
        let mut a = [0u8; 4];
        let mut b = [0u8; 4];
        dev.read_config(0, &mut a);
        std::thread::sleep(std::time::Duration::from_millis(2));
        dev.read_config(0, &mut b);
        let av = u32::from_le_bytes(a);
        let bv = u32::from_le_bytes(b);
        assert!(bv >= av);
    }

    #[test]
    fn test_should_return_zero_on_invalid_offset_or_length() {
        let dev = BootTimerDevice::new();
        let mut buf = [0xAAu8; 8];
        dev.read_config(0x10, &mut buf);
        assert_eq!(buf, [0u8; 8]);
    }
}
