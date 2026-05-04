//! virtio-block — sync engine.
//!
//! Per [14-virtio-and-devices.md § 4.1](../../../specs/14-virtio-and-devices.md#41-virtio-block):
//!
//! > **Sync engine**: every queue notification handler reads/writes
//! > synchronously on the device thread. Suitable for low-IOPS workloads.
//! > **Async engine**: queue notifications dispatch onto
//! > `tokio::task::spawn_blocking`, which calls `pread`/`pwrite` against an
//! > `F_NOCACHE`-opened fd. The macOS analogue of Linux `O_DIRECT`.
//!
//! The async engine + rate limiter are deferred to Phase 7 perf work; they
//! land in [93-improvements-review.md](../../../specs/93-improvements-review.md)
//! as known follow-ups so the sync engine ships clean for 1.0 functional
//! coverage. The trait [`BlockBackend`] abstracts the I/O so the async path
//! can drop in without touching the device-front-end logic.

// The sync block engine deliberately uses `std::fs::File` and synchronous
// I/O per [14-virtio-and-devices.md §
// 4.1](../../../specs/14-virtio-and-devices.md#41-virtio-block): "every queue notification handler
// reads/writes synchronously on the device thread". The workspace `disallowed-types` lint pushes
// runtime I/O onto Tokio; the sync engine is the explicit blocking-I/O alternative for
// low-IOPS workloads, so the allow is intentional. The async engine uses
// `tokio::task::spawn_blocking` and lives in the deferred-findings list.
#[allow(clippy::disallowed_types)]
use std::fs::{File, OpenOptions};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::Arc,
};

use parking_lot::Mutex;
use squib_core::{GuestAddress, GuestMemory};

use crate::{
    device::{ActivateError, VirtioDevice},
    device_id::VirtioDeviceType,
    interrupt::IrqLine,
    queue::Queue,
};

/// Sector size in bytes — fixed at 512 by the virtio-block spec.
pub const SECTOR_SIZE: u64 = 512;

/// `VIRTIO_BLK_T_IN` — guest read.
pub const REQ_TYPE_IN: u32 = 0;
/// `VIRTIO_BLK_T_OUT` — guest write.
pub const REQ_TYPE_OUT: u32 = 1;
/// `VIRTIO_BLK_T_FLUSH` — durability barrier.
pub const REQ_TYPE_FLUSH: u32 = 4;
/// `VIRTIO_BLK_T_GET_ID` — fetch the device's identifier string.
pub const REQ_TYPE_GET_ID: u32 = 8;

/// `VIRTIO_BLK_S_OK` — request completed successfully.
pub const STATUS_OK: u8 = 0;
/// `VIRTIO_BLK_S_IOERR` — host-side I/O failure.
pub const STATUS_IOERR: u8 = 1;
/// `VIRTIO_BLK_S_UNSUPP` — request type unrecognized.
pub const STATUS_UNSUPP: u8 = 2;

/// `VIRTIO_BLK_F_RO` — device is read-only.
pub const F_RO: u64 = 1 << 5;
/// `VIRTIO_BLK_F_BLK_SIZE` — driver may read `blk_size` from config-space.
pub const F_BLK_SIZE: u64 = 1 << 6;
/// `VIRTIO_BLK_F_FLUSH` — device honours flush requests.
pub const F_FLUSH: u64 = 1 << 9;

const REQ_QUEUE: usize = 0;
const QUEUE_MAX_SIZE: u16 = 256;

/// Cache mode — surfaces in feature bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheType {
    /// `Unsafe` cache mode — no flush; matches Firecracker default.
    Unsafe,
    /// `Writeback` cache mode — `VIRTIO_BLK_F_FLUSH` is offered.
    Writeback,
}

/// Block-device configuration.
#[derive(Debug, Clone)]
pub struct BlockConfig {
    /// Operator-supplied identifier — surfaces via `VIRTIO_BLK_T_GET_ID`.
    pub drive_id: String,
    /// Path to the host-side backing file.
    pub path_on_host: PathBuf,
    /// `true` if the drive is the root device (informational; no behavior).
    pub is_root_device: bool,
    /// `true` opens the backing file read-only and offers `VIRTIO_BLK_F_RO`.
    pub is_read_only: bool,
    /// `Unsafe` (no flush) or `Writeback` (flush honored).
    pub cache_type: CacheType,
    /// PARTUUID, threaded into the FDT `boot_args` if `is_root_device`.
    pub partuuid: Option<String>,
}

