//! Per-device-type trait surface.
//!
//! Every virtio device frontend (block, net, vsock, etc.) implements
//! [`VirtioDevice`]. The transport in [`crate::transport`] owns the device
//! behind `Arc<Mutex<dyn VirtioDevice>>` and forwards register reads, queue
//! notifications, and lifecycle events to it.
//!
//! Squib's trait is intentionally smaller than upstream Firecracker's:
//! we don't expose an `EventFd` per queue (we use the GIC pulse directly),
//! we don't surface `MutEventSubscriber` (Tokio drives the event loop), and
//! we don't ship `prepare_save` until snapshots land (Phase 5).

use std::sync::Arc;

use squib_core::GuestMemory;
use thiserror::Error;

use crate::{device_id::VirtioDeviceType, interrupt::IrqLine, queue::Queue};

/// Errors a device can surface during activation. Activation runs once per
/// device, on the driver's `Status |= DRIVER_OK` write.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ActivateError {
    /// Device backend (host file, socket) is unavailable.
    #[error("device backend unavailable: {0}")]
    BackendUnavailable(String),
    /// Driver acked features that are mutually exclusive with the device's
    /// configured backend.
    #[error("incompatible feature set: {0}")]
    IncompatibleFeatures(&'static str),
    /// Device-specific activation error; carries a free-form message so
    /// device modules don't need their own error subtypes.
    #[error("device activation failed: {0}")]
    Other(String),
}

/// Per-device-type contract.
///
/// Implementors hold their own state behind a `parking_lot::Mutex` (the
/// transport already serializes access through `Arc<Mutex<dyn VirtioDevice>>`,
/// so the inner state needs no second lock). Implementors MUST NOT block on
/// I/O inside any method other than `process_queue` — every other method runs
/// under the per-device mutex and a stall freezes the bus.
pub trait VirtioDevice: Send + std::fmt::Debug {
    /// Device-type discriminant for the MMIO `DeviceID` register.
    fn device_type(&self) -> VirtioDeviceType;

    /// Bitmap of features the device offers. The transport ANDs this with
    /// the driver's `acked_features` and surfaces the negotiated set.
    fn avail_features(&self) -> u64;

    /// Bitmap of features the driver has acknowledged.
    fn acked_features(&self) -> u64;

    /// Set the driver-acked feature set. The transport calls this once per
    /// MMIO `DriverFeatures` write; implementors typically just store the
    /// value.
    fn set_acked_features(&mut self, value: u64);

    /// Per-queue maximum descriptor counts. The vector length determines how
    /// many vrings the device exposes; the value at each index becomes
    /// `QueueNumMax` for that queue.
    fn queue_max_sizes(&self) -> &[u16];

    /// Borrow the device's queue state.
    fn queues(&self) -> &[Queue];

    /// Borrow the device's queue state mutably (transport uses this when the
    /// driver writes queue-config registers).
    fn queues_mut(&mut self) -> &mut [Queue];

    /// Read a slice of the device's config space at `offset`.
    fn read_config(&self, offset: u64, data: &mut [u8]);

    /// Write a slice into the device's config space at `offset`. Per the
    /// virtio spec § 2.4.2, drivers SHOULD not write config-space after
    /// `DRIVER_OK`; the transport gates on this and silently drops post-OK
    /// writes via [`crate::transport::VirtioMmioTransport::accept_config_write`].
    fn write_config(&mut self, offset: u64, data: &[u8]);

    /// Take ownership of the resources needed to drive the device live.
    /// Called exactly once on the driver's `DRIVER_OK` transition.
    ///
    /// # Errors
    /// [`ActivateError`] for any backend / configuration failure.
    fn activate(&mut self, mem: Arc<dyn GuestMemory>, irq: IrqLine) -> Result<(), ActivateError>;

    /// `true` once `activate` has succeeded.
    fn is_activated(&self) -> bool;

    /// Handler invoked on a guest write to the MMIO `QueueNotify` register.
    ///
    /// `queue_index` matches `Queue::queues()[queue_index]`. Implementations
    /// drain the queue's available ring, process the descriptors, and push
    /// back into the used ring — typically by spawning the work on a Tokio
    /// task and returning fast.
    fn process_queue(&mut self, queue_index: u16);
}
