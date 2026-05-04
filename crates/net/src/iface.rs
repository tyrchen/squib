//! Safe wrapper over the vmnet FFI: [`VmnetIface`].
//!
//! Per [30-networking.md § 3](../../../specs/30-networking.md#3-vmnet-binding-squib-netsys):
//!
//! ```rust,ignore
//! pub struct VmnetIface {
//!     handle: vmnet_interface_ref,
//!     queue:  dispatch_queue_t,
//!     mtu:    u32,
//!     mac:    [u8; 6],
//! }
//! ```
//!
//! - [`VmnetIface::start`] runs `vmnet_start_interface` and waits on a `dispatch_semaphore` for the
//!   async callback. The negotiated MTU, max packet size, and MAC are stashed for downstream use.
//! - [`VmnetIface::read`] / [`VmnetIface::write`] batch up to 32 frames per call (per [71 §
//!   5](../../../specs/71-performance-budgets.md#5-network-throughput)).
//! - [`VmnetIface::stop`] runs the symmetric stop callback.
//!
//! On non-macOS hosts every method returns [`IfaceError::NotSupported`]; the
//! type compiles so `squib-vmm`'s wiring remains target-agnostic. The
//! `target_os = "macos"` cfg gates the actual FFI plumbing.
//!
//! All FFI lives in [`crate::sys::iface_impl`]; this module only delegates,
//! so it stays under `#![forbid(unsafe_code)]` (I-NET-1).

#![forbid(unsafe_code)]

use std::time::Duration;

use thiserror::Error;
use tracing::{debug, warn};

use crate::mode::VmnetMode;
#[cfg(target_os = "macos")]
pub use crate::sys::iface_impl::{BATCH, IfaceStats};

/// Per-batch upper bound when not on macOS — the constant compiles so callers
/// see the same shape on every platform.
#[cfg(not(target_os = "macos"))]
pub const BATCH: usize = 32;