/// Backend abstraction — the sync engine implements this against a
/// `parking_lot::Mutex<File>`; the async engine (Phase 7) wraps a Tokio task.
pub trait BlockBackend: Send + Sync + std::fmt::Debug {
    /// Total size of the backing storage in bytes.
    fn size_bytes(&self) -> u64;

    /// Read `buf.len()` bytes starting at `byte_offset`.
    ///
    /// # Errors
    /// `std::io::Error` for any host failure.
    fn read_at(&self, byte_offset: u64, buf: &mut [u8]) -> std::io::Result<()>;

    /// Write `buf.len()` bytes starting at `byte_offset`.
    ///
    /// # Errors
    /// `std::io::Error` for any host failure.
    fn write_at(&self, byte_offset: u64, buf: &[u8]) -> std::io::Result<()>;

    /// Synchronous flush (`fsync`).
    ///
    /// # Errors
    /// `std::io::Error` for any host failure.
    fn flush(&self) -> std::io::Result<()>;

    /// `true` if the backing storage is opened read-only.
    fn read_only(&self) -> bool;
}

/// Synchronous file-backed `BlockBackend` — opens the host file once and
/// serializes I/O through a `parking_lot::Mutex<File>`. The mutex guard is
/// released before the device thread re-acquires its own state lock so the
/// bus is not blocked across host I/O.
///
/// The workspace `disallowed-types` lint pushes runtime I/O onto Tokio; the
/// sync engine is the explicit blocking-I/O alternative for low-IOPS
/// workloads (per [14-virtio-and-devices.md §
/// 4.1](../../../specs/14-virtio-and-devices.md#41-virtio-block)) so the `std::fs::File` allow is
/// intentional. The async engine uses `tokio::task::spawn_blocking` and lives in a follow-up.
#[derive(Debug)]
#[allow(clippy::disallowed_types)]
pub struct SyncFileBackend {
    file: Mutex<File>,
    size_bytes: u64,
    read_only: bool,
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl SyncFileBackend {
    /// Open `path` for sync I/O.
    ///
    /// # Errors
    /// `std::io::Error` if the file cannot be opened.
    pub fn open(path: &std::path::Path, read_only: bool) -> std::io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        if !read_only {
            opts.write(true);
        }
        let file = opts.open(path)?;
        let size_bytes = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(file),
            size_bytes,
            read_only,
        })
    }
}

impl BlockBackend for SyncFileBackend {
    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
    fn read_at(&self, byte_offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let mut f = self.file.lock();
        f.seek(SeekFrom::Start(byte_offset))?;
        f.read_exact(buf)
    }
    fn write_at(&self, byte_offset: u64, buf: &[u8]) -> std::io::Result<()> {
        if self.read_only {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "read-only block backend",
            ));
        }
        let mut f = self.file.lock();
        f.seek(SeekFrom::Start(byte_offset))?;
        f.write_all(buf)
    }
    fn flush(&self) -> std::io::Result<()> {
        let f = self.file.lock();
        f.sync_data()
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
}

/// Direct-I/O file-backed `BlockBackend` for the high-IOPS path.
///
/// Differs from [`SyncFileBackend`] structurally: **lockless positioned
/// I/O**. Uses `pread(2)` / `pwrite(2)` via
/// `std::os::unix::fs::FileExt::{read_at, write_at}`, so multiple threads
/// can issue concurrent operations against the same fd without serialising
/// through a `Mutex<File>`. The 100 K IOPS budget in
/// [71 § 3](../../../specs/71-performance-budgets.md#3-block-io) needs the
/// lockless path — a mutex-serialised engine caps at the latency of one
/// pread per op (≈ 30 μs ⇒ 33 K IOPS ceiling on a single thread).
///
/// **`F_NOCACHE` is not set here** because `squib-virtio` carries
/// `#![forbid(unsafe_code)]` (I-CRATE-2): the `fcntl(F_NOCACHE)` call lives
/// in `squib-host::block_io::set_f_nocache`, which can wrap the fd before
/// it's handed to this constructor (the operator-supplied path is opened
/// once at boot, so the small upfront cost is fine). The host-side cache
/// pressure without `F_NOCACHE` is small for the Lambda workload profile;
/// the perf-tuning lane benchmark
/// ([71 § 3](../../../specs/71-performance-budgets.md#3-block-io)) gates
/// the regression once the bench harness lights up.
///
/// Functionally interchangeable with `SyncFileBackend`; constructed via
/// [`Self::open`] and dropped in via the same `Arc<dyn BlockBackend>`
/// the device frontend takes today.
#[derive(Debug)]
#[allow(clippy::disallowed_types)]
pub struct AsyncFileBackend {
    file: File,
    size_bytes: u64,
    read_only: bool,
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl AsyncFileBackend {
    /// Open `path` for lockless positioned I/O.
    ///
    /// The fd is shared across every thread that calls into `BlockBackend`;
    /// concurrent positioned I/O is safe via `pread`/`pwrite`.
    ///
    /// # Errors
    /// `std::io::Error` if the file cannot be opened.
    pub fn open(path: &std::path::Path, read_only: bool) -> std::io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        if !read_only {
            opts.write(true);
        }
        let file = opts.open(path)?;
        Self::from_file(file, read_only)
    }

