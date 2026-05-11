//! UDS-backed `VsockMuxer` implementation.
//!
//! Wires virtio-vsock packets produced by [`squib_virtio::devices::vsock::VsockDevice`]
//! through Unix domain sockets at `{uds_path}_<port>`, one listener per
//! configured host-initiated port. The wire layer is byte-for-byte
//! Firecracker-compatible: the host dials `{uds_path}_<port>`, writes
//! `CONNECT <port>\n`, gets back `OK <host_port>\n`, and then streams
//! bytes in both directions until either end closes.
//!
//! ## Threading shape
//!
//! The muxer is shared behind `Arc<UdsVsockMuxer>` between three worlds:
//!
//! - The **API → VMM event loop**, which constructs the muxer and passes it into
//!   [`squib_virtio::devices::vsock::VsockDevice::new`].
//! - The **virtio device thread**, which calls [`VsockMuxer::handle_tx`] on every guest-originated
//!   packet and [`VsockMuxer::drain_rx`] whenever the guest posts RX descriptors. Both calls come
//!   in synchronously on the vCPU thread — the trait shape is packet-at-a-time, no awaits.
//! - The **async I/O tasks** (one per accepted UDS connection) that read bytes off the host socket
//!   and convert them into `VsockOp::Rw` packets, and write guest-originated `Rw` payloads back out
//!   to the host socket.
//!
//! The three worlds exchange state through:
//! - `Mutex<BTreeMap<ConnectionKey, Connection>>` — routing table keyed on `(host_port,
//!   guest_port)`. Per-connection state.
//! - `mpsc::UnboundedSender<Vec<u8>>` per connection — inbound bytes the async reader hands off to
//!   the sync `handle_tx`/`drain_rx` boundary.
//! - `Arc<Notify>` — fires whenever a new RX packet lands in the shared queue, so the VMM event
//!   loop can kick [`squib_virtio::devices::vsock::VsockDevice::process_queue`] via its own path.
//!
//! ## Flow-control
//!
//! Stream vsock uses a credit-based scheme (`buf_alloc`, `fwd_cnt`).
//! For tok's cold-path traffic the payloads are small (KiB–MiB range)
//! and the guest's stock Linux vsock driver is well-behaved; we emit
//! 1 MiB of advertised credit per direction and bump `fwd_cnt` on
//! every byte we forward. A production-grade credit scheduler is
//! tracked in spec `93-improvements-review.md`.

#![cfg(target_os = "macos")]
// Packet-level code deliberately mixes u16/u32/u64 casts; the wire
// shape pins widths. Same lints the upstream vsock module disables.
// Additional allow-list: the long async handlers map cleanly to the
// Firecracker wire protocol — splitting them hides the protocol flow.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    // Pre-boot listener bind() uses the std blocking remove_file; the
    // async tokio equivalent is unnecessary since the wrapper runs
    // outside any tokio task at construction.
    clippy::disallowed_methods
)]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use squib_virtio::devices::vsock::{
    TYPE_STREAM, VMADDR_CID_HOST, VsockHeader, VsockMuxer, VsockOp, VsockPacket,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Notify, mpsc, oneshot},
};

/// Default per-direction credit advertised to the guest (and back). 1
/// MiB matches Firecracker's default; the guest's vsock driver takes
/// this as a hint to size its send buffer.
const DEFAULT_BUF_ALLOC: u32 = 1024 * 1024;

/// Max bytes we read from a UDS connection per virtio RW packet. Staying
/// well under common MTU-ish boundaries keeps queue fill rates sane.
const RW_CHUNK_BYTES: usize = 4096;

/// Unique key for an open host↔guest connection.
///
/// `host_port` is a monotonic allocator value (host-side) that we
/// synthesise when accepting a UDS connection. `guest_port` is the
/// `CONNECT <port>` the host sent. The pair uniquely identifies the
/// vsock stream inside the guest's router.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
struct ConnectionKey {
    host_port: u32,
    guest_port: u32,
}