/// Per-iface stats placeholder for non-macOS targets.
#[cfg(not(target_os = "macos"))]
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
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IfaceError {
    /// `vmnet.framework` is not available on this platform (non-macOS).
    #[error("vmnet not supported on this platform")]
    NotSupported,

    /// macOS-specific FFI error.
    #[cfg(target_os = "macos")]
    #[error(transparent)]
    Inner(#[from] crate::sys::iface_impl::InnerError),
}

/// Default `vmnet_start_interface` callback timeout. Vmnet typically
/// completes within tens of milliseconds; 5 s is a generous watchdog.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Inputs for [`VmnetIface::start`].
#[derive(Debug, Clone)]
pub struct InterfaceParams {
    /// `iface_id` from the `/network-interfaces/{id}` PUT body. Used to derive
    /// the deterministic vmnet handle name.
    pub iface_id: String,
    /// Vmnet operating mode.
    pub mode: VmnetMode,
    /// Optional bridged-mode physical interface name (e.g. `en0`). Ignored
    /// for shared / host modes.
    pub bridged_iface_name: Option<String>,
    /// Optional explicit MTU; defaults to vmnet's reported MTU when `None`.
    pub mtu: Option<u32>,
    /// Wait budget for the async start callback.
    pub start_timeout: Duration,
    /// Whether to enable inter-VM isolation (`vmnet_enable_isolation_key`).
    /// Default `true` — Lambda-shaped guests should not see each other.
    pub enable_isolation: bool,
}

impl InterfaceParams {
    /// Convenience constructor with the defaults a Phase-4 caller wants.
    #[must_use]
    pub fn new(iface_id: impl Into<String>, mode: VmnetMode) -> Self {
        Self {
            iface_id: iface_id.into(),
            mode,
            bridged_iface_name: None,
            mtu: None,
            start_timeout: DEFAULT_START_TIMEOUT,
            enable_isolation: true,
        }
    }
}

/// Vmnet interface handle. macOS-only; on other targets all methods return
/// [`IfaceError::NotSupported`] but the type is constructible-by-name so
/// downstream code remains target-agnostic in its types.
#[derive(Debug)]
pub struct VmnetIface {
    iface_id: String,
    #[cfg(target_os = "macos")]
    inner: Option<crate::sys::iface_impl::Inner>,
    #[cfg(not(target_os = "macos"))]
    _phantom: std::marker::PhantomData<()>,
}

impl VmnetIface {
    /// Start the interface, blocking until vmnet's async `vmnet_start_interface`
    /// callback fires (or the timeout elapses).
    pub fn start(params: InterfaceParams) -> Result<Self, IfaceError> {
        #[cfg(target_os = "macos")]
        {
            let timeout_ms = u64::try_from(params.start_timeout.as_millis()).unwrap_or(u64::MAX);
            let inner_params = crate::sys::iface_impl::StartParams {
                iface_id: params.iface_id.clone(),
                mode: params.mode.as_xpc_value(),
                bridged_iface_name: params.bridged_iface_name,
                mtu: params.mtu,
                start_timeout_ms: timeout_ms,
                enable_isolation: params.enable_isolation,
            };
            let inner = crate::sys::iface_impl::Inner::start(&inner_params)?;
            debug!(
                iface_id = %params.iface_id,
                mtu = inner.mtu(),
                "vmnet iface started"
            );
            Ok(Self {
                iface_id: params.iface_id,
                inner: Some(inner),
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = params;
            Err(IfaceError::NotSupported)
        }
    }

    /// Operator-supplied iface id (used in logs and the `host_dev_name` mapping).
    #[must_use]
    pub fn iface_id(&self) -> &str {
        &self.iface_id
    }

    /// Negotiated MTU (defaulted to 1500 if vmnet did not return one).
    #[must_use]
    pub fn mtu(&self) -> u32 {
        #[cfg(target_os = "macos")]
        {
            self.inner
                .as_ref()
                .map_or(1500, crate::sys::iface_impl::Inner::mtu)
        }
        #[cfg(not(target_os = "macos"))]
        {
            1500
        }
    }

    /// Vmnet-assigned MAC for the host side of the interface. Distinct from
    /// the guest MAC (which lives in the virtio-net config space).
    #[must_use]
    pub fn host_mac(&self) -> [u8; 6] {
        #[cfg(target_os = "macos")]
        {
            self.inner
                .as_ref()
                .map_or([0; 6], crate::sys::iface_impl::Inner::host_mac)
        }
        #[cfg(not(target_os = "macos"))]
        {
            [0; 6]
        }
    }

    /// Read up to `BATCH` frames into the supplied buffers. Returns the
    /// number of frames delivered; the actual size of each frame is
    /// reported via `sizes[i]`.
    pub fn read(&self, frames: &mut [&mut [u8]], sizes: &mut [usize]) -> Result<usize, IfaceError> {
        #[cfg(target_os = "macos")]
        {
            self.inner.as_ref().map_or_else(
                || Err(IfaceError::NotSupported),
                |inner| Ok(inner.read_into_sized(frames, sizes)?),
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (frames, sizes);
            Err(IfaceError::NotSupported)
        }
    }

    /// Write up to `BATCH` frames. Returns the number actually accepted
    /// by vmnet (typically all of them; vmnet may decline the tail under
    /// transient buffer pressure).
    pub fn write(&self, frames: &[&[u8]]) -> Result<usize, IfaceError> {
        #[cfg(target_os = "macos")]
        {
            self.inner.as_ref().map_or_else(
                || Err(IfaceError::NotSupported),
                |inner| Ok(inner.write_frames(frames)?),
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = frames;
            Err(IfaceError::NotSupported)
        }
    }

    /// Snapshot of the cumulative stats.
    #[must_use]
    pub fn stats(&self) -> IfaceStats {
        #[cfg(target_os = "macos")]
        {
            self.inner.as_ref().map_or_else(
                IfaceStats::default,
                crate::sys::iface_impl::Inner::stats_snapshot,
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            IfaceStats::default()
        }
    }

    /// Stop the interface. Idempotent; safe to call from `Drop`.
    pub fn stop(&mut self) -> Result<(), IfaceError> {
        #[cfg(target_os = "macos")]
        {
            if let Some(mut inner) = self.inner.take() {
                inner.stop()?;
            }
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(())
        }
    }
}

impl Drop for VmnetIface {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            warn!(iface_id = %self.iface_id, error = %e, "vmnet stop on Drop failed");
        }
    }
}