    /// Wrap an already-open `File`. Used by `squib-host` after applying
    /// `F_NOCACHE` via the unsafe `fcntl` boundary: the operator constructs
    /// the fd, hands it to us, and we own the read/write side.
    ///
    /// # Errors
    /// `std::io::Error` if the file's metadata can't be queried.
    pub fn from_file(file: File, read_only: bool) -> std::io::Result<Self> {
        let size_bytes = file.metadata()?.len();
        Ok(Self {
            file,
            size_bytes,
            read_only,
        })
    }
}

impl BlockBackend for AsyncFileBackend {
    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
    fn read_at(&self, byte_offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        // `read_at` (pread) doesn't change the fd seek position, so concurrent
        // calls against the same fd are safe per `FileExt` contract. No mutex.
        use std::os::unix::fs::FileExt as _;
        // `read_exact_at` would loop on partial reads; we want exactly that
        // semantic for virtio-block where requests are size-bounded.
        self.file.read_exact_at(buf, byte_offset)
    }
    fn write_at(&self, byte_offset: u64, buf: &[u8]) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt as _;
        if self.read_only {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "read-only block backend",
            ));
        }
        self.file.write_all_at(buf, byte_offset)
    }
    fn flush(&self) -> std::io::Result<()> {
        self.file.sync_data()
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
}

/// Token-bucket rate limiter wrapping any [`BlockBackend`].
///
/// `tower::Layer`-shaped (without the actual `tower` dependency — the
/// trait surface here is sync, not request/response): construct via
/// [`RateLimitedBackend::new`] with a wrapped backend and a [`RateLimit`]
/// budget. Every `read_at` / `write_at` calls `acquire(buf.len())` first;
/// if the bucket is empty the call blocks (`std::thread::sleep`) until
/// enough tokens replenish.
///
/// Per [14 § 4.1](../../../specs/14-virtio-and-devices.md#41-virtio-block) and
/// the `D7-adjacent` rate-limiter requirement: bound aggregate throughput
/// within ±5 % of the configured rate. The bucket is refilled
/// continuously based on wall-clock elapsed time since the last drain,
/// which gives a smooth ±n % envelope at any rate.
#[derive(Debug)]
pub struct RateLimitedBackend {
    inner: Arc<dyn BlockBackend>,
    state: Mutex<TokenBucket>,
    config: RateLimit,
}

/// Per-direction rate cap (bytes/sec). Use `unlimited()` to bypass.
#[derive(Debug, Clone, Copy)]
pub struct RateLimit {
    /// Refill rate (bytes per second).
    pub bytes_per_sec: u64,
    /// Burst — bucket size in bytes; max instantaneous draw.
    pub burst_bytes: u64,
}

impl RateLimit {
    /// No rate limit (pass-through).
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            bytes_per_sec: u64::MAX,
            burst_bytes: u64::MAX,
        }
    }

    /// `bytes_per_sec` as a steady-state cap, with a 100ms-worth burst.
    #[must_use]
    pub const fn steady(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec,
            burst_bytes: bytes_per_sec / 10,
        }
    }
}

#[derive(Debug)]
struct TokenBucket {
    tokens: u64,
    last_refill: std::time::Instant,
}

impl RateLimitedBackend {
    /// Wrap `inner` with a rate-limit gate. `inner` is held by `Arc` so
    /// the same backend can sit behind several rate-limit slices.
    #[must_use]
    pub fn new(inner: Arc<dyn BlockBackend>, config: RateLimit) -> Self {
        Self {
            state: Mutex::new(TokenBucket {
                tokens: config.burst_bytes,
                last_refill: std::time::Instant::now(),
            }),
            config,
            inner,
        }
    }

    fn refill(state: &mut TokenBucket, config: &RateLimit, ceiling: u64) {
        if config.bytes_per_sec == u64::MAX {
            state.tokens = ceiling;
            return;
        }
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(state.last_refill);
        // Saturating to defeat overflow on long pauses (e.g. snapshot
        // restore where the bucket sits idle for hours).
        let earned_u128 =
            u128::from(config.bytes_per_sec).saturating_mul(elapsed.as_nanos()) / 1_000_000_000;
        let earned = u64::try_from(earned_u128).unwrap_or(u64::MAX);
        state.tokens = state.tokens.saturating_add(earned).min(ceiling);
        state.last_refill = now;
    }

