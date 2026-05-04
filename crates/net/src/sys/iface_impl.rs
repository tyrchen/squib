//! Platform-specific [`Inner`] implementing `vmnet_start_interface` /
//! `vmnet_read` / `vmnet_write` / `vmnet_stop_interface` end-to-end.
//!
//! All FFI lives here so the rest of the crate stays under
//! `#![forbid(unsafe_code)]` (I-NET-1). The safe wrapper in `iface.rs`
//! delegates to this module.

use std::sync::Arc;

use parking_lot::Mutex;

use super::{
    VmnetReturn,
    block::{StartContext, start_block, stop_block},
    dispatch::{DISPATCH_TIME_FOREVER, Queue, Semaphore, dispatch_time_now_plus_ns},
    vmnet::{
        Iovec, VmPktDesc, keys, vmnet_read, vmnet_start_interface, vmnet_stop_interface,
        vmnet_write,
    },
    xpc::XpcObject,
};

/// Per-iface stats. Read on every `recv()`-equivalent for diagnostics.
#[derive(Debug, Default, Clone, Copy)]
pub struct IfaceStats {
    /// Total bytes read from vmnet since start.
    pub rx_bytes: u64,
    /// Total bytes written to vmnet since start.
    pub tx_bytes: u64,
    /// Total frames read from vmnet since start.
    pub rx_frames: u64,
    /// Total frames written to vmnet since start.
    pub tx_frames: u64,
}

/// Errors surfaced from the vmnet binding.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InnerError {
    /// libdispatch could not allocate the per-iface serial queue, or the
    /// supplied label contained an interior NUL.
    #[error("dispatch queue allocation failed: {0}")]
    DispatchQueue(#[source] std::io::Error),

    /// `vmnet_start_interface` failed.
    #[error("vmnet_start_interface failed: code={code:?}")]
    StartFailed {
        /// Vmnet return code.
        code: VmnetReturn,
    },

    /// `vmnet_stop_interface` failed.
    #[error("vmnet_stop_interface failed: code={code:?}")]
    StopFailed {
        /// Vmnet return code.
        code: VmnetReturn,
    },

    /// `vmnet_read` failed.
    #[error("vmnet_read failed: code={code:?}")]
    ReadFailed {
        /// Vmnet return code.
        code: VmnetReturn,
    },

    /// `vmnet_write` failed.
    #[error("vmnet_write failed: code={code:?}")]
    WriteFailed {
        /// Vmnet return code.
        code: VmnetReturn,
    },

    /// Async start callback timed out before vmnet acknowledged.
    #[error("vmnet_start_interface timed out after {timeout_ms} ms")]
    StartTimeout {
        /// Wait budget that elapsed.
        timeout_ms: u64,
    },

    /// vmnet returned a parameter dictionary missing one of the documented
    /// fields.
    #[error("vmnet returned an incomplete parameter dictionary: missing {field}")]
    MissingField {
        /// XPC dictionary key that was absent.
        field: &'static str,
    },
}

/// Per-batch upper bound. vmnet's `vmpktdesc[]` is whatever length the caller
/// passes, but the spec calibrates to 32 to keep one batch fitting in cache.
pub const BATCH: usize = 32;

/// Inputs the safe wrapper hands down. Plain data; no FFI shapes leak up.
#[derive(Debug, Clone)]
pub struct StartParams {
    pub iface_id: String,
    pub mode: u64,
    pub bridged_iface_name: Option<String>,
    pub mtu: Option<u32>,
    pub start_timeout_ms: u64,
    pub enable_isolation: bool,
}

/// Active vmnet interface; FFI-handle owner.
pub struct Inner {
    /// `vmnet_interface_ref` — opaque pointer.
    handle: *mut std::os::raw::c_void,
    /// Dedicated serial dispatch queue for vmnet callbacks.
    queue: Queue,
    mtu: u32,
    max_packet_size: u32,
    mac: [u8; 6],
    stats: Arc<Mutex<IfaceStats>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmnetInner")
            .field("handle_is_null", &self.handle.is_null())
            .field("mtu", &self.mtu)
            .field("max_packet_size", &self.max_packet_size)
            .field("mac", &self.mac)
            .finish_non_exhaustive()
    }
}

