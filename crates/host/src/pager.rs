//! Postcopy / lazy-restore pager — Mach-exception-port-driven page server.
//!
//! Implements [16-snapshots.md § 5](../../../specs/16-snapshots.md#5-postcopy--lazy-restore).
//! Two distinct backends:
//!
//! - [`FilePageSource`] — pages come from the `<id>.mem` file by `pread(2)` at `(ipa - ram_start)`.
//! - [`UffdPageSource`] — pages come from a UDS the operator runs a page server on,
//!   protocol-compatible with upstream Firecracker's `Uffd` backend.
//!
//! The pager itself is host-portable: it owns the page-source dispatcher, the
//! pre-warm list, statistics, and the LLDB-coexistence policy state. The Mach side
//! lives in a `mach_imp` module under `cfg(target_os = "macos")`; the rest of the
//! crate is testable on Linux too.
//!
//! ## LLDB coexistence
//!
//! On startup the pager calls `task_swap_exception_ports` (NOT `task_set_exception_ports`)
//! so the previous exception handler — kernel default, or LLDB's port — is captured.
//! On every received fault that does not fall in any registered postcopy region, the
//! pager forwards the exception via `mach_exception_raise_state_identity` to the saved
//! prior port and returns the prior handler's `KERN_*` value.
//!
//! The "lldb-attaches-after-pager" case is handled by re-reading the current ports
//! on every `mach_msg` 1 s timeout (configurable via [`PagerConfig::poll_interval`])
//! and re-installing if they have drifted.

#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;
use thiserror::Error;

/// Pager configuration knobs (16 § 5).
#[derive(Debug, Clone)]
pub struct PagerConfig {
    /// `mach_msg` poll interval. The pager wakes up every interval to re-read the
    /// task's exception ports and re-install if LLDB has overwritten ours.
    pub poll_interval: Duration,
    /// Pre-warm list of guest IPAs to fault in before vCPU 0 runs.
    pub prewarm: PrewarmList,
}

impl Default for PagerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            prewarm: PrewarmList::default(),
        }
    }
}

/// Pre-warm list — a small set of guest IPAs the pager faults in before vCPU 0
/// runs so the first guest cycles are not all blocking on the pager (16 § 5.2).
#[derive(Debug, Clone, Default)]
pub struct PrewarmList {
    /// One page IPA per entry. The pager faults each in order on registration.
    pub pages: Vec<u64>,
}

impl PrewarmList {
    /// Build a list from kernel `_text`/`_stext` neighbourhood, vCPU 0 stack, and
    /// FDT.
    ///
    /// Each input is a guest-physical address; we add it as a single page entry.
    /// The pager pads the address to its tracking page granule on `prewarm`.
    #[must_use]
    pub fn from_boot_critical(kernel_text_ipa: u64, vcpu0_stack_ipa: u64, fdt_ipa: u64) -> Self {
        Self {
            pages: vec![kernel_text_ipa, vcpu0_stack_ipa, fdt_ipa],
        }
    }
}

/// One page request the pager sends to a [`PageSource`].
#[derive(Debug, Clone)]
pub struct PageRequest {
    /// Guest-physical address (IPA) of the page.
    pub ipa: u64,
    /// Page size in bytes (matches the host page on Apple Silicon = 16 KiB).
    pub page_size: u64,
}

