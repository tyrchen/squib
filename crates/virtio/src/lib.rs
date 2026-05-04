//! virtio-MMIO transport and per-device drivers for squib.
//!
//! This crate ports the virtio-MMIO state machine and per-device frontends
//! described in [14-virtio-and-devices.md](../../../specs/14-virtio-and-devices.md).
//! The transport speaks the [virtio v1.2 MMIO register layout][spec] and
//! adapts it to squib-native abstractions:
//!
//! - [`squib_bus::BusDevice`] for the MMIO-routed register surface.
//! - [`squib_core::GuestMemory`] for descriptor / payload reads and writes.
//! - [`squib_gic::Gic::pulse_spi`] for edge-rising IRQ delivery (D24).
//!
//! The crate intentionally avoids `vmm-sys-util`, `kvm-ioctls`, and the
//! upstream Linux-flavoured `EventFd` plumbing — squib-vmm uses Tokio
//! channels for cross-thread queue notifications and the GIC's `pulse_spi` is
//! synchronous through `Arc<dyn Gic>`. See I-CRATE-3 in
//! [61-crates-and-features.md § 7](../../../specs/61-crates-and-features.md#7-invariants).
//!
//! ## Module layout
//!
//! | Module | Role |
//! |--------|------|
//! | [`device`] | Per-device-type trait surface ([`device::VirtioDevice`]) |
//! | [`device_id`] | Standard virtio device IDs (block=2, net=1, vsock=19, ...) |
//! | [`device_status`] | Status bits driving the driver-init state machine |
//! | [`feature_bits`] | Common feature bits (VIRTIO_F_VERSION_1, ...) |
//! | [`interrupt`] | [`interrupt::IrqLine`] — wraps `Gic + IntId` for the device side |
//! | [`queue`] | Virtqueue: descriptors, avail/used rings, [`queue::DescriptorChain`] |
//! | [`slot`] | MMIO-slot allocator (32-slot ceiling, base `0x0F00_0000 + slot * 0x1000`) |
//! | [`transport`] | virtio-MMIO `BusDevice`: register layout + driver-init state machine |
//! | [`devices`] | Per-device frontends: block, net, vsock, balloon, rng, console, pmem, mem, boot-timer |
//!
//! [spec]: https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-1340002

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// virtio wire shapes pin every register width: descriptor.len is u32,
// queue_size is u16, MMIO register payloads are u32. Casting between u64
// host computations and these wire widths is the device contract — we keep
// the casts where they belong and trust the type system at the boundary
// (validation runs in `Queue::set_size` etc.). The clippy pedantic
// truncation/precision lints are too noisy to be useful here; the
// `cast_possible_truncation` lint is informative for app code, not for
// wire-shape code.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::similar_names
)]

pub mod device;
pub mod device_id;
pub mod device_status;
pub mod devices;
pub mod error;
pub mod feature_bits;
pub mod interrupt;
pub mod queue;
pub mod slot;
pub mod transport;

pub use device::{ActivateError, VirtioDevice};
pub use error::VirtioError;
pub use interrupt::IrqLine;
pub use queue::{DescriptorChain, MAX_QUEUE_SIZE, Queue, QueueError, QueueIndex};
pub use slot::{MMIO_SLOT_COUNT, Slot, SlotAllocator, VIRTIO_MMIO_BASE, VIRTIO_MMIO_REGION_SIZE};
pub use transport::{VIRTIO_MMIO_REGION_BYTES, VirtioMmioTransport};
