//! `vmnet.framework` FFI: the four entry points squib needs.
//!
//! Per [70-security.md § 3.2](../../../specs/70-security.md#32-squib-netsys):
//! `vmnet_start_interface`, `vmnet_read`, `vmnet_write`, `vmnet_stop_interface`
//! against a serial dispatch queue. ~300-line cap on `unsafe` enforced by the
//! crate's module layout.

use std::os::raw::{c_int, c_void};

use block2::Block;

/// Block signature for `vmnet_start_interface_completion_handler_t`:
/// `void (^)(vmnet_return_t status, xpc_object_t interface_param)`.
pub type StartHandler = dyn Fn(u32, *mut c_void) + 'static;
/// Block signature for `vmnet_interface_completion_handler_t`:
/// `void (^)(vmnet_return_t status)`.
pub type StopHandler = dyn Fn(u32) + 'static;

#[link(name = "vmnet", kind = "framework")]
unsafe extern "C" {
    pub fn vmnet_start_interface(
        interface_desc: *mut c_void,
        queue: *mut c_void,
        handler: *const Block<StartHandler>,
    ) -> *mut c_void;

    pub fn vmnet_stop_interface(
        interface: *mut c_void,
        queue: *mut c_void,
        handler: *const Block<StopHandler>,
    ) -> u32;

    pub fn vmnet_read(interface: *mut c_void, packets: *mut VmPktDesc, pktcnt: *mut c_int) -> u32;

    pub fn vmnet_write(interface: *mut c_void, packets: *mut VmPktDesc, pktcnt: *mut c_int) -> u32;
}

/// `iovec` from `<sys/uio.h>`. Same layout on Darwin `x86_64` / `aarch64`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Iovec {
    /// Pointer to the buffer.
    pub iov_base: *mut c_void,
    /// Number of bytes addressable starting at `iov_base`.
    pub iov_len: libc::size_t,
}

/// `vmpktdesc` from `<vmnet/vmnet.h>`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VmPktDesc {
    /// Total bytes in this packet (across all iovecs). Caller initialises
    /// to the remaining capacity for read; vmnet writes the actual length.
    pub vm_pkt_size: libc::size_t,
    /// Pointer to the iovec array.
    pub vm_pkt_iov: *mut Iovec,
    /// Number of iovecs.
    pub vm_pkt_iovcnt: u32,
    /// Per-packet flags (unused by squib).
    pub vm_flags: u32,
}

// SAFETY: `VmPktDesc` is a plain repr(C) struct — Send/Sync are sound for the
// outer pointer; the inner pointers (`iov_base` etc.) are managed by the caller
// keeping the underlying buffers alive for the duration of the FFI call.
unsafe impl Send for VmPktDesc {}
unsafe impl Sync for VmPktDesc {}

/// Documented vmnet keys from `<vmnet/vmnet.h>`. The C *variable* name carries a
/// `_key` suffix (`vmnet_operation_mode_key` etc.), but each variable points
/// to the actual key string **without** the suffix (`"vmnet_operation_mode"`).
/// Verified against the system framework on macOS 15.x. Encoded as `&CStr`
/// literals so the xpc-dictionary helpers in [`super::xpc`] can pass them to
/// the FFI without a runtime `CString::new`.
pub mod keys {
    use std::ffi::CStr;

    /// `vmnet_operation_mode_key` → `"vmnet_operation_mode"`
    pub const OPERATION_MODE: &CStr = c"vmnet_operation_mode";
    /// `vmnet_interface_id_key` → `"vmnet_interface_id"`
    pub const INTERFACE_ID: &CStr = c"vmnet_interface_id";
    /// `vmnet_mtu_key` → `"vmnet_mtu"`
    pub const MTU: &CStr = c"vmnet_mtu";
    /// `vmnet_max_packet_size_key` → `"vmnet_max_packet_size"`
    pub const MAX_PACKET_SIZE: &CStr = c"vmnet_max_packet_size";
    /// `vmnet_mac_address_key` → `"vmnet_mac_address"`
    pub const MAC_ADDRESS: &CStr = c"vmnet_mac_address";
    /// `vmnet_enable_isolation_key` → `"vmnet_enable_isolation"`
    pub const ENABLE_ISOLATION: &CStr = c"vmnet_enable_isolation";
    /// `vmnet_shared_interface_name_key` → `"vmnet_shared_interface_name"` (bridged)
    pub const SHARED_INTERFACE_NAME: &CStr = c"vmnet_shared_interface_name";
}