/// Per-connection state kept in the routing table.
struct Connection {
    /// Sender half: packets the guest sent *into* this connection
    /// land as payload bytes on this channel, drained by the async
    /// writer task.
    guest_to_host_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// One-shot notification fired exactly once when the guest sends
    /// the `Response` packet establishing this stream. The
    /// `handle_host_connection` task awaits this before sending the
    /// `OK <host_port>\n` line back to the UDS client.
    ///
    /// Event-driven so we don't miss the signal when the guest's
    /// entire response (Response + Rw + Shutdown) completes inside a
    /// single tokio tick — which is the normal case for short-lived
    /// requests like the readiness probe. Polling a map entry for
    /// `buf_alloc > 0` races the `Shutdown` handler which removes the
    /// entry before the poller wakes up.
    response_signal: Option<oneshot::Sender<()>>,
    /// Last-known credit snapshot from the guest. Drives host→guest
    /// flow control: the reader task blocks when `host_forwarded -
    /// guest_fwd_cnt >= guest_buf_alloc` and wakes on
    /// [`Self::credit_notify`] when the guest sends a `CreditUpdate`
    /// or `Rw` packet (which carries a refreshed `fwd_cnt`).
    guest_buf_alloc: u32,
    guest_fwd_cnt: u32,
    /// Number of payload bytes we've forwarded **to the guest** via
    /// `Rw` packets. Drives the `fwd_cnt` field on host→guest packets
    /// — cumulative byte count per vsock protocol §5.10.6.
    host_forwarded: u32,
    /// Number of payload bytes we've forwarded **from the guest to the
    /// host UDS**. Drives the `fwd_cnt` field we advertise back on
    /// `CreditUpdate` packets. Separate counter so guest→host and
    /// host→guest streams advance independently.
    guest_rx_forwarded: u32,
    /// Fired from `handle_tx` whenever the guest sends a packet that
    /// refreshes the credit window (`Response`, `Rw`, `CreditUpdate`),
    /// so the reader task waiting for credit can wake up immediately.
    credit_notify: Arc<Notify>,
    /// Marks the connection as closed; both halves stop pushing new
    /// packets once set.
    closed: bool,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("host_forwarded", &self.host_forwarded)
            .field("guest_rx_forwarded", &self.guest_rx_forwarded)
            .field("guest_buf_alloc", &self.guest_buf_alloc)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

/// State shared between the async I/O tasks and the virtio device
/// callbacks.
#[derive(Debug)]
struct SharedState {
    /// Guest CID configured on the virtio-vsock device — packets we
    /// produce carry this as `dst_cid`.
    guest_cid: u64,
    /// `src_port` counter for host-initiated connections. `1024..` per
    /// the vsock conventional split between well-known and ephemeral.
    next_host_port: std::sync::atomic::AtomicU32,
    /// Packets queued for delivery to the guest. Drained by
    /// [`UdsVsockMuxer::drain_rx`].
    rx_queue: Mutex<Vec<VsockPacket>>,
    /// Routing table — maps `(host_port, guest_port)` to the async
    /// writer channel for that connection.
    connections: Mutex<BTreeMap<ConnectionKey, Connection>>,
    /// Fires whenever a new RX packet lands in the queue, so the VMM
    /// event loop can call `process_queue(RX_QUEUE)` promptly.
    rx_notify: Notify,
}

impl SharedState {
    fn push_rx(&self, pkt: VsockPacket) {
        tracing::trace!(
            op = ?pkt.hdr.op,
            src_port = pkt.hdr.src_port,
            dst_port = pkt.hdr.dst_port,
            payload_len = pkt.payload.len(),
            "vsock muxer: push_rx (host->guest)"
        );
        self.rx_queue.lock().push(pkt);
        self.rx_notify.notify_one();
    }

