//! gvproxy bundled-binary userspace mode.
//!
//! Per [30-networking.md § 4](../../../specs/30-networking.md#4-userspace-mode-gvproxy)
//! the operator picks `--network=userspace` when they don't have or want
//! `vmnet.framework` access. squib spawns a child process — the bundled
//! `gvproxy` binary — and exchanges L2 frames with it over a `socketpair(2)`
//! shared at fd 3 in the child.
//!
//! ## Framing
//!
//! qemu's classic length-prefixed framing (4-byte big-endian frame length,
//! followed by `length` bytes of Ethernet payload). gvproxy speaks this
//! shape natively (`stream` mode); vfkit, podman-machine, and lima all
//! exchange frames with gvproxy this way.
//!
//! ## Lifetime
//!
//! - The child process is launched with `tokio::process::Command::spawn` and `kill_on_drop(true)`
//!   so tokio sends SIGKILL when the [`Child`] drops.
//! - On `Drop` we additionally call `start_kill()` (non-blocking) so a panic during teardown still
//!   signals the child synchronously. We deliberately do **not** poll `try_wait` from `Drop` —
//!   `Drop` may run on a tokio worker thread and a synchronous spin would deadlock the executor.
//! - Per I-NET-5: "gvproxy child is reaped on every shutdown path (Drop, signal, panic)".
//!   `kill_on_drop` covers Drop and signal; the explicit `start_kill` covers panic.

#![forbid(unsafe_code)]
// `validate_binary_path` runs once at VM start to stat the bundled `gvproxy`
// binary; sub-millisecond on a local filesystem. The clippy-disallowed
// `std::fs::metadata` rule targets hot async paths, not one-shot
// config-time reads — same precedent as `crates/vmm/src/lib.rs`.
#![allow(clippy::disallowed_methods)]

use std::{
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::Mutex;
use squib_virtio::devices::net::{Frame, NetBackend};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::{Child, Command},
    sync::mpsc,
};
use tracing::{debug, info, warn};

