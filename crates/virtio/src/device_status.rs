//! virtio device-status bits.
//!
//! Defined in the [virtio v1.2 specification][spec] § 2.1; written by the
//! guest driver into the MMIO `Status` register at offset `0x70`. Squib's
//! transport state machine in [`crate::transport`] enforces the canonical
//! transition graph and rejects illegal moves — the device cannot be
//! activated until the driver has acked features and posted `DRIVER_OK`.
//!
//! [spec]: https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-110001

/// `0` — fresh / reset device. The driver writes this to reset.
pub const INIT: u32 = 0;
/// `1` — driver has noticed the device.
pub const ACKNOWLEDGE: u32 = 1;
/// `2` — driver knows how to drive the device.
pub const DRIVER: u32 = 2;
/// `4` — driver is up; queues are live.
pub const DRIVER_OK: u32 = 4;
/// `8` — driver has acked features; features are now negotiated.
pub const FEATURES_OK: u32 = 8;
/// `64` — device has noticed an error; driver should reset.
pub const DEVICE_NEEDS_RESET: u32 = 64;
/// `128` — driver has noticed an error; device is unrecoverable until reset.
pub const FAILED: u32 = 128;