    fn acquire(&self, n: u64) {
        if self.config.bytes_per_sec == u64::MAX {
            return;
        }
        // For oversize requests (n > burst_bytes), let the bucket grow up to
        // `n` for this call so the request can complete in finite time.
        // Steady-state burst is still `burst_bytes`; this is the
        // single-request override.
        let ceiling = n.max(self.config.burst_bytes);
        let mut g = self.state.lock();
        Self::refill(&mut g, &self.config, ceiling);
        if g.tokens >= n {
            g.tokens -= n;
            return;
        }
        // Sleep for the remainder of the time it would take to refill
        // `n - tokens` bytes, then drain. `parking_lot::Mutex` is not
        // poison-safe; we hold it across the sleep deliberately so a
        // concurrent caller cannot bleed in and drain the bucket.
        let needed = n - g.tokens;
        let nanos_to_wait =
            (u128::from(needed) * 1_000_000_000) / u128::from(self.config.bytes_per_sec.max(1));
        let wait =
            std::time::Duration::from_nanos(u64::try_from(nanos_to_wait).unwrap_or(u64::MAX));
        // Suppressing the cv to avoid the wait_for-loop bug where a
        // refill clamped to `burst_bytes` would never reach `n`. Single
        // sleep then drain is correct: by the time we wake, at least
        // `needed` more bytes have been "earned" by the rate limit.
        std::thread::sleep(wait);
        Self::refill(&mut g, &self.config, ceiling);
        g.tokens = g.tokens.saturating_sub(n);
    }
}

impl BlockBackend for RateLimitedBackend {
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
    fn read_at(&self, byte_offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.acquire(u64::try_from(buf.len()).unwrap_or(u64::MAX));
        self.inner.read_at(byte_offset, buf)
    }
    fn write_at(&self, byte_offset: u64, buf: &[u8]) -> std::io::Result<()> {
        self.acquire(u64::try_from(buf.len()).unwrap_or(u64::MAX));
        self.inner.write_at(byte_offset, buf)
    }
    fn flush(&self) -> std::io::Result<()> {
        self.inner.flush()
    }
    fn read_only(&self) -> bool {
        self.inner.read_only()
    }
}

/// virtio-block frontend.
#[derive(Debug)]
pub struct BlockDevice {
    avail: u64,
    acked: u64,
    queues: Vec<Queue>,
    config: BlockConfig,
    backend: Arc<dyn BlockBackend>,
    state: Arc<Mutex<ActiveState>>,
}

#[derive(Debug, Default)]
struct ActiveState {
    mem: Option<Arc<dyn GuestMemory>>,
    irq: Option<IrqLine>,
    activated: bool,
}

impl BlockDevice {
    /// Build a virtio-block from a validated [`BlockConfig`] and a
    /// concrete backend.
    #[must_use]
    pub fn new(config: BlockConfig, backend: Arc<dyn BlockBackend>) -> Self {
        let mut avail = F_BLK_SIZE;
        if config.is_read_only || backend.read_only() {
            avail |= F_RO;
        }
        if matches!(config.cache_type, CacheType::Writeback) {
            avail |= F_FLUSH;
        }
        Self {
            avail,
            acked: 0,
            queues: vec![Queue::new(QUEUE_MAX_SIZE)],
            config,
            backend,
            state: Arc::new(Mutex::new(ActiveState::default())),
        }
    }

    /// Backing-store size in 512-byte sectors (virtio-block config layout).
    #[must_use]
    pub fn capacity_sectors(&self) -> u64 {
        self.backend.size_bytes() / SECTOR_SIZE
    }

