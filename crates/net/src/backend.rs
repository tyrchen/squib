//! `NetBackend` glue between the virtio-net frontend and the host networking
//! plumbing in `squib-net`.
//!
//! The virtio-net frontend (in `squib-virtio::devices::net`) drives a
//! [`squib_virtio::devices::net::NetBackend`] trait — `send(&Frame)` plus
//! `recv() -> Vec<Frame>`. This module supplies three implementations:
//!
//! - [`VmnetHostBackend`] — bridges to a [`VmnetIface`] (shared / host / bridged modes).
//! - [`LoopbackHostBackend`] — re-exposes virtio-net's loopback for the case "no network
//!   configured" without forcing the device-manager to pick the type.
//! - [`crate::gvproxy::GvproxyBackend`] — owned by [`crate::gvproxy`] and re-exposed through
//!   [`NetHostBackend::Gvproxy`] for ergonomic dispatch.

#![forbid(unsafe_code)]

use std::sync::Arc;

use parking_lot::Mutex;
use squib_virtio::devices::net::{Frame, LoopbackBackend, NetBackend};

use crate::{
    gvproxy::GvproxyBackend,
    iface::{IfaceError, VmnetIface},
};

/// Send-side queue depth (frames buffered for the next `vmnet_write` batch).
const TX_BATCH_DEPTH: usize = 32;

/// Per-recv RX batch size. Matches [`crate::iface::BATCH`] (32) — vmnet
/// returns up to N frames per call. We allocate buffers per call rather
/// than maintaining a pool; the proper [`bytes::BytesMut`] pool is a
/// Phase 7 perf-tuning concern (I-NET-4).
const RX_BATCH_DEPTH: usize = 32;

/// Vmnet-backed host implementation. Pulls frames from vmnet on `recv()`
/// and pushes them through `vmnet_write` on `send()`.
#[derive(Debug)]
pub struct VmnetHostBackend {
    iface: Arc<VmnetIface>,
    tx_queue: Mutex<Vec<Frame>>,
}

impl VmnetHostBackend {
    /// Wrap a started [`VmnetIface`].
    #[must_use]
    pub fn new(iface: VmnetIface) -> Self {
        Self {
            iface: Arc::new(iface),
            tx_queue: Mutex::new(Vec::with_capacity(TX_BATCH_DEPTH)),
        }
    }

    /// Borrow the underlying interface (used by snapshot save to read MTU /
    /// stats; never to mutate vmnet directly).
    #[must_use]
    pub fn iface(&self) -> &VmnetIface {
        &self.iface
    }

    /// Drain the TX queue into vmnet in one `vmnet_write` batch. Called
    /// implicitly on every `send()` and on every `recv()` so frames don't
    /// sit longer than one virtio queue notification.
    fn flush_tx(&self) -> Result<(), IfaceError> {
        let mut guard = self.tx_queue.lock();
        if guard.is_empty() {
            return Ok(());
        }
        let pending: Vec<Frame> = std::mem::take(&mut *guard);
        drop(guard);
        let slices: Vec<&[u8]> = pending.iter().map(|f| f.bytes.as_ref()).collect();
        for chunk in slices.chunks(crate::iface::BATCH) {
            self.iface.write(chunk)?;
        }
        Ok(())
    }
}

impl NetBackend for VmnetHostBackend {
    fn send(&self, frame: &Frame) {
        {
            let mut guard = self.tx_queue.lock();
            guard.push(frame.clone());
            if guard.len() < TX_BATCH_DEPTH {
                return;
            }
        }
        if let Err(err) = self.flush_tx() {
            tracing::warn!(error = %err, "vmnet write batch failed; dropping frames");
        }
    }

    fn recv(&self) -> Vec<Frame> {
        // Flush any pending TX first — keeps round-trip latency tight.
        if let Err(err) = self.flush_tx() {
            tracing::warn!(error = %err, "vmnet flush_tx failed");
        }
        let mtu = (self.iface.mtu() as usize).max(1500).saturating_add(32);
        let mut storage: Vec<Vec<u8>> = (0..RX_BATCH_DEPTH).map(|_| vec![0u8; mtu]).collect();
        let mut views: Vec<&mut [u8]> = storage.iter_mut().map(Vec::as_mut_slice).collect();
        let mut sizes = vec![0usize; views.len()];
        let n = match self.iface.read(&mut views[..], &mut sizes[..]) {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(error = %err, "vmnet read failed");
                return Vec::new();
            }
        };
        let mut frames = Vec::with_capacity(n);
        for (i, mut buf) in storage.into_iter().enumerate().take(n) {
            let len = sizes[i].min(buf.len());
            buf.truncate(len);
            frames.push(Frame::from_bytes(bytes::Bytes::from(buf)));
        }
        frames
    }
}

/// Loopback backend re-exported for symmetry. Identical semantics to the
/// virtio-net's own [`LoopbackBackend`] — every send becomes a recv on the
/// next drain. Used as the default when no host networking is configured
/// (so MMDS still has a place to deliver responses).
#[derive(Debug, Default)]
pub struct LoopbackHostBackend {
    inner: LoopbackBackend,
}

impl NetBackend for LoopbackHostBackend {
    fn send(&self, frame: &Frame) {
        self.inner.send(frame);
    }
    fn recv(&self) -> Vec<Frame> {
        self.inner.recv()
    }
}

/// Tagged enum so `device_manager` can return a single `Arc<dyn NetBackend>`
/// regardless of which mode the operator picked.
#[derive(Debug)]
#[non_exhaustive]
pub enum NetHostBackend {
    /// Vmnet shared/host/bridged mode.
    Vmnet(VmnetHostBackend),
    /// gvproxy userspace mode.
    Gvproxy(GvproxyBackend),
    /// Loopback (no host networking) — kept so MMDS still works.
    Loopback(LoopbackHostBackend),
}

impl NetBackend for NetHostBackend {
    fn send(&self, frame: &Frame) {
        match self {
            Self::Vmnet(b) => b.send(frame),
            Self::Gvproxy(b) => b.send(frame),
            Self::Loopback(b) => b.send(frame),
        }
    }
    fn recv(&self) -> Vec<Frame> {
        match self {
            Self::Vmnet(b) => b.recv(),
            Self::Gvproxy(b) => b.recv(),
            Self::Loopback(b) => b.recv(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_loopback_round_trip_through_net_host_backend() {
        let backend = NetHostBackend::Loopback(LoopbackHostBackend::default());
        backend.send(&Frame::from_slice(b"hello"));
        let got = backend.recv();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].bytes.as_ref(), b"hello");
    }
}