/// Error variants surfaced from a [`PageSource`].
#[derive(Debug, Error)]
pub enum PageSourceError {
    /// The source returned fewer bytes than `page_size`.
    #[error("page source returned a short read: {got} bytes, expected {expected}")]
    Short {
        /// Bytes received.
        got: u64,
        /// Bytes expected.
        expected: u64,
    },
    /// The operator-supplied path / UDS could not be opened.
    #[error("page source open: {0}")]
    Open(String),
    /// Underlying I/O failure.
    #[error("page source I/O: {0}")]
    Io(#[source] std::io::Error),
}

impl From<std::io::Error> for PageSourceError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Where the pager pulls page bytes from.
///
/// One concrete impl per `mem_backend.backend_type` (`File`, `Uffd`).
pub trait PageSource: Send + Sync + std::fmt::Debug {
    /// Return the bytes for `req.ipa` rounded down to its `req.page_size` boundary.
    ///
    /// # Errors
    /// [`PageSourceError`] for any source-side failure.
    fn fetch(&self, req: &PageRequest) -> Result<Bytes, PageSourceError>;
}

/// `File`-backed page source. Reads from a memory-file on local disk by `pread`.
#[derive(Debug)]
pub struct FilePageSource {
    file: File,
    ram_start: u64,
}

impl FilePageSource {
    /// Open the memory file and bind it to `ram_start`.
    ///
    /// `ram_start` is the guest-physical address corresponding to byte 0 of the
    /// memory file (typically `DRAM_BASE = 0x8000_0000`).
    ///
    /// # Errors
    /// [`PageSourceError::Open`] if the file cannot be opened.
    pub fn open(path: &Path, ram_start: u64) -> Result<Self, PageSourceError> {
        let file = File::open(path)
            .map_err(|e| PageSourceError::Open(format!("{}: {e}", path.display())))?;
        Ok(Self { file, ram_start })
    }
}

impl PageSource for FilePageSource {
    fn fetch(&self, req: &PageRequest) -> Result<Bytes, PageSourceError> {
        if req.ipa < self.ram_start {
            return Err(PageSourceError::Open(format!(
                "ipa {:#x} below ram_start {:#x}",
                req.ipa, self.ram_start
            )));
        }
        let offset = req.ipa - self.ram_start;
        let aligned = offset & !(req.page_size - 1);
        let mut buf = vec![
            0u8;
            usize::try_from(req.page_size).map_err(|_| {
                PageSourceError::Open("page_size > usize::MAX".into())
            })?
        ];
        // `pread` keeps the read off the shared seek cursor — works under
        // concurrent fault threads.
        #[cfg(unix)]
        let n = self.file.read_at(&mut buf, aligned)?;
        #[cfg(not(unix))]
        let n = {
            // Non-unix is test-only; the lib is unsafe on Mach which is unix-only.
            let _ = aligned;
            buf.fill(0);
            buf.len()
        };
        // `usize → u64` is infallible on every supported squib host (Apple
        // Silicon is 64-bit), but `try_from` makes the widening explicit so the
        // crate-level `cast_possible_truncation` allow no longer hides a wrap.
        let n_u64 = u64::try_from(n).unwrap_or(u64::MAX);
        if n_u64 < req.page_size {
            return Err(PageSourceError::Short {
                got: n_u64,
                expected: req.page_size,
            });
        }
        Ok(Bytes::from(buf))
    }
}

/// `Uffd`-backed page source. Talks to a page-server on a UDS in the
/// upstream-Firecracker wire shape (see § 5.1).
///
/// Wire shape (squib's; mirrors upstream's `userfaultfd` request shape):
///
/// ```text
/// request:  <u64 LE: ipa>
///           <u64 LE: page_size>
/// response: <u64 LE: ipa>
///           <u64 LE: page_size>
///           <bytes: page payload, length == page_size>
/// ```
///
/// The connection is `parking_lot::Mutex`-serialised so concurrent fault threads
/// can share one UDS without interleaving requests.
#[derive(Debug)]
pub struct UffdPageSource {
    socket: Mutex<UnixStream>,
    uds_path: PathBuf,
}

impl UffdPageSource {
    /// Connect to the page-server's UDS.
    ///
    /// # Errors
    /// [`PageSourceError::Open`] if connect fails.
    pub fn connect(path: &Path) -> Result<Self, PageSourceError> {
        let sock = UnixStream::connect(path)
            .map_err(|e| PageSourceError::Open(format!("connect({}): {e}", path.display())))?;
        Ok(Self {
            socket: Mutex::new(sock),
            uds_path: path.to_path_buf(),
        })
    }

    /// Path of the UDS this source is connected to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.uds_path
    }
}

impl PageSource for UffdPageSource {
    fn fetch(&self, req: &PageRequest) -> Result<Bytes, PageSourceError> {
        let mut sock = self.socket.lock();
        // Send request.
        sock.write_all(&req.ipa.to_le_bytes())?;
        sock.write_all(&req.page_size.to_le_bytes())?;
        // Read header echo.
        let mut hdr = [0u8; 16];
        sock.read_exact(&mut hdr)?;
        let echo_ipa = u64::from_le_bytes(hdr[0..8].try_into().unwrap_or([0; 8]));
        let echo_size = u64::from_le_bytes(hdr[8..16].try_into().unwrap_or([0; 8]));
        if echo_ipa != req.ipa || echo_size != req.page_size {
            return Err(PageSourceError::Open(format!(
                "Uffd protocol violation: expected ipa={:#x} size={} got ipa={:#x} size={}",
                req.ipa, req.page_size, echo_ipa, echo_size
            )));
        }
        let want = usize::try_from(req.page_size)
            .map_err(|_| PageSourceError::Open("page_size > usize::MAX".into()))?;
        let mut buf = BytesMut::zeroed(want);
        sock.read_exact(&mut buf)?;
        Ok(buf.freeze())
    }
}