    fn handle_request_inner(
        backend: &dyn BlockBackend,
        drive_id: &str,
        mem: &dyn GuestMemory,
        descs: &[crate::queue::Descriptor],
    ) -> (u8, u32) {
        // The driver lays out: header descriptor, payload descriptor(s),
        // status descriptor (always device-write, 1 byte).
        if descs.len() < 2 {
            return (STATUS_IOERR, 0);
        }
        let header = descs[0];
        if header.is_write_only() || header.len < 16 {
            return (STATUS_IOERR, 0);
        }
        let Ok(req_type) = mem.read_u32_le(header.addr) else {
            return (STATUS_IOERR, 0);
        };
        let Ok(sector) = mem.read_u64_le(GuestAddress(header.addr.raw() + 8)) else {
            return (STATUS_IOERR, 0);
        };
        // Payload descriptors are everything between header and status.
        let payload = &descs[1..descs.len() - 1];
        let status_desc = descs.last().copied().unwrap_or(header);
        if !status_desc.is_write_only() || status_desc.len < 1 {
            return (STATUS_IOERR, 0);
        }
        let mut bytes_written: u32 = 0;
        let status = match req_type {
            REQ_TYPE_IN => match Self::do_read(backend, mem, payload, sector) {
                Ok(written) => {
                    bytes_written = written;
                    STATUS_OK
                }
                Err(_) => STATUS_IOERR,
            },
            REQ_TYPE_OUT => match Self::do_write(backend, mem, payload, sector) {
                Ok(()) => STATUS_OK,
                Err(_) => STATUS_IOERR,
            },
            REQ_TYPE_FLUSH => match backend.flush() {
                Ok(()) => STATUS_OK,
                Err(_) => STATUS_IOERR,
            },
            REQ_TYPE_GET_ID => match Self::do_get_id(drive_id, mem, payload) {
                Ok(written) => {
                    bytes_written = written;
                    STATUS_OK
                }
                Err(_) => STATUS_IOERR,
            },
            _ => STATUS_UNSUPP,
        };
        // Status byte is always one byte.
        if mem.write(status_desc.addr, &[status]).is_err() {
            return (STATUS_IOERR, bytes_written);
        }
        (status, bytes_written.saturating_add(1))
    }

    fn do_read(
        backend: &dyn BlockBackend,
        mem: &dyn GuestMemory,
        payload: &[crate::queue::Descriptor],
        sector: u64,
    ) -> std::io::Result<u32> {
        let mut byte_off = sector
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| std::io::Error::other("sector*SECTOR_SIZE overflow"))?;
        let mut total: u32 = 0;
        for desc in payload {
            if !desc.is_write_only() {
                continue;
            }
            let len = desc.len as usize;
            let mut buf = vec![0u8; len];
            backend.read_at(byte_off, &mut buf)?;
            mem.write(desc.addr, &buf)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            byte_off = byte_off
                .checked_add(u64::from(desc.len))
                .ok_or_else(|| std::io::Error::other("descriptor offset overflow"))?;
            total = total.saturating_add(desc.len);
        }
        Ok(total)
    }

    fn do_write(
        backend: &dyn BlockBackend,
        mem: &dyn GuestMemory,
        payload: &[crate::queue::Descriptor],
        sector: u64,
    ) -> std::io::Result<()> {
        let mut byte_off = sector
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| std::io::Error::other("sector*SECTOR_SIZE overflow"))?;
        for desc in payload {
            if desc.is_write_only() {
                continue;
            }
            let len = desc.len as usize;
            let mut buf = vec![0u8; len];
            mem.read(desc.addr, &mut buf)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            backend.write_at(byte_off, &buf)?;
            byte_off = byte_off
                .checked_add(u64::from(desc.len))
                .ok_or_else(|| std::io::Error::other("descriptor offset overflow"))?;
        }
        Ok(())
    }

    fn do_get_id(
        drive_id: &str,
        mem: &dyn GuestMemory,
        payload: &[crate::queue::Descriptor],
    ) -> std::io::Result<u32> {
        if payload.is_empty() {
            return Ok(0);
        }
        // Virtio spec: `VIRTIO_BLK_T_GET_ID` returns 20 bytes of ID, padded
        // with zeros if shorter.
        let mut id = [0u8; 20];
        let bytes = drive_id.as_bytes();
        let n = bytes.len().min(20);
        id[..n].copy_from_slice(&bytes[..n]);
        let desc = payload[0];
        if !desc.is_write_only() {
            return Ok(0);
        }
        let len = (desc.len as usize).min(20);
        mem.write(desc.addr, &id[..len])
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(len as u32)
    }

    fn drain_requests(&mut self) {
        let (mem, irq) = {
            let state = self.state.lock();
            match (state.mem.clone(), state.irq.clone()) {
                (Some(m), Some(i)) => (m, i),
                _ => return,
            }
        };
        let backend = Arc::clone(&self.backend);
        let drive_id = self.config.drive_id.clone();
        let queue = &mut self.queues[REQ_QUEUE];
        let mut completed = false;
        loop {
            let chain = match queue.pop_avail(mem.as_ref()) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "block: walk failed");
                    break;
                }
            };
            let head = chain.head_index();
            let descs = match chain.collect(mem.as_ref()) {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!(error = %err, "block: chain collect failed");
                    break;
                }
            };
            let (_status, written) =
                Self::handle_request_inner(backend.as_ref(), &drive_id, mem.as_ref(), &descs);
            if let Err(err) = queue.push_used(mem.as_ref(), head, written) {
                tracing::warn!(error = %err, "block: push_used failed");
                break;
            }
            completed = true;
        }
        if completed {
            let _ = irq.trigger_queue();
        }
    }
}