/// Errors produced by the gvproxy backend.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GvproxyError {
    /// `socketpair(2)` failed.
    #[error("socketpair failed: {0}")]
    SocketPair(#[source] std::io::Error),

    /// Could not spawn the gvproxy binary.
    #[error("spawn gvproxy {path}: {source}")]
    Spawn {
        /// Path that was attempted.
        path: PathBuf,
        /// Underlying io error.
        #[source]
        source: std::io::Error,
    },

    /// `binary_path` failed validation (length / NUL byte / not a regular
    /// file). Reject at the boundary per [70-security.md § 4-5].
    #[error("invalid gvproxy binary_path: {reason}")]
    InvalidBinaryPath {
        /// Why the path was rejected.
        reason: &'static str,
    },

    /// One of the `extra_args` failed validation (length / charset).
    #[error("invalid gvproxy extra arg #{index}: {reason}")]
    InvalidExtraArg {
        /// Index in the supplied `extra_args` vector.
        index: usize,
        /// Why the arg was rejected.
        reason: &'static str,
    },

    /// I/O error reading or writing the L2 frame stream.
    #[error("gvproxy io error: {0}")]
    Io(#[from] std::io::Error),

    /// Frame body exceeded the configured cap.
    #[error("gvproxy frame too large: {got} > {cap}")]
    FrameTooLarge {
        /// Length declared by the framing prefix.
        got: usize,
        /// Configured cap.
        cap: usize,
    },
}

/// Inputs for [`GvproxyBackend::start`].
#[derive(Debug, Clone)]
pub struct GvproxyParams {
    /// Path to the bundled `gvproxy` binary.
    pub binary_path: PathBuf,
    /// Extra arguments after the squib-supplied defaults. Empty for the
    /// inner-dev-loop case.
    pub extra_args: Vec<String>,
    /// Maximum accepted frame body, in bytes. Defends against a malformed
    /// length prefix asking the reader to allocate gigabytes. Default 65536
    /// (well above standard Ethernet jumbo).
    pub max_frame_bytes: usize,
}

impl GvproxyParams {
    /// Construct with sensible inner-dev-loop defaults.
    #[must_use]
    pub fn new(binary_path: impl Into<PathBuf>) -> Self {
        Self {
            binary_path: binary_path.into(),
            extra_args: Vec::new(),
            max_frame_bytes: 65_536,
        }
    }
}

/// Active gvproxy backend.
#[derive(Debug)]
pub struct GvproxyBackend {
    /// Frames received from gvproxy and not yet drained by the virtio-net
    /// frontend. Bounded so a slow guest doesn't grow the queue without
    /// bound — under back-pressure we drop the oldest, matching the
    /// physical-network behaviour the guest already tolerates.
    rx_buffer: Arc<Mutex<Vec<Frame>>>,
    /// Outbound frame channel — the writer task reads this and writes
    /// length-prefixed frames into the UDS half we kept.
    tx_tx: mpsc::Sender<Frame>,
    /// Child process handle. `Drop` reaps it.
    child: Arc<Mutex<Option<Child>>>,
}

impl GvproxyBackend {
    /// Spawn gvproxy and start the I/O tasks. Must be called from within a
    /// tokio runtime context — [`tokio::spawn`] is invoked for the reader /
    /// writer tasks.
    ///
    /// # Errors
    /// Returns [`GvproxyError`] for validation failure on the binary path or
    /// extra-args, or for `socketpair(2)` / `Command::spawn` failure.
    pub fn start(params: &GvproxyParams) -> Result<Self, GvproxyError> {
        validate_binary_path(&params.binary_path)?;
        validate_extra_args(&params.extra_args)?;

        let (host_uds, child_uds) = make_socketpair()?;

        let mut cmd = Command::new(&params.binary_path);
        cmd.kill_on_drop(true);
        cmd.args(&params.extra_args);

        // Wire the child's stdin/stdout to the same `socketpair(SOCK_STREAM)`
        // endpoint. `socketpair(2)` returns a pair of bidirectional fds —
        // the child reads from fd 0 and writes to fd 1, both backed by the
        // single `child_uds` socket. We `try_clone()` so the parent's fd
        // table can hand both `Stdio` slots an owned dup; tokio closes the
        // duplicates after `spawn()` returns.
        let child_uds_clone = child_uds.try_clone()?;
        let stdin_fd: OwnedFd = child_uds_clone.into();
        let stdout_fd: OwnedFd = child_uds.into();
        cmd.stdin(std::process::Stdio::from(stdin_fd));
        cmd.stdout(std::process::Stdio::from(stdout_fd));
        cmd.stderr(std::process::Stdio::null());

        let child = cmd.spawn().map_err(|e| GvproxyError::Spawn {
            path: params.binary_path.clone(),
            source: e,
        })?;
        info!(path = %params.binary_path.display(), pid = ?child.id(), "gvproxy spawned");

        host_uds.set_nonblocking(true).map_err(GvproxyError::Io)?;
        let host_uds_async = UnixStream::from_std(host_uds).map_err(GvproxyError::Io)?;
        let (read_half, write_half) = host_uds_async.into_split();

        let rx_buffer: Arc<Mutex<Vec<Frame>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx_tx, tx_rx) = mpsc::channel::<Frame>(256);

        // Reader task: pulls length-prefixed frames out of gvproxy and
        // appends them to the rx buffer.
        let rx_buffer_for_task = Arc::clone(&rx_buffer);
        let max_bytes = params.max_frame_bytes;
        tokio::spawn(async move {
            run_reader(read_half, rx_buffer_for_task, max_bytes).await;
        });

        // Writer task: drains the outbound channel, writing length-prefixed
        // frames into gvproxy.
        tokio::spawn(async move {
            run_writer(write_half, tx_rx).await;
        });

        Ok(Self {
            rx_buffer,
            tx_tx,
            child: Arc::new(Mutex::new(Some(child))),
        })
    }
}

impl NetBackend for GvproxyBackend {
    fn send(&self, frame: &Frame) {
        // Non-blocking: if the channel is full, drop the frame. Lossy
        // semantics match physical Ethernet under congestion.
        if let Err(err) = self.tx_tx.try_send(frame.clone()) {
            tracing::trace!(error = %err, "gvproxy tx queue full; dropping frame");
        }
    }
    fn recv(&self) -> Vec<Frame> {
        std::mem::take(&mut *self.rx_buffer.lock())
    }
}

