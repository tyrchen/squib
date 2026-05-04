//! Direct-I/O helper used by the high-IOPS virtio-block engine.
//!
//! `squib-virtio` is `#![forbid(unsafe_code)]` (I-CRATE-2), so the
//! `fcntl(F_NOCACHE)` call that turns off the host buffer cache cannot
//! live in `squib-virtio::devices::block`. It lives here in `squib-host`
//! instead, where the unsafe-allowed boundary is already documented.
//!
//! The flow:
//!
//! 1. Operator-supplied path is opened (read-only or read-write) by the caller via
//!    `std::fs::OpenOptions`.
//! 2. The resulting `File` is handed to [`set_f_nocache`] which flips the `F_NOCACHE` flag.
//! 3. The `File` is then handed to `squib_virtio::devices::block::AsyncFileBackend::from_file`,
//!    which holds it for the lifetime of the device.
//!
//! `F_NOCACHE` is the macOS analogue of Linux `O_DIRECT`: future
//! `pread`/`pwrite` calls bypass the unified buffer cache, which keeps
//! host RSS bounded for write-heavy block workloads (every cached host
//! byte is also a guest-page-cache byte; double-caching wastes memory).
//!
//! Per [71 § 3](../../../specs/71-performance-budgets.md#3-block-io)
//! and the deferred Phase 7 perf-tuning lane.

#[cfg(target_os = "macos")]
#[allow(clippy::disallowed_types)]
use std::fs::File;

/// Enable `F_NOCACHE` on a freshly-opened fd. Best-effort — some
/// filesystems (network mounts, encrypted volumes) reject the flag and
/// surface the error. In that case the caller can drop the F_NOCACHE
/// requirement (the AsyncFileBackend works without it) or fall back to
/// the sync engine.
///
/// # Errors
/// `std::io::Error` from the underlying `fcntl(2)` call.
#[cfg(target_os = "macos")]
pub fn set_f_nocache(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    // `F_NOCACHE` (= 48) per `<sys/fcntl.h>`. Inlined rather than pulled
    // through `libc::F_NOCACHE` to keep this module's libc footprint to
    // the single `fcntl` symbol.
    const F_NOCACHE: libc::c_int = 48;
    // SAFETY: `fcntl` with `F_NOCACHE` takes an int argument; passing
    // a `c_int` of 1 turns the flag on. The fd is borrowed for the
    // duration of the syscall only — the caller holds the `&File`.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), F_NOCACHE, 1 as libc::c_int) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Stub for non-macOS hosts. squib runs only on Apple Silicon, but
/// `squib-virtio` cross-compiles for unit tests; this stub keeps the
/// trait surface available without conditional imports at the call site.
///
/// # Errors
/// Always succeeds (no-op).
#[cfg(not(target_os = "macos"))]
#[allow(clippy::disallowed_types)]
pub fn set_f_nocache(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    fn test_should_set_f_nocache_on_temp_file() {
        // Temp file lives on /tmp which is APFS — F_NOCACHE is honored.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nocache_smoke");
        std::fs::write(&path, b"hello").unwrap();
        let f = File::open(&path).unwrap();
        set_f_nocache(&f).expect("F_NOCACHE should succeed on APFS temp file");
    }
}