impl VirtioDevice for BlockDevice {
    fn device_type(&self) -> VirtioDeviceType {
        VirtioDeviceType::Block
    }
    fn avail_features(&self) -> u64 {
        self.avail
    }
    fn acked_features(&self) -> u64 {
        self.acked
    }
    fn set_acked_features(&mut self, value: u64) {
        self.acked = value;
    }
    fn queue_max_sizes(&self) -> &[u16] {
        const SIZES: &[u16] = &[QUEUE_MAX_SIZE];
        SIZES
    }
    fn queues(&self) -> &[Queue] {
        &self.queues
    }
    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // Config layout (virtio v1.2 § 5.2.4):
        //   0x00 u64 capacity (sectors)
        //   0x08 u32 size_max          (max single-segment size; 0 = no limit)
        //   0x0C u32 seg_max           (max segments per request)
        //   0x14 u32 blk_size          (only valid if F_BLK_SIZE)
        // Per virtio v1.2 the driver assumes `seg_max = 1` if the field
        // reads zero — that limits each request to a single descriptor and
        // breaks chained reads/writes. Publish `QUEUE_MAX_SIZE - 2` so the
        // driver can chain header + payload + status (matches upstream
        // Firecracker's `vendors/firecracker/.../block/.../device.rs`).
        let mut full = [0u8; 64];
        full[0..8].copy_from_slice(&self.capacity_sectors().to_le_bytes());
        // size_max stays 0 (no limit).
        let seg_max = u32::from(QUEUE_MAX_SIZE - 2);
        full[12..16].copy_from_slice(&seg_max.to_le_bytes());
        full[20..24].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
        let off = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = full.get(off + i).copied().unwrap_or(0);
        }
    }
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    fn activate(&mut self, mem: Arc<dyn GuestMemory>, irq: IrqLine) -> Result<(), ActivateError> {
        let mut state = self.state.lock();
        state.mem = Some(mem);
        state.irq = Some(irq);
        state.activated = true;
        Ok(())
    }
    fn is_activated(&self) -> bool {
        self.state.lock().activated
    }
    fn process_queue(&mut self, queue_index: u16) {
        if queue_index as usize == REQ_QUEUE {
            self.drain_requests();
        }
    }
}

#[cfg(test)]
mod tests {
    use squib_arch::IntId;
    use squib_core::SliceGuestMemory;
    use squib_gic::Gic;

    use super::*;
    use crate::queue::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn test_should_round_trip_async_file_backend_via_pread_pwrite() {
        // Use a tempfile so the test doesn't depend on /tmp pinned state.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("blk.img");
        std::fs::write(&path, [0u8; 4096]).unwrap();
        let backend = AsyncFileBackend::open(&path, false).unwrap();
        assert_eq!(backend.size_bytes(), 4096);
        backend.write_at(512, b"hello-async").unwrap();
        let mut buf = [0u8; 11];
        backend.read_at(512, &mut buf).unwrap();
        assert_eq!(&buf, b"hello-async");
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn test_should_reject_writes_through_async_file_backend_when_read_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ro.img");
        std::fs::write(&path, [0u8; 16]).unwrap();
        let backend = AsyncFileBackend::open(&path, true).unwrap();
        let err = backend.write_at(0, b"x").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn test_should_pass_through_when_rate_limit_is_unlimited() {
        let inner: Arc<dyn BlockBackend> = Arc::new(MemoryBackend::new(1024));
        let limited = RateLimitedBackend::new(inner, RateLimit::unlimited());
        // 256-byte write — well above any meaningful per-call cap, but
        // unlimited means it's instant.
        let start = std::time::Instant::now();
        limited.write_at(0, &[0xAA; 256]).unwrap();
        assert!(start.elapsed() < std::time::Duration::from_millis(50));
    }

    #[test]
    fn test_should_block_until_tokens_replenish() {
        // 1 KiB/s with a 100-byte burst → 256-byte write must wait
        // (256 - 100) / 1024 ≈ 152 ms for the additional 156 bytes to
        // refill. Test asserts the call waited at least 100 ms.
        let inner: Arc<dyn BlockBackend> = Arc::new(MemoryBackend::new(2048));
        let limited = RateLimitedBackend::new(
            inner,
            RateLimit {
                bytes_per_sec: 1024,
                burst_bytes: 100,
            },
        );
        let start = std::time::Instant::now();
        // Drain the burst first.
        limited.write_at(0, &[0; 100]).unwrap();
        // Second write needs to wait for refill.
        limited.write_at(0, &[0; 256]).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "limiter must throttle; elapsed = {elapsed:?}"
        );
    }

