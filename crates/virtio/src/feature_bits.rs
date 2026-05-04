//! Common virtio feature bits.
//!
//! Per [virtio v1.2 § 6 ("Reserved Feature Bits")][spec]. Only the bits squib
//! actually negotiates appear here; per-device features live alongside their
//! respective device modules in [`crate::devices`].
//!
//! Each constant carries the **shift** in the section header so the doc
//! tracks the actual `1 << N` value below it.
//!
//! [spec]: https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-2680006

/// `24` — `VIRTIO_F_NOTIFY_ON_EMPTY`. Device sends interrupt when the queue
/// is fully consumed (used in legacy block; squib's edge-pulse model
/// trivially satisfies it).
pub const NOTIFY_ON_EMPTY: u64 = 1 << 24;
/// `27` — `VIRTIO_F_ANY_LAYOUT`. Driver may pack request headers and bodies
/// into a single descriptor; required for modern net / block.
pub const ANY_LAYOUT: u64 = 1 << 27;
/// `32` — `VIRTIO_F_VERSION_1`. The device offers v1.0+ semantics. Squib
/// **always** offers this; legacy mode is not supported.
pub const VERSION_1: u64 = 1 << 32;
/// `33` — `VIRTIO_F_ACCESS_PLATFORM`. Required when the platform interposes
/// IOMMU or memory protection. Off on squib (HVF stage-2 is direct-mapped).
pub const ACCESS_PLATFORM: u64 = 1 << 33;
/// `34` — `VIRTIO_F_RING_PACKED`. Packed-ring layout. Off on squib for 1.0;
/// classic split-ring keeps the queue handler simple.
pub const RING_PACKED: u64 = 1 << 34;
/// `35` — `VIRTIO_F_IN_ORDER`. Used buffers ack in submission order.
pub const IN_ORDER: u64 = 1 << 35;
/// `36` — `VIRTIO_F_ORDER_PLATFORM`. Memory ordering follows platform rules.
pub const ORDER_PLATFORM: u64 = 1 << 36;
/// `38` — `VIRTIO_F_NOTIFICATION_DATA`. Driver writes notification data
/// alongside the queue index.
pub const NOTIFICATION_DATA: u64 = 1 << 38;