// SAFETY: `vmnet_interface_ref` is reference-counted; the queue is
// libdispatch-managed. Both can cross threads.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Inner {
    pub fn start(params: &StartParams) -> Result<Self, InnerError> {
        let queue = Queue::create_serial(&format!("squib-net.{}", params.iface_id))
            .map_err(InnerError::DispatchQueue)?;

        let descriptor = build_descriptor(params);

        let semaphore = Semaphore::new();
        let ctx = StartContext::new(semaphore);
        let block = start_block(Arc::clone(&ctx));

        // SAFETY: `vmnet_start_interface` retains the descriptor and the
        // queue; the block is kept alive on this stack until the
        // semaphore wait below returns. `block2::RcBlock` produces a
        // properly-shaped Apple Block whose retain semantics libdispatch
        // and vmnet understand.
        let handle = unsafe {
            vmnet_start_interface(
                descriptor.as_ptr(),
                queue.as_ptr(),
                super::block::as_raw_start(&block),
            )
        };
        if handle.is_null() {
            return Err(InnerError::StartFailed {
                code: VmnetReturn::Failure,
            });
        }

        let timeout = if params.start_timeout_ms == 0 {
            DISPATCH_TIME_FOREVER
        } else {
            dispatch_time_now_plus_ns(params.start_timeout_ms.saturating_mul(1_000_000))
        };
        if !ctx.semaphore.wait(timeout) {
            return Err(InnerError::StartTimeout {
                timeout_ms: params.start_timeout_ms,
            });
        }

        let outcome = ctx.outcome.lock().take().ok_or(InnerError::StartFailed {
            code: VmnetReturn::Failure,
        })?;
        if outcome.status != VmnetReturn::Success {
            return Err(InnerError::StartFailed {
                code: outcome.status,
            });
        }
        let param = outcome.param.ok_or(InnerError::StartFailed {
            code: VmnetReturn::Failure,
        })?;

        let mtu = u32::try_from(param.get_uint64(keys::MTU)).unwrap_or(1500);
        let max_packet_size =
            u32::try_from(param.get_uint64(keys::MAX_PACKET_SIZE)).unwrap_or(1518);
        let mac_str = param
            .get_string(keys::MAC_ADDRESS)
            .ok_or(InnerError::MissingField {
                field: "vmnet_mac_address_key",
            })?;
        let mac = parse_mac_string(&mac_str).ok_or(InnerError::MissingField {
            field: "vmnet_mac_address_key (parse)",
        })?;
        tracing::info!(
            iface_id = %params.iface_id,
            mtu,
            max_packet_size,
            mac = %mac_str,
            "vmnet interface up"
        );

        Ok(Self {
            handle,
            queue,
            mtu,
            max_packet_size,
            mac,
            stats: Arc::new(Mutex::new(IfaceStats::default())),
        })
    }

    pub fn stop(&mut self) -> Result<(), InnerError> {
        if self.handle.is_null() {
            return Ok(());
        }
        let semaphore = Semaphore::new();
        let ctx = StartContext::new(semaphore);
        let block = stop_block(Arc::clone(&ctx));

        // SAFETY: the handle came from `vmnet_start_interface`; the queue
        // outlives the call (it's owned by `self`). The block is kept
        // alive on this stack until the semaphore wait below returns. We
        // own the only reference to `self.handle`.
        let rc = unsafe {
            vmnet_stop_interface(
                self.handle,
                self.queue.as_ptr(),
                super::block::as_raw_stop(&block),
            )
        };
        if rc != VmnetReturn::Success as u32 {
            self.handle = std::ptr::null_mut();
            return Err(InnerError::StopFailed {
                code: VmnetReturn::from_raw(rc),
            });
        }
        // Wait for callback (forever — stop is fast and not network-bound).
        ctx.semaphore.wait(DISPATCH_TIME_FOREVER);
        self.handle = std::ptr::null_mut();
        Ok(())
    }

    pub fn read_into_sized(
        &self,
        frames: &mut [&mut [u8]],
        sizes: &mut [usize],
    ) -> Result<usize, InnerError> {
        let n = frames.len().min(BATCH).min(sizes.len());
        if n == 0 || self.handle.is_null() {
            return Ok(0);
        }
        let mut iovecs: [Iovec; BATCH] = [Iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        }; BATCH];
        let mut pkts: [VmPktDesc; BATCH] = [VmPktDesc {
            vm_pkt_size: 0,
            vm_pkt_iov: std::ptr::null_mut(),
            vm_pkt_iovcnt: 0,
            vm_flags: 0,
        }; BATCH];
        for (i, buf) in frames.iter_mut().take(n).enumerate() {
            iovecs[i] = Iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            };
            pkts[i] = VmPktDesc {
                vm_pkt_size: buf.len(),
                vm_pkt_iov: &raw mut iovecs[i],
                vm_pkt_iovcnt: 1,
                vm_flags: 0,
            };
        }
        let mut count: std::os::raw::c_int = i32::try_from(n).unwrap_or(i32::MAX);
        // SAFETY: pkts/iovecs/buffers all live until the call returns
        // (we have the borrow on `frames`). vmnet writes the actual
        // frame size into `pkts[i].vm_pkt_size` and the count of
        // delivered frames into `count`.
        let rc = unsafe { vmnet_read(self.handle, pkts.as_mut_ptr(), &raw mut count) };
        if rc != VmnetReturn::Success as u32 {
            return Err(InnerError::ReadFailed {
                code: VmnetReturn::from_raw(rc),
            });
        }
        let delivered = usize::try_from(count.max(0)).unwrap_or(0).min(n);
        let mut bytes = 0u64;
        for (i, len_slot) in sizes.iter_mut().take(delivered).enumerate() {
            *len_slot = pkts[i].vm_pkt_size;
            bytes = bytes.saturating_add(*len_slot as u64);
        }
        let mut stats = self.stats.lock();
        stats.rx_bytes = stats.rx_bytes.saturating_add(bytes);
        stats.rx_frames = stats.rx_frames.saturating_add(delivered as u64);
        Ok(delivered)
    }

    pub fn write_frames(&self, frames: &[&[u8]]) -> Result<usize, InnerError> {
        let n = frames.len().min(BATCH);
        if n == 0 || self.handle.is_null() {
            return Ok(0);
        }
        let mut iovecs: [Iovec; BATCH] = [Iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        }; BATCH];
        let mut pkts: [VmPktDesc; BATCH] = [VmPktDesc {
            vm_pkt_size: 0,
            vm_pkt_iov: std::ptr::null_mut(),
            vm_pkt_iovcnt: 0,
            vm_flags: 0,
        }; BATCH];
        for (i, frame) in frames.iter().take(n).enumerate() {
            iovecs[i] = Iovec {
                iov_base: frame.as_ptr().cast::<std::os::raw::c_void>().cast_mut(),
                iov_len: frame.len(),
            };
            pkts[i] = VmPktDesc {
                vm_pkt_size: frame.len(),
                vm_pkt_iov: &raw mut iovecs[i],
                vm_pkt_iovcnt: 1,
                vm_flags: 0,
            };
        }
        let mut count: std::os::raw::c_int = i32::try_from(n).unwrap_or(i32::MAX);
        // SAFETY: pkts/iovecs/frames all live until the call returns
        // (we have the borrow on `frames`). vmnet copies the bytes
        // synchronously; no buffer escape.
        let rc = unsafe { vmnet_write(self.handle, pkts.as_mut_ptr(), &raw mut count) };
        if rc != VmnetReturn::Success as u32 {
            return Err(InnerError::WriteFailed {
                code: VmnetReturn::from_raw(rc),
            });
        }
        let delivered = usize::try_from(count.max(0)).unwrap_or(0).min(n);
        let mut bytes = 0u64;
        for frame in frames.iter().take(delivered) {
            bytes = bytes.saturating_add(frame.len() as u64);
        }
        let mut stats = self.stats.lock();
        stats.tx_bytes = stats.tx_bytes.saturating_add(bytes);
        stats.tx_frames = stats.tx_frames.saturating_add(delivered as u64);
        Ok(delivered)
    }

    pub fn mtu(&self) -> u32 {
        self.mtu
    }

    pub fn max_packet_size(&self) -> u32 {
        self.max_packet_size
    }

    pub fn host_mac(&self) -> [u8; 6] {
        self.mac
    }

    pub fn stats_snapshot(&self) -> IfaceStats {
        *self.stats.lock()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            let _ = self.stop();
        }
    }
}

