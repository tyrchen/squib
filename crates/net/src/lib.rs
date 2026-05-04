//! squib-net — host-side networking for virtio-net.
//!
//! Per [30-networking.md](../../../specs/30-networking.md), squib offers four host-side
//! attachment modes:
//!
//! | Mode | Backend | Entitlement |
//! |------|---------|-------------|
//! | [`VmnetMode::Shared`] (default) | `vmnet.framework` `VMNET_SHARED_MODE` (NAT) | none beyond `com.apple.security.hypervisor` |
//! | [`VmnetMode::Host`] | `vmnet.framework` `VMNET_HOST_MODE` (host-only) | none beyond `com.apple.security.hypervisor` |
//! | `VmnetMode::Bridged` (cargo feature) | `vmnet.framework` `VMNET_BRIDGED_MODE` | `com.apple.vm.networking` (restricted) — gated by the `bridged` cargo feature |
//! | [`NetMode::Userspace`] | bundled `gvproxy` over a `socketpair(2)` carrying length-prefixed L2 frames | none |
//!
//! ## Layout
//!
//! - [`sys`] — the second of two `unsafe` boundaries in the workspace (the first is `squib-hv`).
//!   Hand-rolled FFI to `vmnet.framework`, libdispatch, and XPC. Per [99-key-decisions.md §
//!   D13](../../../specs/99-key-decisions.md#d13-vmnet-via-hand-rolled-ffi).
//! - [`iface`] — safe `VmnetIface` wrapper.
//! - [`backend`] — `NetBackend` impls feeding the virtio-net frontend.
//! - [`gvproxy`] — gvproxy child-process management for `--network=userspace`.
//!
//! Outside the [`sys`] module, `#![forbid(unsafe_code)]` holds for the rest of the
//! crate (I-NET-1). I-NET-4 (pre-allocated [`bytes::BytesMut`] pool, no per-packet
//! `Vec<u8>` alloc in the hot path) lands with the Phase 7 perf-tuning sweep — see
//! `specs/93-improvements-review.md` for the deferred backlog entry.

#![warn(missing_docs)]
// virtio-net descriptor lengths are u32; we cast to usize for slice indexing. Same
// pattern as `squib-virtio` — wire shapes pin the widths.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::similar_names,
    // Module-name repetition (`vmnet::vmnet_*`) is the spec's naming, not noise.
    clippy::module_name_repetitions
)]

pub mod backend;
pub mod gvproxy;
pub mod iface;
pub mod mode;
pub mod sys;

pub use backend::{LoopbackHostBackend, NetHostBackend, VmnetHostBackend};
pub use gvproxy::{GvproxyBackend, GvproxyError, GvproxyParams};
pub use iface::{IfaceError, IfaceStats, InterfaceParams, VmnetIface};
pub use mode::{NetMode, VmnetMode};
