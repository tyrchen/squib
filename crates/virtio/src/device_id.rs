//! Standard virtio device-type IDs.
//!
//! Sourced from the [virtio v1.2 specification][spec] § 5 ("Device Types").
//! These values are wire-stable; the guest driver consults the MMIO `DeviceID`
//! register at offset `0x08` and matches against this list to decide which
//! device frontend to bind.
//!
//! [spec]: https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-1930005

/// Type-safe wrapper around a virtio device-type ID.
///
/// `VirtioDeviceType::const_id()` exposes the raw `u32` for the MMIO
/// `DeviceID` register; matching is by associated constant rather than raw
/// integer so a typo in the device frontend is a compile error, not a silent
/// "no driver bound".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum VirtioDeviceType {
    /// virtio-net (network frontend).
    Net = 1,
    /// virtio-block (storage frontend).
    Block = 2,
    /// virtio-console (serial frontend).
    Console = 3,
    /// virtio-rng (entropy source frontend).
    Rng = 4,
    /// virtio-balloon (memory ballooning frontend).
    Balloon = 5,
    /// virtio-vsock (host-guest socket transport).
    Vsock = 19,
    /// virtio-pmem (persistent memory frontend).
    Pmem = 27,
    /// virtio-mem (memory hotplug frontend).
    Mem = 24,
    /// boot-timer (squib extension; uses unassigned-by-virtio ID `0xFFFF_FF00`).
    ///
    /// Per [14-virtio-and-devices.md § 4.8](../../../specs/14-virtio-and-devices.md#48-boot-timer)
    /// boot-timer is a squib-internal device with no virtio queues; the ID is
    /// chosen well outside the assigned range so it can never collide with a
    /// future virtio standard device type.
    BootTimer = 0xFFFF_FF00,
}

impl VirtioDeviceType {
    /// Raw `u32` for the MMIO `DeviceID` register.
    #[must_use]
    pub const fn const_id(self) -> u32 {
        self as u32
    }

    /// Human-readable label for log lines and `Debug`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Net => "net",
            Self::Block => "block",
            Self::Console => "console",
            Self::Rng => "rng",
            Self::Balloon => "balloon",
            Self::Vsock => "vsock",
            Self::Pmem => "pmem",
            Self::Mem => "mem",
            Self::BootTimer => "boot-timer",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_use_canonical_virtio_ids_for_standard_devices() {
        assert_eq!(VirtioDeviceType::Net.const_id(), 1);
        assert_eq!(VirtioDeviceType::Block.const_id(), 2);
        assert_eq!(VirtioDeviceType::Console.const_id(), 3);
        assert_eq!(VirtioDeviceType::Rng.const_id(), 4);
        assert_eq!(VirtioDeviceType::Balloon.const_id(), 5);
        assert_eq!(VirtioDeviceType::Vsock.const_id(), 19);
        assert_eq!(VirtioDeviceType::Pmem.const_id(), 27);
        assert_eq!(VirtioDeviceType::Mem.const_id(), 24);
    }

    #[test]
    fn test_should_use_squib_extension_id_outside_assigned_range_for_boot_timer() {
        // The virtio v1.2 assigned-IDs range tops out at low triple digits;
        // 0xFFFF_FF00 is well outside, so a future spec addition can never
        // collide.
        assert!(VirtioDeviceType::BootTimer.const_id() > 1024);
    }
}