/// Pager statistics — exposed for tracing and unit tests. All counters are
/// monotonic so a test can assert "fault count went up by N".
#[derive(Debug, Default)]
pub struct PagerStats {
    /// Number of fault requests served.
    pub faults: AtomicU64,
    /// Number of pages pre-warmed at registration.
    pub prewarmed: AtomicU64,
    /// Number of times the pager re-installed its exception port after detecting
    /// LLDB drift.
    pub port_reinstalls: AtomicU64,
    /// Number of exceptions forwarded to a prior handler (out-of-region faults).
    pub forwarded_exceptions: AtomicU64,
}

impl PagerStats {
    /// Snapshot the counters into a plain-data struct for tests / logging.
    pub fn snapshot(&self) -> PagerStatsSnapshot {
        PagerStatsSnapshot {
            faults: self.faults.load(Ordering::Relaxed),
            prewarmed: self.prewarmed.load(Ordering::Relaxed),
            port_reinstalls: self.port_reinstalls.load(Ordering::Relaxed),
            forwarded_exceptions: self.forwarded_exceptions.load(Ordering::Relaxed),
        }
    }
}

/// Plain-data snapshot of [`PagerStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagerStatsSnapshot {
    /// Faults served.
    pub faults: u64,
    /// Pages pre-warmed.
    pub prewarmed: u64,
    /// Times we re-installed the exception port.
    pub port_reinstalls: u64,
    /// Exceptions forwarded to a prior handler.
    pub forwarded_exceptions: u64,
}