impl Drop for GvproxyBackend {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().take() {
            // I-NET-5: gvproxy child is reaped on every shutdown path. The
            // tokio `Child` was constructed with `kill_on_drop(true)`, which
            // queues a SIGKILL through tokio's reaping machinery on the
            // `Child::Drop` that fires below. On its own that's *not*
            // enough when the tokio runtime is being torn down in parallel
            // — the runtime's signal-delivery task can die before the
            // reap call runs, leaving gvproxy orphaned. Send an explicit
            // SIGKILL via `kill(2)` first so the signal lands on the
            // kernel side immediately, independent of tokio state. Both
            // calls are non-blocking; we deliberately do not spin on
            // `try_wait` here because `Drop` may execute on a tokio
            // worker thread and a blocking wait risks deadlock.
            if let Some(pid) = child.id() {
                crate::sys::kill_pid(pid, libc::SIGKILL);
            }
            // Belt and braces: also arm tokio's reaping path so the
            // zombie gets collected even if the runtime is still alive.
            let _ = child.start_kill();
            drop(child);
        }
    }
}

/// Cap the rx-buffer at this many frames to bound memory under guest stalls.
const RX_QUEUE_CAP: usize = 256;

async fn run_reader(
    mut read_half: tokio::net::unix::OwnedReadHalf,
    rx_buffer: Arc<Mutex<Vec<Frame>>>,
    max_frame_bytes: usize,
) {
    let mut hdr = [0u8; 4];
    loop {
        if let Err(err) = read_half.read_exact(&mut hdr).await {
            if err.kind() != std::io::ErrorKind::UnexpectedEof {
                debug!(error = %err, "gvproxy reader: header read failed");
            }
            return;
        }
        let len = u32::from_be_bytes(hdr) as usize;
        if len > max_frame_bytes {
            warn!(
                len,
                max_frame_bytes, "gvproxy reader: frame too large; closing"
            );
            return;
        }
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u8; len];
        if let Err(err) = read_half.read_exact(&mut buf).await {
            debug!(error = %err, "gvproxy reader: body read failed");
            return;
        }
        let frame = Frame::from_bytes(bytes::Bytes::from(buf));
        let mut guard = rx_buffer.lock();
        if guard.len() >= RX_QUEUE_CAP {
            guard.remove(0);
        }
        guard.push(frame);
    }
}

async fn run_writer(
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    mut tx_rx: mpsc::Receiver<Frame>,
) {
    while let Some(frame) = tx_rx.recv().await {
        let len = u32::try_from(frame.bytes.len()).unwrap_or(u32::MAX);
        let hdr = len.to_be_bytes();
        if let Err(err) = write_half.write_all(&hdr).await {
            debug!(error = %err, "gvproxy writer: header write failed");
            return;
        }
        if let Err(err) = write_half.write_all(&frame.bytes).await {
            debug!(error = %err, "gvproxy writer: body write failed");
            return;
        }
    }
}

/// Build a `SOCK_STREAM` socketpair. The socketpair-syscall lives behind
/// `unsafe` even in the `libc` crate; we go through `std::os::unix::net::UnixStream::pair`
/// which is safe (it does the unsafe internally and validates the
/// fds before handing them back).
fn make_socketpair() -> Result<
    (
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ),
    GvproxyError,
> {
    std::os::unix::net::UnixStream::pair().map_err(GvproxyError::SocketPair)
}

/// Reject `binary_path`s that look hostile — empty, NUL byte, longer than
/// Darwin's `PATH_MAX` of 1024, or pointing at a non-regular file. Per
/// [70-security.md § 4-5]: validate at the boundary, never sanitize. The
/// path comes from `--gvproxy-path` / `SQUIB_GVPROXY_PATH`, both env-shaped.
fn validate_binary_path(path: &Path) -> Result<(), GvproxyError> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() {
        return Err(GvproxyError::InvalidBinaryPath {
            reason: "must not be empty",
        });
    }
    if bytes.contains(&0) {
        return Err(GvproxyError::InvalidBinaryPath {
            reason: "must not contain NUL bytes",
        });
    }
    if bytes.len() > 1024 {
        return Err(GvproxyError::InvalidBinaryPath {
            reason: "exceeds PATH_MAX (1024 bytes)",
        });
    }
    let metadata = std::fs::metadata(path).map_err(|_| GvproxyError::InvalidBinaryPath {
        reason: "could not stat binary path",
    })?;
    if !metadata.is_file() {
        return Err(GvproxyError::InvalidBinaryPath {
            reason: "not a regular file",
        });
    }
    Ok(())
}