    fn allocate_host_port(&self) -> u32 {
        // Firecracker's convention reserves 0..1024 for well-known
        // ports; ephemeral allocations live at 1024..
        self.next_host_port
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

/// Inputs for [`UdsVsockMuxer::spawn`].
#[derive(Debug, Clone)]
pub(crate) struct UdsVsockMuxerParams {
    /// Base UDS path configured via `PUT /vsock`. Per-port listeners
    /// bind at `{base}_{port}`.
    pub uds_base: PathBuf,
    /// Guest CID configured on the virtio-vsock device.
    pub guest_cid: u64,
    /// Ports the host wants to dial *into* the guest. Each gets its
    /// own UDS listener bound at `{base}_{port}`. For tok's cold-path
    /// that's `[5001, 5002, 5003, 5004]` (exec / obs / stage / health),
    /// matching the ports bound by `tok-initd` inside the guest.
    pub host_initiated_ports: Vec<u32>,
}

/// Production UDS-backed [`VsockMuxer`].
#[derive(Debug)]
pub(crate) struct UdsVsockMuxer {
    state: Arc<SharedState>,
}

impl UdsVsockMuxer {
    /// Spawn the muxer and its UDS listeners.
    ///
    /// # Errors
    /// Surfaces any `bind(2)` failure on the per-port listener paths.
    pub(crate) fn spawn(params: UdsVsockMuxerParams) -> Result<Arc<Self>> {
        let state = Arc::new(SharedState {
            guest_cid: params.guest_cid,
            // 1024 — conventional first ephemeral vsock port; anything
            // above the IANA-style `privileged` split works.
            next_host_port: std::sync::atomic::AtomicU32::new(1024),
            rx_queue: Mutex::new(Vec::new()),
            connections: Mutex::new(BTreeMap::new()),
            rx_notify: Notify::new(),
        });

        for port in &params.host_initiated_ports {
            let sock_path = derive_port_path(&params.uds_base, *port);
            // Remove stale file — bind(2) fails with EADDRINUSE
            // otherwise. The parent dir is expected to already exist
            // (per-VM work dir created by the executor).
            let _ = std::fs::remove_file(&sock_path);
            let listener = UnixListener::bind(&sock_path)
                .with_context(|| format!("binding vsock listener at {}", sock_path.display()))?;
            let state_for_task = Arc::clone(&state);
            let port = *port;
            let path_for_task = sock_path.clone();
            tokio::spawn(async move {
                host_listen_task(listener, state_for_task, port, path_for_task).await;
            });
            tracing::info!(
                port,
                path = %sock_path.display(),
                "vsock muxer: host listener bound"
            );
        }

        let _ = params.uds_base; // retained in SharedState via listener tasks
        Ok(Arc::new(Self { state }))
    }

    /// Direct handle on the inner wake signal. Waiters call
    /// `.notified().await` on this; the producer side calls
    /// `.notify_one()`.
    #[must_use]
    pub(crate) fn notify_handle(&self) -> NotifyHandle {
        NotifyHandle {
            state: Arc::clone(&self.state),
        }
    }
}

/// Small wrapper that exposes just the rx-ready wake signal.
#[derive(Debug, Clone)]
pub(crate) struct NotifyHandle {
    state: Arc<SharedState>,
}

impl NotifyHandle {
    /// Await the next RX-ready notification.
    pub(crate) async fn notified(&self) {
        self.state.rx_notify.notified().await;
    }
}

impl VsockMuxer for UdsVsockMuxer {
    fn handle_tx(&self, pkt: VsockPacket) -> Vec<VsockPacket> {
        // The virtio device hands us every guest-originated packet.
        // Our job: route it to the matching connection (or reject
        // guest-initiated connections with `Rst` for now).
        let guest_src_port = pkt.hdr.src_port;
        let guest_dst_port = pkt.hdr.dst_port;
        tracing::trace!(
            op = ?pkt.hdr.op,
            src_cid = pkt.hdr.src_cid,
            dst_cid = pkt.hdr.dst_cid,
            src_port = guest_src_port,
            dst_port = guest_dst_port,
            buf_alloc = pkt.hdr.buf_alloc,
            fwd_cnt = pkt.hdr.fwd_cnt,
            payload_len = pkt.payload.len(),
            "vsock muxer: guest TX"
        );
        let key = ConnectionKey {
            host_port: guest_dst_port,
            guest_port: guest_src_port,
        };

        match pkt.hdr.op {
            VsockOp::Request => {
                // Guest-initiated: the guest is trying to reach the
                // host at `dst_port`. Tok's cold path doesn't use
                // this direction; reply with `Rst` so the guest's
                // `connect()` fails fast rather than hanging.
                let rst = VsockPacket {
                    hdr: VsockHeader {
                        src_cid: VMADDR_CID_HOST,
                        dst_cid: self.state.guest_cid,
                        src_port: pkt.hdr.dst_port,
                        dst_port: pkt.hdr.src_port,
                        len: 0,
                        type_: TYPE_STREAM,
                        op: VsockOp::Rst,
                        flags: 0,
                        buf_alloc: 0,
                        fwd_cnt: 0,
                    },
                    payload: Vec::new(),
                };
                vec![rst]
            }
            VsockOp::Response => {
                // Guest accepted a host-initiated Request. Fire the
                // per-connection oneshot so the listener task stops
                // waiting for the ACK and writes `OK <host_port>\n`
                // back to the UDS client. Event-driven so we don't
                // race a short-lived guest that replies + shuts down
                // inside a single tokio tick. Also seeds the initial
                // credit window from the guest's advertised `buf_alloc`
                // so the reader task can start pushing payload bytes.
                let (signal, credit) = {
                    let mut conns = self.state.connections.lock();
                    if let Some(conn) = conns.get_mut(&key) {
                        conn.guest_buf_alloc = pkt.hdr.buf_alloc;
                        conn.guest_fwd_cnt = pkt.hdr.fwd_cnt;
                        (
                            conn.response_signal.take(),
                            Some(Arc::clone(&conn.credit_notify)),
                        )
                    } else {
                        (None, None)
                    }
                };
                if let Some(tx) = signal {
                    let _ = tx.send(());
                }
                if let Some(n) = credit {
                    n.notify_waiters();
                }
                Vec::new()
            }
            VsockOp::Rw => {
                // Guest → host payload. Forward bytes to the listener
                // task's writer channel and emit a CreditUpdate so the
                // guest's send buffer credit check doesn't stall.
                //
                // The vsock protocol requires `fwd_cnt` to be the
                // **running cumulative count** of payload bytes the
                // receiver has consumed on this stream — not this
                // packet's length. We keep that counter on the
                // per-connection record (`guest_rx_forwarded`) and
                // advertise its monotonic snapshot on every credit
                // packet. Every guest packet also refreshes our
                // snapshot of the guest's receive-side credit window
                // (`guest_buf_alloc`, `guest_fwd_cnt`) so the reader
                // task can make progress.
                let payload_len = u32::try_from(pkt.payload.len()).unwrap_or(u32::MAX);
                let mut conns = self.state.connections.lock();
                let (fwd_cnt_snapshot, credit_notify) = if let Some(conn) = conns.get_mut(&key) {
                    if conn.closed {
                        return Vec::new();
                    }
                    if conn.guest_to_host_tx.send(pkt.payload).is_err() {
                        conn.closed = true;
                    }
                    conn.guest_rx_forwarded = conn.guest_rx_forwarded.saturating_add(payload_len);
                    conn.guest_buf_alloc = pkt.hdr.buf_alloc;
                    conn.guest_fwd_cnt = pkt.hdr.fwd_cnt;
                    (
                        Some(conn.guest_rx_forwarded),
                        Some(Arc::clone(&conn.credit_notify)),
                    )
                } else {
                    (None, None)
                };
                drop(conns);
                if let Some(fwd_cnt) = fwd_cnt_snapshot {
                    let credit = VsockPacket {
                        hdr: VsockHeader {
                            src_cid: VMADDR_CID_HOST,
                            dst_cid: self.state.guest_cid,
                            src_port: guest_dst_port,
                            dst_port: guest_src_port,
                            len: 0,
                            type_: TYPE_STREAM,
                            op: VsockOp::CreditUpdate,
                            flags: 0,
                            buf_alloc: DEFAULT_BUF_ALLOC,
                            fwd_cnt,
                        },
                        payload: Vec::new(),
                    };
                    self.state.push_rx(credit);
                }
                if let Some(n) = credit_notify {
                    n.notify_waiters();
                }
                Vec::new()
            }
            VsockOp::Shutdown | VsockOp::Rst => {
                // Remove and drop the connection; dropping the
                // `guest_to_host_tx` inside `Connection` closes the
                // mpsc channel so the writer task sees EOF and
                // exits cleanly. If the oneshot hasn't fired yet,
                // dropping it closes the receive half too so the
                // listener's await falls into the `Err` arm and
                // responds with `RST`.
                let _ = self.state.connections.lock().remove(&key);
                Vec::new()
            }
            VsockOp::CreditUpdate | VsockOp::CreditRequest => {
                // Read-only credit updates from the guest: refresh the
                // per-connection credit snapshot and wake the reader
                // task so it can drain UDS bytes against the new
                // window. On a CreditRequest we reply with our current
                // `fwd_cnt` so the guest can recompute its send-side
                // credit.
                let (fwd_cnt_snapshot, credit_notify) = {
                    let mut conns = self.state.connections.lock();
                    if let Some(conn) = conns.get_mut(&key) {
                        conn.guest_buf_alloc = pkt.hdr.buf_alloc;
                        conn.guest_fwd_cnt = pkt.hdr.fwd_cnt;
                        (
                            Some(conn.guest_rx_forwarded),
                            Some(Arc::clone(&conn.credit_notify)),
                        )
                    } else {
                        (None, None)
                    }
                };
                if matches!(pkt.hdr.op, VsockOp::CreditRequest)
                    && let Some(fwd_cnt) = fwd_cnt_snapshot
                {
                    let credit = VsockPacket {
                        hdr: VsockHeader {
                            src_cid: VMADDR_CID_HOST,
                            dst_cid: self.state.guest_cid,
                            src_port: guest_dst_port,
                            dst_port: guest_src_port,
                            len: 0,
                            type_: TYPE_STREAM,
                            op: VsockOp::CreditUpdate,
                            flags: 0,
                            buf_alloc: DEFAULT_BUF_ALLOC,
                            fwd_cnt,
                        },
                        payload: Vec::new(),
                    };
                    self.state.push_rx(credit);
                }
                if let Some(n) = credit_notify {
                    n.notify_waiters();
                }
                Vec::new()
            }
            VsockOp::Invalid => Vec::new(),
        }
    }

    fn drain_rx(&self) -> Vec<VsockPacket> {
        let pkts = std::mem::take(&mut *self.state.rx_queue.lock());
        if !pkts.is_empty() {
            tracing::trace!(count = pkts.len(), "vsock muxer: drain_rx");
        }
        pkts
    }
}

/// Derive `{base}_{port}` honouring the 103-byte Darwin UDS cap.
fn derive_port_path(base: &Path, port: u32) -> PathBuf {
    let name = base
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("vsock.sock");
    let dir = base.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{name}_{port}"))
}

async fn host_listen_task(
    listener: UnixListener,
    state: Arc<SharedState>,
    guest_port: u32,
    path: PathBuf,
) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "vsock listener accept failed");
                return;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(err) = handle_host_connection(stream, Arc::clone(&state), guest_port).await {
                tracing::debug!(error = %err, guest_port, "vsock connection closed");
            }
        });
    }
}