fn build_descriptor(params: &StartParams) -> XpcObject {
    let dict = XpcObject::new_dictionary();
    dict.set_uint64(keys::OPERATION_MODE, params.mode);
    // `vmnet_interface_id_key` is a `uuid_t` (16 raw bytes) per
    // `<vmnet/vmnet.h>`, not a hex-string. We derive it deterministically
    // from the operator-supplied `iface_id` so snapshots round-trip and
    // so two squib instances picking the same id don't fight over a vmnet
    // interface.
    dict.set_uuid(keys::INTERFACE_ID, &iface_uuid_for(&params.iface_id));
    if let Some(mtu) = params.mtu {
        dict.set_uint64(keys::MTU, u64::from(mtu));
    }
    if let Some(name) = &params.bridged_iface_name {
        dict.set_string(keys::SHARED_INTERFACE_NAME, name);
    }
    dict.set_bool(keys::ENABLE_ISOLATION, params.enable_isolation);
    dict
}

/// Build the 16-byte `uuid_t` payload for `vmnet_interface_id_key`.
///
/// vmnet treats the value as opaque, so any deterministic-from-`iface_id`
/// derivation works. We use [`uuid::Uuid::new_v5`] over the OID namespace
/// — RFC 4122 § 4.3 — so the resulting UUID is structurally a valid v5
/// UUID (the version-5 nibble is in the right place, the variant bits are
/// set per RFC 4122). Tools like `xpc dump` that introspect the `iface_id`
/// field then surface a sensible value rather than 16 random-looking bytes.
fn iface_uuid_for(iface_id: &str) -> [u8; 16] {
    *uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, iface_id.as_bytes()).as_bytes()
}

fn parse_mac_string(raw: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    for (i, p) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_render_iface_uuid_as_16_raw_bytes() {
        let bytes = iface_uuid_for("eth0");
        assert_eq!(bytes.len(), 16);
        // Deterministic hash — same input must yield the same bytes.
        assert_eq!(iface_uuid_for("eth0"), bytes);
        assert_ne!(iface_uuid_for("eth1"), bytes);
    }

    #[test]
    fn test_should_parse_mac_string() {
        let mac = parse_mac_string("06:00:ac:10:00:02").unwrap();
        assert_eq!(mac, [0x06, 0x00, 0xAC, 0x10, 0x00, 0x02]);
    }
}