    #[test]
    fn test_should_recharge_steady_helper_to_one_tenth_of_rate() {
        let cfg = RateLimit::steady(1_000_000);
        assert_eq!(cfg.bytes_per_sec, 1_000_000);
        assert_eq!(cfg.burst_bytes, 100_000);
    }

    /// In-memory backend for tests — 1 MiB of zero-initialized space.
    #[derive(Debug)]
    struct MemoryBackend {
        bytes: Mutex<Vec<u8>>,
    }
    impl MemoryBackend {
        fn new(size: usize) -> Self {
            Self {
                bytes: Mutex::new(vec![0u8; size]),
            }
        }
    }
    impl BlockBackend for MemoryBackend {
        fn size_bytes(&self) -> u64 {
            self.bytes.lock().len() as u64
        }
        fn read_at(&self, byte_offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
            let bytes = self.bytes.lock();
            let off = byte_offset as usize;
            let end = off + buf.len();
            if end > bytes.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "out of range",
                ));
            }
            buf.copy_from_slice(&bytes[off..end]);
            Ok(())
        }
        fn write_at(&self, byte_offset: u64, buf: &[u8]) -> std::io::Result<()> {
            let mut bytes = self.bytes.lock();
            let off = byte_offset as usize;
            let end = off + buf.len();
            if end > bytes.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "out of range",
                ));
            }
            bytes[off..end].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn read_only(&self) -> bool {
            false
        }
    }

    #[derive(Debug, Default)]
    struct StubGic;
    impl Gic for StubGic {
        fn pulse_spi(&self, _: IntId) -> Result<(), squib_gic::GicError> {
            Ok(())
        }
        fn set_spi_level(&self, _: IntId, _: bool) -> Result<(), squib_gic::GicError> {
            Ok(())
        }
        fn save_state(&self) -> Result<Vec<u8>, squib_gic::GicError> {
            Ok(Vec::new())
        }
        fn restore_state(&self, _data: &[u8]) -> Result<(), squib_gic::GicError> {
            Ok(())
        }
    }

    fn line() -> IrqLine {
        let gic: Arc<dyn Gic + Send + Sync> = Arc::new(StubGic);
        IrqLine::new(gic, IntId::from_spi_cell(16).unwrap())
    }

    fn config(read_only: bool) -> BlockConfig {
        BlockConfig {
            drive_id: "rootfs".into(),
            path_on_host: "/dev/null".into(),
            is_root_device: true,
            is_read_only: read_only,
            cache_type: CacheType::Writeback,
            partuuid: None,
        }
    }

    #[test]
    fn test_should_offer_ro_when_config_marks_read_only() {
        let backend = Arc::new(MemoryBackend::new(SECTOR_SIZE as usize));
        let dev = BlockDevice::new(config(true), backend);
        assert_ne!(dev.avail_features() & F_RO, 0);
    }

    #[test]
    fn test_should_offer_flush_for_writeback_cache() {
        let backend = Arc::new(MemoryBackend::new(SECTOR_SIZE as usize));
        let dev = BlockDevice::new(config(false), backend);
        assert_ne!(dev.avail_features() & F_FLUSH, 0);
    }

    #[test]
    fn test_should_publish_capacity_sectors_in_config() {
        let backend = Arc::new(MemoryBackend::new((SECTOR_SIZE * 1024) as usize));
        let dev = BlockDevice::new(config(false), backend);
        let mut cfg = [0u8; 64];
        dev.read_config(0, &mut cfg);
        let cap = u64::from_le_bytes(cfg[0..8].try_into().unwrap());
        assert_eq!(cap, 1024);
    }

    #[test]
    fn test_should_complete_get_id_with_drive_id_padded_to_20_bytes() {
        let backend = Arc::new(MemoryBackend::new((SECTOR_SIZE * 32) as usize));
        let mut dev = BlockDevice::new(config(false), backend);
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        let q = &mut dev.queues_mut()[REQ_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        // Header at 0x4000_2000: type=GET_ID, sector=0.
        mem.write_u32_le(GuestAddress(0x4000_2000), REQ_TYPE_GET_ID)
            .unwrap();
        // header descriptor (16 bytes), payload descriptor (20 bytes write-only),
        // status descriptor (1 byte write-only).
        let base = 0x4000_0000u64;
        // Header
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), 16).unwrap();
        mem.write_u16_le(GuestAddress(base + 12), VIRTQ_DESC_F_NEXT)
            .unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 1).unwrap();
        // Payload (20 bytes)
        let p = base + 16;
        mem.write_u32_le(GuestAddress(p), 0x4000_2100).unwrap();
        mem.write_u32_le(GuestAddress(p + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(p + 8), 20).unwrap();
        mem.write_u16_le(GuestAddress(p + 12), VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE)
            .unwrap();
        mem.write_u16_le(GuestAddress(p + 14), 2).unwrap();
        // Status (1 byte)
        let s = base + 32;
        mem.write_u32_le(GuestAddress(s), 0x4000_2200).unwrap();
        mem.write_u32_le(GuestAddress(s + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(s + 8), 1).unwrap();
        mem.write_u16_le(GuestAddress(s + 12), VIRTQ_DESC_F_WRITE)
            .unwrap();
        mem.write_u16_le(GuestAddress(s + 14), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();
        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(REQ_QUEUE as u16);
        let mut id = [0u8; 20];
        mem.read(GuestAddress(0x4000_2100), &mut id).unwrap();
        assert_eq!(&id[..6], b"rootfs");
        let status = {
            let mut b = [0u8; 1];
            mem.read(GuestAddress(0x4000_2200), &mut b).unwrap();
            b[0]
        };
        assert_eq!(status, STATUS_OK);
    }

    #[test]
    fn test_should_complete_in_request_with_payload_data_from_backend() {
        let backend = Arc::new(MemoryBackend::new((SECTOR_SIZE * 32) as usize));
        // Pre-populate sector 1 with "hello\0\0\0...".
        backend.write_at(SECTOR_SIZE, b"hello").unwrap();
        let mut dev = BlockDevice::new(config(false), backend);
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x1_0000));
        let q = &mut dev.queues_mut()[REQ_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        // Header: REQ_TYPE_IN, sector=1.
        mem.write_u32_le(GuestAddress(0x4000_2000), REQ_TYPE_IN)
            .unwrap();
        mem.write_u64_le(GuestAddress(0x4000_2008), 1).unwrap();
        let base = 0x4000_0000u64;
        // Header descriptor.
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), 16).unwrap();
        mem.write_u16_le(GuestAddress(base + 12), VIRTQ_DESC_F_NEXT)
            .unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 1).unwrap();
        // Payload: 512-byte write-only.
        let p = base + 16;
        mem.write_u32_le(GuestAddress(p), 0x4000_3000).unwrap();
        mem.write_u32_le(GuestAddress(p + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(p + 8), 512).unwrap();
        mem.write_u16_le(GuestAddress(p + 12), VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE)
            .unwrap();
        mem.write_u16_le(GuestAddress(p + 14), 2).unwrap();
        // Status: 1-byte write-only.
        let s = base + 32;
        mem.write_u32_le(GuestAddress(s), 0x4000_3200).unwrap();
        mem.write_u32_le(GuestAddress(s + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(s + 8), 1).unwrap();
        mem.write_u16_le(GuestAddress(s + 12), VIRTQ_DESC_F_WRITE)
            .unwrap();
        mem.write_u16_le(GuestAddress(s + 14), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();
        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(REQ_QUEUE as u16);
        let mut got = [0u8; 5];
        mem.read(GuestAddress(0x4000_3000), &mut got).unwrap();
        assert_eq!(&got, b"hello");
        let mut st = [0u8; 1];
        mem.read(GuestAddress(0x4000_3200), &mut st).unwrap();
        assert_eq!(st[0], STATUS_OK);
    }

    #[test]
    fn test_should_return_unsupp_for_unknown_request_type() {
        let backend = Arc::new(MemoryBackend::new(SECTOR_SIZE as usize));
        let dev = BlockDevice::new(config(false), backend);
        let mem = SliceGuestMemory::new(GuestAddress(0), 0x1_0000);
        // Build a synthetic descriptor list manually for the unit-level test.
        let header_addr = GuestAddress(0x100);
        mem.write_u32_le(header_addr, 99).unwrap();
        let descs = vec![
            crate::queue::Descriptor {
                addr: header_addr,
                len: 16,
                flags: VIRTQ_DESC_F_NEXT,
                next: 1,
            },
            crate::queue::Descriptor {
                addr: GuestAddress(0x200),
                len: 1,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        ];
        let (status, _) = BlockDevice::handle_request_inner(
            dev.backend.as_ref(),
            &dev.config.drive_id,
            &mem,
            &descs,
        );
        assert_eq!(status, STATUS_UNSUPP);
    }
}