async fn handle_host_connection(
    mut stream: UnixStream,
    state: Arc<SharedState>,
    guest_port: u32,
) -> Result<()> {
    // Strip the Firecracker `CONNECT <port>\n` preamble. We already
    // know the port from the listener — we still read the line for
    // protocol compatibility (clients always send it).
    let mut preamble = Vec::with_capacity(32);
    let mut buf = [0u8; 1];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("host closed during CONNECT preamble");
        }
        preamble.push(buf[0]);
        if buf[0] == b'\n' {
            break;
        }
        if preamble.len() > 64 {
            anyhow::bail!("CONNECT line too long");
        }
    }

    let host_port = state.allocate_host_port();
    let key = ConnectionKey {
        host_port,
        guest_port,
    };

    // Create the per-connection byte channel and the response
    // oneshot. The handle_tx side pushes guest-originated payloads
    // onto `guest_to_host_tx`; the writer half forwards them to the
    // UDS stream. `response_rx` fires when the guest accepts the
    // connection with a `Response` packet — event-driven so a short-
    // lived guest (Response + Rw + Shutdown in one tokio tick)
    // doesn't race the listener's poll.
    let (guest_to_host_tx, mut guest_to_host_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (response_tx, response_rx) = oneshot::channel::<()>();
    let credit_notify = Arc::new(Notify::new());
    state.connections.lock().insert(
        key,
        Connection {
            guest_to_host_tx,
            response_signal: Some(response_tx),
            guest_buf_alloc: 0,
            guest_fwd_cnt: 0,
            host_forwarded: 0,
            guest_rx_forwarded: 0,
            credit_notify: Arc::clone(&credit_notify),
            closed: false,
        },
    );

    // Send the virtio `Request` packet to the guest.
    let req = VsockPacket {
        hdr: VsockHeader {
            src_cid: VMADDR_CID_HOST,
            dst_cid: state.guest_cid,
            src_port: host_port,
            dst_port: guest_port,
            len: 0,
            type_: TYPE_STREAM,
            op: VsockOp::Request,
            flags: 0,
            buf_alloc: DEFAULT_BUF_ALLOC,
            fwd_cnt: 0,
        },
        payload: Vec::new(),
    };
    state.push_rx(req);

    // Await the guest's `Response` (via the oneshot fired from
    // `handle_tx`'s Response arm) with a 5-second ceiling. We can be
    // generous with the timeout because the happy path is sub-
    // millisecond on HVF and a missing response always means the
    // guest kernel either rejected (Rst) or the port has no listener.
    let ack_deadline = tokio::time::Duration::from_secs(5);
    // Either the timeout expired or the oneshot was dropped
    // (connection removed by a Shutdown/Rst handler before the guest
    // responded). Tell the client and clean up.
    if !matches!(
        tokio::time::timeout(ack_deadline, response_rx).await,
        Ok(Ok(()))
    ) {
        let _ = stream
            .write_all(format!("RST {guest_port}\n").as_bytes())
            .await;
        state.connections.lock().remove(&key);
        return Ok(());
    }

    // Ack the client.
    stream
        .write_all(format!("OK {host_port}\n").as_bytes())
        .await?;

    // Split the UDS for bidirectional byte streaming.
    let (mut read_half, mut write_half) = stream.into_split();

    // Reader: host UDS → guest (RW packets). Respects the guest's
    // advertised credit window (`buf_alloc - (host_forwarded -
    // guest_fwd_cnt)`) and waits on `credit_notify` whenever the
    // window closes. Without this throttle, Linux's vsock driver
    // discards packets silently once its socket `sk_rcvbuf` fills
    // up — observed as a stage-and-call that stalls at ~75 % of a
    // multi-MiB payload.
    let state_reader = Arc::clone(&state);
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; RW_CHUNK_BYTES];
        loop {
            // Wait for credit before reading from the UDS. Reading and
            // then blocking on credit would strand bytes in a local
            // Vec — doing it UDS-side preserves the kernel's natural
            // flow control to the host client. Register the notified
            // future *before* re-reading credit so we don't lose
            // wakeups that fire between the credit check and the
            // await (Notify::notify_waiters() is lost if no waiter is
            // registered at the time of the call).
            loop {
                let notify = {
                    let conns = state_reader.connections.lock();
                    match conns.get(&key) {
                        Some(conn) if !conn.closed => Arc::clone(&conn.credit_notify),
                        _ => return,
                    }
                };
                let notified = notify.notified();
                tokio::pin!(notified);
                // Second check after the registration so a wake posted
                // between the first read and `notified.enable()` is
                // observed immediately.
                let credit_bytes = {
                    let conns = state_reader.connections.lock();
                    match conns.get(&key) {
                        Some(conn) if !conn.closed => {
                            let outstanding = conn.host_forwarded.wrapping_sub(conn.guest_fwd_cnt);
                            conn.guest_buf_alloc.saturating_sub(outstanding)
                        }
                        _ => return,
                    }
                };
                if credit_bytes > 0 {
                    break;
                }
                notified.as_mut().await;
            }
            // Clamp the UDS read to at most the current credit window
            // AND the fixed chunk size so a single RW packet never
            // exceeds either. We read `buf[..chunk]` rather than the
            // full vec so credit bookkeeping stays accurate.
            let window = {
                let conns = state_reader.connections.lock();
                match conns.get(&key) {
                    Some(conn) if !conn.closed => {
                        let outstanding = conn.host_forwarded.wrapping_sub(conn.guest_fwd_cnt);
                        conn.guest_buf_alloc.saturating_sub(outstanding)
                    }
                    _ => return,
                }
            };
            let chunk = RW_CHUNK_BYTES.min(window as usize).max(1);
            let n = match read_half.read(&mut buf[..chunk]).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) => {
                    tracing::debug!(error = %err, "vsock uds read failed");
                    break;
                }
            };
            let payload = buf[..n].to_vec();
            // vsock protocol §5.10.6.3: `fwd_cnt` in an outgoing packet is
            // the **sender's** running count of bytes it has consumed
            // from its RX buffer — i.e., bytes the host has delivered to
            // the UDS client (guest → host direction). Advertising
            // `host_forwarded` (bytes host has pushed to guest) instead
            // causes Linux's vsock driver to compute a nonsensical
            // peer-credit snapshot and stall the guest's userspace
            // reader. Track the counter for observability but advertise
            // `guest_rx_forwarded` on the wire.
            let (buf_alloc, fwd_cnt) = {
                let mut conns = state_reader.connections.lock();
                match conns.get_mut(&key) {
                    Some(conn) if !conn.closed => {
                        conn.host_forwarded = conn.host_forwarded.saturating_add(n as u32);
                        (DEFAULT_BUF_ALLOC, conn.guest_rx_forwarded)
                    }
                    _ => return,
                }
            };
            let rw = VsockPacket {
                hdr: VsockHeader {
                    src_cid: VMADDR_CID_HOST,
                    dst_cid: state_reader.guest_cid,
                    src_port: host_port,
                    dst_port: guest_port,
                    len: n as u32,
                    type_: TYPE_STREAM,
                    op: VsockOp::Rw,
                    flags: 0,
                    buf_alloc,
                    fwd_cnt,
                },
                payload,
            };
            state_reader.push_rx(rw);
        }

        // Host closed the UDS: tell the guest we're done. Use the
        // connection's final guest→host byte counter on the shutdown
        // packet so the guest's credit state stays consistent.
        let shutdown_fwd_cnt = state_reader
            .connections
            .lock()
            .get(&key)
            .map_or(0, |c| c.guest_rx_forwarded);
        let shutdown = VsockPacket {
            hdr: VsockHeader {
                src_cid: VMADDR_CID_HOST,
                dst_cid: state_reader.guest_cid,
                src_port: host_port,
                dst_port: guest_port,
                len: 0,
                type_: TYPE_STREAM,
                op: VsockOp::Shutdown,
                flags: 0x3, // bit0=send, bit1=recv
                buf_alloc: DEFAULT_BUF_ALLOC,
                fwd_cnt: shutdown_fwd_cnt,
            },
            payload: Vec::new(),
        };
        state_reader.push_rx(shutdown);
        state_reader.connections.lock().remove(&key);
    });

    // Writer: guest → host UDS (drain mpsc of payloads).
    let writer = tokio::spawn(async move {
        while let Some(payload) = guest_to_host_rx.recv().await {
            if write_half.write_all(&payload).await.is_err() {
                break;
            }
        }
        let _ = write_half.shutdown().await;
    });

    // If either side exits we close the whole connection. Joining the
    // reader first propagates host EOF; the writer drops naturally
    // when its channel closes on the connection-removal path.
    let _ = reader.await;
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_should_bind_listeners_for_host_initiated_ports() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("vsock.sock");
        let muxer = UdsVsockMuxer::spawn(UdsVsockMuxerParams {
            uds_base: base.clone(),
            guest_cid: 3,
            host_initiated_ports: vec![5001, 5003],
        })
        .unwrap();
        // Listeners are spawned in the background — give tokio a
        // scheduler tick to call bind(2) before we probe.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(derive_port_path(&base, 5001).exists());
        assert!(derive_port_path(&base, 5003).exists());
        drop(muxer);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_should_reject_guest_initiated_connection_with_rst() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("vsock.sock");
        let muxer = UdsVsockMuxer::spawn(UdsVsockMuxerParams {
            uds_base: base,
            guest_cid: 3,
            host_initiated_ports: vec![],
        })
        .unwrap();
        // Fabricate a guest-initiated Request → we expect a Rst reply.
        let pkt = VsockPacket {
            hdr: VsockHeader {
                src_cid: 3,
                dst_cid: VMADDR_CID_HOST,
                src_port: 4000,
                dst_port: 99,
                len: 0,
                type_: TYPE_STREAM,
                op: VsockOp::Request,
                flags: 0,
                buf_alloc: 0,
                fwd_cnt: 0,
            },
            payload: Vec::new(),
        };
        let reply = muxer.handle_tx(pkt);
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0].hdr.op, VsockOp::Rst);
        assert_eq!(reply[0].hdr.src_port, 99);
        assert_eq!(reply[0].hdr.dst_port, 4000);
    }
}