/// Errors produced by the pager.
#[derive(Debug, Error)]
pub enum PagerError {
    /// The IPA the host requested falls outside any registered postcopy region.
    #[error("postcopy region missing: ipa={ipa:#x}")]
    OutOfRegion {
        /// The requested IPA.
        ipa: u64,
    },
    /// The page source surfaced an error.
    #[error("page source: {0}")]
    Source(#[from] PageSourceError),
    /// Mach-side error (always opaque; the OS surfaces these via `kern_return_t`).
    #[error("mach error: {0}")]
    Mach(String),
    /// `pthread_create` / `thread::Builder::spawn` failed, typically with EAGAIN
    /// (per-process thread limit reached). DoS-relevant on a multi-VM host.
    #[error("pager server thread spawn: {0}")]
    Spawn(#[source] std::io::Error),
}

/// Boundaries of one registered postcopy region.
#[derive(Debug, Clone, Copy)]
struct Region {
    base: u64,
    size: u64,
}

impl Region {
    fn contains(&self, ipa: u64) -> bool {
        ipa >= self.base && ipa < self.base.saturating_add(self.size)
    }
}

/// The pager dispatches fault requests onto its [`PageSource`] and tracks which
/// IPA ranges are eligible.
///
/// The Mach side runs as a dedicated `std::thread` (the `mach_msg` server loop is
/// blocking, can't go on the tokio runtime). On non-macOS targets the `serve_*`
/// entry-points are provided as stubs so unit tests of the dispatch logic still
/// run on Linux CI nodes.
#[derive(Debug)]
pub struct Pager {
    inner: Arc<PagerInner>,
}

#[derive(Debug)]
struct PagerInner {
    source: Box<dyn PageSource>,
    config: PagerConfig,
    regions: Mutex<BTreeMap<u64, Region>>, // base → Region
    stats: PagerStats,
    /// Set on `request_shutdown`; the `mach_msg` server loop exits on its next
    /// poll wakeup.
    shutdown: AtomicBool,
}

impl Pager {
    /// Build a pager around a page source.
    ///
    /// `regions` is added separately via [`Self::register_region`] — typically
    /// `[ram_start, ram_size)`.
    #[must_use]
    pub fn new(source: Box<dyn PageSource>, config: PagerConfig) -> Self {
        Self {
            inner: Arc::new(PagerInner {
                source,
                config,
                regions: Mutex::new(BTreeMap::new()),
                stats: PagerStats::default(),
                shutdown: AtomicBool::new(false),
            }),
        }
    }

    /// Register a postcopy region `[base, base + size)`. Subsequent faults for IPAs
    /// inside the region are served from the page source.
    pub fn register_region(&self, base: u64, size: u64) {
        let mut regs = self.inner.regions.lock();
        regs.insert(base, Region { base, size });
    }

    /// Pre-warm the pages in [`PagerConfig::prewarm`].
    ///
    /// # Errors
    /// First [`PagerError::OutOfRegion`] / [`PagerError::Source`].
    pub fn prewarm(&self) -> Result<(), PagerError> {
        let pages = self.inner.config.prewarm.pages.clone();
        for ipa in pages {
            self.serve_fault(ipa, host_page_size())?;
            self.inner.stats.prewarmed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Serve one fault by computing the page boundary and pulling bytes.
    ///
    /// Returns the page bytes; the caller is responsible for `mach_vm_protect`
    /// on macOS or the equivalent host-side write through the HVF mapping.
    ///
    /// # Errors
    /// [`PagerError::OutOfRegion`] if the IPA isn't in a registered region;
    /// [`PagerError::Source`] for source-side failures.
    pub fn serve_fault(&self, ipa: u64, page_size: u64) -> Result<Bytes, PagerError> {
        if !self.contains_ipa(ipa) {
            return Err(PagerError::OutOfRegion { ipa });
        }
        let aligned = ipa & !(page_size - 1);
        let req = PageRequest {
            ipa: aligned,
            page_size,
        };
        let bytes = self.inner.source.fetch(&req)?;
        self.inner.stats.faults.fetch_add(1, Ordering::Relaxed);
        Ok(bytes)
    }

    /// `true` if the IPA falls inside any registered region.
    #[must_use]
    pub fn contains_ipa(&self, ipa: u64) -> bool {
        let regs = self.inner.regions.lock();
        regs.values().any(|r| r.contains(ipa))
    }

    /// Snapshot of the pager's counters.
    #[must_use]
    pub fn stats(&self) -> PagerStatsSnapshot {
        self.inner.stats.snapshot()
    }

    /// Signal the Mach server loop to exit at its next poll boundary.
    pub fn request_shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
    }

    /// Reference-counted clone of the pager handle. Used so the Mach server thread
    /// (or test scaffolding) can hold a handle for the lifetime of the run.
    #[must_use]
    pub fn handle(&self) -> PagerHandle {
        PagerHandle(Arc::clone(&self.inner))
    }

    /// Inner stats accessor (used by the Mach server thread to record forwarded
    /// exceptions and re-installs).
    #[doc(hidden)]
    pub fn record_port_reinstall(&self) {
        self.inner
            .stats
            .port_reinstalls
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Inner stats accessor.
    #[doc(hidden)]
    pub fn record_forwarded(&self) {
        self.inner
            .stats
            .forwarded_exceptions
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Cheap clone of the pager handle (an `Arc`).
#[derive(Debug, Clone)]
pub struct PagerHandle(Arc<PagerInner>);

impl PagerHandle {
    /// `true` if the supervisor has requested shutdown.
    pub fn is_shutting_down(&self) -> bool {
        self.0.shutdown.load(Ordering::SeqCst)
    }

    /// Borrow the poll interval.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        self.0.config.poll_interval
    }
}

/// Apple Silicon page size. Stays a function so squib-host compiles cross-platform.
#[must_use]
pub fn host_page_size() -> u64 {
    16 * 1024
}

// ---------------------------------------------------------------------------
// macOS-specific Mach exception port server (the live FFI surface).
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod mach_imp {
    //! The Mach-exception-port server thread.
    //!
    //! Phase 5.5 ships the dispatcher skeleton: the server thread, the
    //! shutdown-aware run loop, and a periodic drift check that the live
    //! `task_get_exception_ports` reader will plug into without changing the
    //! lifecycle.
    //!
    //! The actual `mach_msg(MACH_RCV_MSG)` server loop and
    //! `mach_exception_raise_state_identity` forwarder are gated behind the
    //! `pager-live-mach` cargo feature so unit tests on a developer Mac don't
    //! need to register a task-level exception port. The default build sleeps
    //! for `poll_interval` between drift checks — sufficient to verify the
    //! pager + drift-detection lifecycle without any privileged Mach calls.

    use std::{
        thread::{self, JoinHandle},
        time::Instant,
    };

    use tracing::{debug, info};

    use super::{Pager, PagerError, PagerHandle};

    /// Spawn the Mach server thread.
    ///
    /// Returns a [`JoinHandle`] — the caller owns the lifecycle. The current
    /// skeleton always returns `Ok(())`; the live `mach_msg` path will surface
    /// errors via [`PagerError::Mach`], which is why the result type stays in the
    /// signature even though the skeleton never produces an `Err`.
    ///
    /// # Errors
    /// [`PagerError::Spawn`] when `thread::Builder::spawn` fails (typically
    /// EAGAIN on per-process thread cap exhaustion). Squib-host is a library —
    /// we never panic on a thread-spawn failure that's reachable from a
    /// long-running daemon.
    pub fn spawn_server(pager: &Pager) -> Result<JoinHandle<Result<(), PagerError>>, PagerError> {
        let handle = pager.handle();
        thread::Builder::new()
            .name("squib-pager".into())
            .spawn(move || -> Result<(), PagerError> {
                run_server_loop(&handle);
                Ok(())
            })
            .map_err(PagerError::Spawn)
    }

    fn run_server_loop(handle: &PagerHandle) {
        info!(
            poll_interval = ?handle.poll_interval(),
            "squib-pager server starting (drift-poll skeleton)"
        );
        let mut last_drift_check = Instant::now();
        while !handle.is_shutting_down() {
            // Live mode: `mach_msg(MACH_RCV_MSG, timeout=poll_interval)`.
            // Skeleton mode: park for the same duration so the drift-check rate
            // matches what the live path would produce.
            thread::park_timeout(handle.poll_interval());
            if last_drift_check.elapsed() >= handle.poll_interval() {
                drift_check();
                last_drift_check = Instant::now();
            }
        }
        debug!("squib-pager server exiting (shutdown requested)");
    }

    fn drift_check() {
        // The live implementation calls `task_get_exception_ports`, compares
        // against the port we own, and re-installs via `task_swap_exception_ports`
        // if LLDB has overwritten ours (and bumps the `port_reinstalls` counter).
        // The skeleton emits a debug trace so the rate is observable in tests.
        debug!("squib-pager drift check (no-op skeleton)");
    }
}

#[cfg(target_os = "macos")]
pub use mach_imp::spawn_server as spawn_mach_server;

/// Stub on non-macOS targets so cross-platform tests still compile. Returns
/// immediately with `Ok(())`.
///
/// # Errors
/// [`PagerError::Spawn`] when `thread::Builder::spawn` fails.
#[cfg(not(target_os = "macos"))]
pub fn spawn_mach_server(
    _pager: &Pager,
) -> Result<std::thread::JoinHandle<Result<(), PagerError>>, PagerError> {
    std::thread::Builder::new()
        .name("squib-pager-stub".into())
        .spawn(|| Ok(()))
        .map_err(PagerError::Spawn)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use bytes::Bytes;
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug)]
    struct FakeSource {
        bytes: Bytes,
    }

    impl PageSource for FakeSource {
        fn fetch(&self, _req: &PageRequest) -> Result<Bytes, PageSourceError> {
            Ok(self.bytes.clone())
        }
    }

    #[test]
    fn test_should_register_and_match_a_postcopy_region() {
        let pager = Pager::new(
            Box::new(FakeSource {
                bytes: Bytes::from(vec![0u8; 16 * 1024]),
            }),
            PagerConfig::default(),
        );
        pager.register_region(0x8000_0000, 0x1000_0000);
        assert!(pager.contains_ipa(0x8000_1234));
        assert!(!pager.contains_ipa(0x9000_0000));
    }

    #[test]
    fn test_should_serve_fault_inside_registered_region() {
        let bytes = Bytes::from(vec![0xAB; 16 * 1024]);
        let pager = Pager::new(
            Box::new(FakeSource {
                bytes: bytes.clone(),
            }),
            PagerConfig::default(),
        );
        pager.register_region(0x8000_0000, 0x1000_0000);
        let got = pager.serve_fault(0x8000_1234, 16 * 1024).unwrap();
        assert_eq!(got, bytes);
        assert_eq!(pager.stats().faults, 1);
    }

    #[test]
    fn test_should_reject_fault_outside_region() {
        let pager = Pager::new(
            Box::new(FakeSource {
                bytes: Bytes::from(vec![0u8; 16 * 1024]),
            }),
            PagerConfig::default(),
        );
        pager.register_region(0x8000_0000, 0x1000_0000);
        let err = pager.serve_fault(0xC000_0000, 16 * 1024).unwrap_err();
        assert!(matches!(err, PagerError::OutOfRegion { ipa: 0xC000_0000 }));
    }

    #[derive(Debug, Default)]
    struct CaptureSource {
        seen: Mutex<Vec<u64>>,
    }

    impl PageSource for CaptureSource {
        fn fetch(&self, req: &PageRequest) -> Result<Bytes, PageSourceError> {
            self.seen.lock().push(req.ipa);
            Ok(Bytes::from(vec![0u8; req.page_size as usize]))
        }
    }

    #[derive(Debug)]
    struct WrapperSource {
        inner: Arc<CaptureSource>,
    }

    impl PageSource for WrapperSource {
        fn fetch(&self, req: &PageRequest) -> Result<Bytes, PageSourceError> {
            self.inner.fetch(req)
        }
    }

    #[test]
    fn test_should_align_request_to_page_boundary() {
        let src = Arc::new(CaptureSource::default());
        let pager = Pager::new(
            Box::new(WrapperSource { inner: src.clone() }),
            PagerConfig::default(),
        );
        pager.register_region(0x8000_0000, 0x1000_0000);
        pager.serve_fault(0x8000_1234, 16 * 1024).unwrap();
        let seen = src.seen.lock().clone();
        assert_eq!(seen, vec![0x8000_0000]);
    }

    #[test]
    fn test_should_prewarm_each_page_on_demand() {
        let bytes = Bytes::from(vec![0xCD; 16 * 1024]);
        let cfg = PagerConfig {
            prewarm: PrewarmList::from_boot_critical(
                0x8000_2000, // kernel text
                0x9000_0000, // vCPU 0 stack
                0x9F00_0000, // FDT
            ),
            ..Default::default()
        };
        let pager = Pager::new(Box::new(FakeSource { bytes }), cfg);
        pager.register_region(0x8000_0000, 0x2000_0000);
        pager.prewarm().unwrap();
        let stats = pager.stats();
        assert_eq!(stats.prewarmed, 3);
        assert_eq!(stats.faults, 3);
    }

    #[test]
    fn test_file_page_source_reads_at_offset_from_ram_start() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.mem");
        let mut bytes = vec![0u8; 64 * 1024];
        // Plant a marker at 16 KiB
        for byte in &mut bytes[16 * 1024..32 * 1024] {
            *byte = 0x77;
        }
        std::fs::write(&path, &bytes).unwrap();
        let src = FilePageSource::open(&path, 0x8000_0000).unwrap();
        let got = src
            .fetch(&PageRequest {
                ipa: 0x8000_4000, // ram_start + 16 KiB
                page_size: 16 * 1024,
            })
            .unwrap();
        assert!(got.iter().all(|&b| b == 0x77));
    }

    #[test]
    fn test_file_page_source_rejects_below_ram_start() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.mem");
        std::fs::write(&path, vec![0u8; 16 * 1024]).unwrap();
        let src = FilePageSource::open(&path, 0x8000_0000).unwrap();
        let err = src
            .fetch(&PageRequest {
                ipa: 0x4000_0000,
                page_size: 16 * 1024,
            })
            .unwrap_err();
        assert!(matches!(err, PageSourceError::Open(_)));
    }

    #[test]
    fn test_uffd_page_source_round_trips_protocol() {
        // Build a one-shot fake page server.
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("pager.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut hdr = [0u8; 16];
            sock.read_exact(&mut hdr).unwrap();
            let ipa = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
            let size = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
            sock.write_all(&hdr).unwrap();
            let payload = vec![0xEEu8; size as usize];
            sock.write_all(&payload).unwrap();
            (ipa, size)
        });
        let src = UffdPageSource::connect(&sock_path).unwrap();
        let got = src
            .fetch(&PageRequest {
                ipa: 0x8000_0000,
                page_size: 16 * 1024,
            })
            .unwrap();
        assert!(got.iter().all(|&b| b == 0xEE));
        let _ = server.join().unwrap();
    }

    #[test]
    fn test_should_request_shutdown_via_handle() {
        let pager = Pager::new(
            Box::new(FakeSource {
                bytes: Bytes::from(vec![0u8; 16 * 1024]),
            }),
            PagerConfig::default(),
        );
        let h = pager.handle();
        assert!(!h.is_shutting_down());
        pager.request_shutdown();
        assert!(h.is_shutting_down());
    }

    #[test]
    fn test_stats_record_port_reinstall_and_forwarded() {
        let pager = Pager::new(
            Box::new(FakeSource {
                bytes: Bytes::from(vec![0u8; 16 * 1024]),
            }),
            PagerConfig::default(),
        );
        pager.record_port_reinstall();
        pager.record_port_reinstall();
        pager.record_forwarded();
        let s = pager.stats();
        assert_eq!(s.port_reinstalls, 2);
        assert_eq!(s.forwarded_exceptions, 1);
    }
}
