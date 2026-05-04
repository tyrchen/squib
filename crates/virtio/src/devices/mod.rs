//! Per-device frontends.
//!
//! Each module here implements [`crate::VirtioDevice`] for one virtio device
//! type. Per [14-virtio-and-devices.md §
//! 4](../../../specs/14-virtio-and-devices.md#4-device-catalogue):
//!
//! | Module | Spec § |
//! |--------|--------|
//! | [`block`] | 4.1 |
//! | [`net`] | 4.2 |
//! | [`vsock`] | 4.3 (TSI off by default per D8) |
//! | [`balloon`] | 4.4 |
//! | [`rng`] | 4.5 |
//! | [`console`] | 4.6 |
//! | [`pmem`] | 4.7 |
//! | [`mem`] | 4.7 |
//! | [`boot_timer`] | 4.8 |

pub mod balloon;
pub mod block;
pub mod boot_timer;
pub mod console;
pub mod mem;
pub mod net;
pub mod pmem;
pub mod rng;
pub mod vsock;