/// Cap `extra_args` at 32 elements; cap each element at 512 bytes; reject
/// NUL bytes (the shell doesn't tolerate them either, but `Command::args`
/// would silently embed). The byte-cap and NUL-rejection is load-bearing;
/// gvproxy itself validates flag *content*.
fn validate_extra_args(args: &[String]) -> Result<(), GvproxyError> {
    if args.len() > 32 {
        return Err(GvproxyError::InvalidExtraArg {
            index: 32,
            reason: "more than 32 extra args",
        });
    }
    for (i, a) in args.iter().enumerate() {
        if a.len() > 512 {
            return Err(GvproxyError::InvalidExtraArg {
                index: i,
                reason: "exceeds 512 bytes",
            });
        }
        if a.contains('\0') {
            return Err(GvproxyError::InvalidExtraArg {
                index: i,
                reason: "contains NUL byte",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_construct_default_params() {
        let p = GvproxyParams::new("/usr/local/libexec/squib/gvproxy");
        assert_eq!(p.max_frame_bytes, 65_536);
        assert!(p.extra_args.is_empty());
    }

    #[test]
    fn test_should_reject_empty_binary_path() {
        let r = validate_binary_path(Path::new(""));
        assert!(matches!(r, Err(GvproxyError::InvalidBinaryPath { .. })));
    }

    #[test]
    fn test_should_reject_binary_path_with_nul_byte() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        let p = Path::new(OsStr::from_bytes(b"/etc/p\0asswd"));
        let r = validate_binary_path(p);
        assert!(matches!(r, Err(GvproxyError::InvalidBinaryPath { .. })));
    }

    #[test]
    fn test_should_reject_extra_args_above_cap() {
        let args: Vec<String> = (0..40).map(|i| format!("--flag-{i}")).collect();
        let r = validate_extra_args(&args);
        assert!(matches!(r, Err(GvproxyError::InvalidExtraArg { .. })));
    }

    #[test]
    fn test_should_reject_extra_arg_with_nul_byte() {
        let args = vec!["--ok".into(), "with\0nul".into()];
        let r = validate_extra_args(&args);
        assert!(matches!(
            r,
            Err(GvproxyError::InvalidExtraArg { index: 1, .. })
        ));
    }

    #[tokio::test]
    async fn test_should_round_trip_a_frame_through_simulated_gvproxy_endpoint() {
        // Stand up a UnixStream pair, attach the GvproxyBackend's reader/writer
        // tasks to one half manually (without spawning a real gvproxy). The other
        // half becomes our "fake gvproxy".
        let (host_std, child_std) = std::os::unix::net::UnixStream::pair().unwrap();
        host_std.set_nonblocking(true).unwrap();
        child_std.set_nonblocking(true).unwrap();
        let host = UnixStream::from_std(host_std).unwrap();
        let mut child = UnixStream::from_std(child_std).unwrap();
        let (read_half, write_half) = host.into_split();
        let rx_buffer: Arc<Mutex<Vec<Frame>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx_tx, tx_rx) = mpsc::channel::<Frame>(8);

        let rx_for_task = Arc::clone(&rx_buffer);
        tokio::spawn(async move {
            run_reader(read_half, rx_for_task, 65_536).await;
        });
        tokio::spawn(async move {
            run_writer(write_half, tx_rx).await;
        });

        // Host → "gvproxy": send a frame and observe it on the child side.
        tx_tx.send(Frame::from_slice(b"helloeth0")).await.unwrap();
        let mut hdr = [0u8; 4];
        child.read_exact(&mut hdr).await.unwrap();
        assert_eq!(u32::from_be_bytes(hdr), b"helloeth0".len() as u32);
        let mut buf = vec![0u8; b"helloeth0".len()];
        child.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"helloeth0");

        // "gvproxy" → host: send a frame and observe it on the rx_buffer.
        let body = b"replyabc";
        let mut hdr = (body.len() as u32).to_be_bytes().to_vec();
        hdr.extend_from_slice(body);
        child.write_all(&hdr).await.unwrap();
        // Spin briefly for the reader task to ingest.
        for _ in 0..50 {
            if !rx_buffer.lock().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let frames = std::mem::take(&mut *rx_buffer.lock());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].bytes.as_ref(), body);
    }
}
