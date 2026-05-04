//! Unsafe boundary for vmnet/dispatch/XPC FFI.
//!
//! This module contains every `unsafe` block in `squib-net`. Per
//! [70-security.md § 3.2](../../../specs/70-security.md#32-squib-netsys) the budget
//! is ~300 lines of `unsafe`, all annotated with `// SAFETY:` comments referencing
//! the framework contract they rely on.
//!
//! The submodules split the framework surface:
//!
//! - [`block`] — minimal Apple Block ABI shim. Handles the single block we pass to
//!   `vmnet_start_interface` / `vmnet_stop_interface` (a callback with no captures by using a
//!   `&'static` global state), plus a tiny inline trampoline.
//! - [`dispatch`] — `dispatch_queue_t` / `dispatch_semaphore_t` wrappers.
//! - [`xpc`] — XPC dictionary helpers (build the `vmnet_*` parameter dictionary).
//! - [`vmnet`] — the four `vmnet_*` calls.
//!
//! Outside this module the rest of `squib-net` runs under
//! `#![forbid(unsafe_code)]` (I-NET-1).

// We deliberately enumerate `unsafe` here rather than `forbid`-ing it.
#![allow(unsafe_code)]
// FFI bindings carry `c_int` / `c_void` / `*mut T` shapes that pedantic clippy
// flags as "missing safety doc" or "needless cast". Each unsafe block has its
// own SAFETY comment; per-call lints would be noise.
#![allow(
    clippy::missing_safety_doc,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::missing_const_for_fn,
    clippy::needless_pass_by_value
)]
// FFI internal-surface methods are documented at the module level. Marking each
// raw extern fn with a doc comment doubles the noise without adding signal — they
// are the syscall-shape contract, not user-facing API.
#![allow(missing_docs)]

#[cfg(target_os = "macos")]
pub mod block;
#[cfg(target_os = "macos")]
pub mod dispatch;
#[cfg(target_os = "macos")]
pub mod iface_impl;
#[cfg(target_os = "macos")]
pub mod vmnet;
#[cfg(target_os = "macos")]
pub mod xpc;

/// `vmnet_return_t` from `<vmnet/vmnet.h>`. Pinned as a `u32` to match the
/// Darwin ABI; values are stable across macOS versions.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VmnetReturn {
    /// `VMNET_SUCCESS`
    Success = 1000,
    /// `VMNET_FAILURE`
    Failure = 1001,
    /// `VMNET_MEM_FAILURE`
    MemFailure = 1002,
    /// `VMNET_INVALID_ARGUMENT`
    InvalidArgument = 1003,
    /// `VMNET_SETUP_INCOMPLETE`
    SetupIncomplete = 1004,
    /// `VMNET_INVALID_ACCESS`
    InvalidAccess = 1005,
    /// `VMNET_PACKET_TOO_BIG`
    PacketTooBig = 1006,
    /// `VMNET_BUFFER_EXHAUSTED`
    BufferExhausted = 1007,
    /// `VMNET_TOO_MANY_PACKETS`
    TooManyPackets = 1008,
    /// `VMNET_SHARING_SERVICE_BUSY`
    SharingServiceBusy = 1009,
    /// Anything else — opaque integer preserved for diagnostics.
    Unknown = u32::MAX,
}

impl VmnetReturn {
    /// Decode a raw `vmnet_return_t` integer, preserving unknown variants as
    /// [`Self::Unknown`].
    #[must_use]
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            1000 => Self::Success,
            1001 => Self::Failure,
            1002 => Self::MemFailure,
            1003 => Self::InvalidArgument,
            1004 => Self::SetupIncomplete,
            1005 => Self::InvalidAccess,
            1006 => Self::PacketTooBig,
            1007 => Self::BufferExhausted,
            1008 => Self::TooManyPackets,
            1009 => Self::SharingServiceBusy,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_decode_known_vmnet_return_codes() {
        assert_eq!(VmnetReturn::from_raw(1000), VmnetReturn::Success);
        assert_eq!(VmnetReturn::from_raw(1001), VmnetReturn::Failure);
        assert_eq!(VmnetReturn::from_raw(1009), VmnetReturn::SharingServiceBusy);
    }

    #[test]
    fn test_should_preserve_unknown_vmnet_return_codes() {
        assert_eq!(VmnetReturn::from_raw(99_999), VmnetReturn::Unknown);
    }
}
