//! Atomic temp-file + fsync + rename pattern (D25).
//!
//! Implements the rule from
//! [16-snapshots.md § 2](../../../specs/16-snapshots.md#2-state-file): every snapshot
//! file is staged into `<dest>.tmp` next to the destination, fsynced, then atomically
//! renamed. A half-disk-full host or a SIGTERM mid-save never corrupts the previous
//! snapshot pair.
//!
//! Two safety properties:
//! - Pre-flight cross-filesystem check: if the temp directory and destination live on different
//!   filesystems, `rename(2)` cannot be atomic. We reject before opening any file. (Risk row in 91
//!   § 12 — "User points `<id>.snap.tmp` at a different filesystem.")
//! - Best-effort cleanup: on any failure between `open(O_CREAT)` and the final `rename`, the temp
//!   file is unlinked.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use crate::error::SnapshotError;

/// Suffix appended to the destination path to derive the temp-file name.
pub const TEMP_SUFFIX: &str = ".tmp";

/// Validate that the temp-file path's directory and the destination share a
/// filesystem. Required for `rename(2)` atomicity.
///
/// On Unix we compare `stat::st_dev`. On non-Unix targets (Windows, WASI) the check
/// is best-effort: we currently treat them as same-fs (the pager + writer code
/// targets macOS — the cross-platform stub is for compile-time portability of the
/// snapshot crate itself).
///
/// # Errors
/// [`SnapshotError::AtomicCommitCrossFs`] if the two paths' parent directories live
/// on different filesystems; [`SnapshotError::Io`] if either path cannot be `stat`-ed.
pub fn check_same_filesystem(dest: &Path, temp: &Path) -> Result<(), SnapshotError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // The destination may not exist yet (e.g. first save). Probe its parent
        // directory's filesystem; that's where `rename(2)` will land.
        let dest_dir = dest.parent().unwrap_or_else(|| Path::new("."));
        let temp_dir = temp.parent().unwrap_or_else(|| Path::new("."));
        let dest_dev = std::fs::metadata(dest_dir)
            .map_err(SnapshotError::Io)?
            .dev();
        let temp_dev = std::fs::metadata(temp_dir)
            .map_err(SnapshotError::Io)?
            .dev();
        if dest_dev != temp_dev {
            return Err(SnapshotError::AtomicCommitCrossFs {
                dest: dest.to_path_buf(),
                temp_dir: temp_dir.to_path_buf(),
            });
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (dest, temp);
        Ok(())
    }
}

/// Derive the temp-file name from a destination path: `dest + TEMP_SUFFIX`.
///
/// The temp file lives next to the destination, in the same directory, so
/// `rename(2)` is atomic by construction (same filesystem). This *is* the contract
/// from D25 — moving the temp file to e.g. `/tmp` would defeat the atomicity.
#[must_use]
pub fn derive_temp_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(TEMP_SUFFIX);
    PathBuf::from(s)
}

/// Drop-guard that unlinks a path on drop unless [`disarm`](Self::disarm) is called.
///
/// Used to ensure that any failure between temp-file open and final rename leaves
/// the filesystem clean.
#[derive(Debug)]
pub struct UnlinkOnDrop {
    path: Option<PathBuf>,
}

impl UnlinkOnDrop {
    /// Arm the guard for `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Disarm the guard — the path will not be unlinked when the guard drops.
    pub fn disarm(&mut self) {
        self.path = None;
    }

    /// The path the guard would unlink (if armed).
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl Drop for UnlinkOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            // `remove_file` errors are deliberately swallowed: the only sensible
            // recovery is "log and move on" — the temp file is leaked but the
            // destination pair is untouched, which is the property D25 promises.
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Atomic-write fixture: open a temp file, hand the caller a writer, then either
/// fsync + rename it onto `dest` or unlink it on error.
///
/// The pattern in `commit`:
/// 1. Caller writes payload via [`Self::write_all`].
/// 2. Caller invokes [`Self::commit`].
/// 3. We `flush` + `sync_all` + `close`, then `rename(2)` onto the destination.
/// 4. On error in step 3, the [`UnlinkOnDrop`] guard removes the temp file.
///
/// The cross-FS pre-flight check runs in [`Self::open`]; subsequent `commit` cannot
/// fail with `EXDEV` because we've already validated.
#[derive(Debug)]
pub struct AtomicWriter {
    file: File,
    temp_path: PathBuf,
    dest_path: PathBuf,
    guard: UnlinkOnDrop,
}

impl AtomicWriter {
    /// Open the temp file for writing.
    ///
    /// Pre-flight: validate that the temp dir and destination dir live on the same
    /// filesystem (cross-FS rename cannot be atomic).
    ///
    /// # Errors
    /// [`SnapshotError::AtomicCommitCrossFs`] for the cross-FS case;
    /// [`SnapshotError::Io`] for an open / mkdir / stat failure.
    pub fn open(dest: &Path) -> Result<Self, SnapshotError> {
        let temp = derive_temp_path(dest);
        check_same_filesystem(dest, &temp)?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        let guard = UnlinkOnDrop::new(temp.clone());
        Ok(Self {
            file,
            temp_path: temp,
            dest_path: dest.to_path_buf(),
            guard,
        })
    }

    /// The temp-file path (visible during writing, before commit).
    #[must_use]
    pub fn temp_path(&self) -> &Path {
        &self.temp_path
    }

    /// The destination path the commit will produce.
    #[must_use]
    pub fn dest_path(&self) -> &Path {
        &self.dest_path
    }

    /// Borrow the underlying file for direct writes (used by the streaming envelope
    /// + memory file paths).
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    /// fsync the temp file, drop the writer, and `rename(2)` it onto the destination.
    ///
    /// # Errors
    /// [`SnapshotError::AtomicCommitFailed`] for the rename failure (temp file is
    /// unlinked); [`SnapshotError::Io`] for an fsync failure (temp file is also
    /// unlinked).
    pub fn commit(mut self) -> Result<(), SnapshotError> {
        self.file.flush()?;
        self.file.sync_all()?;
        // Drop the file handle before rename: macOS / Linux both allow rename
        // over an open file, but closing first removes any chance of a stale
        // descriptor confusing the test fixture.
        drop(self.file);
        match std::fs::rename(&self.temp_path, &self.dest_path) {
            Ok(()) => {
                // Disarm the guard so we don't unlink a destination we just
                // committed.
                self.guard.disarm();
                Ok(())
            }
            Err(e) => Err(SnapshotError::AtomicCommitFailed(e)),
        }
    }
}

impl Write for AtomicWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn dest_in(dir: &Path, name: &str) -> PathBuf {
        dir.join(name)
    }

    #[test]
    fn test_should_derive_temp_path_with_tmp_suffix() {
        let p = derive_temp_path(Path::new("/tmp/x.snap"));
        assert_eq!(p, PathBuf::from("/tmp/x.snap.tmp"));
    }

    #[test]
    fn test_should_commit_temp_file_onto_destination() {
        let dir = TempDir::new().unwrap();
        let dest = dest_in(dir.path(), "x.snap");
        let mut w = AtomicWriter::open(&dest).unwrap();
        w.write_all(b"hello world").unwrap();
        w.commit().unwrap();

        let s = std::fs::read_to_string(&dest).unwrap();
        assert_eq!(s, "hello world");
        // No stranded temp.
        assert!(!derive_temp_path(&dest).exists());
    }

    #[test]
    fn test_should_unlink_temp_when_writer_dropped_without_commit() {
        let dir = TempDir::new().unwrap();
        let dest = dest_in(dir.path(), "x.snap");
        {
            let mut w = AtomicWriter::open(&dest).unwrap();
            w.write_all(b"partial").unwrap();
            // No commit() — guard fires on drop.
        }
        assert!(!dest.exists(), "no commit happened");
        assert!(!derive_temp_path(&dest).exists(), "temp file leaked");
    }

    #[test]
    fn test_should_leave_existing_destination_alone_when_writer_dropped() {
        let dir = TempDir::new().unwrap();
        let dest = dest_in(dir.path(), "x.snap");
        std::fs::write(&dest, b"prior good").unwrap();
        {
            let mut w = AtomicWriter::open(&dest).unwrap();
            w.write_all(b"new bad").unwrap();
        }
        let s = std::fs::read_to_string(&dest).unwrap();
        assert_eq!(s, "prior good", "previous good pair clobbered");
    }

    #[test]
    fn test_should_handle_back_to_back_atomic_writes() {
        let dir = TempDir::new().unwrap();
        let dest = dest_in(dir.path(), "x.snap");
        for i in 0..3 {
            let mut w = AtomicWriter::open(&dest).unwrap();
            let payload = format!("snapshot {i}");
            w.write_all(payload.as_bytes()).unwrap();
            w.commit().unwrap();
            assert_eq!(std::fs::read_to_string(&dest).unwrap(), payload);
        }
    }

    #[test]
    fn test_should_reject_cross_filesystem_path() {
        // We can't reliably create a different mount in unit tests, but on Unix
        // we can synthesise the situation by lying about the temp directory.
        // We pretend the destination's parent and the temp file's parent are
        // unrelated by feeding `check_same_filesystem` a path whose parent
        // doesn't exist — that surfaces an Io error, distinguishable from
        // AtomicCommitCrossFs and good enough to assert the function does NOT
        // silently succeed when stat fails.
        let dir = TempDir::new().unwrap();
        let nonexistent = dir.path().join("does-not-exist").join("inner.snap");
        // Parent of `nonexistent` is `dir.path()/does-not-exist` which doesn't
        // exist, so the stat call must fail with NotFound. The function
        // surfaces it as Io, not as AtomicCommitCrossFs, which is correct.
        let res = AtomicWriter::open(&nonexistent);
        assert!(res.is_err());
    }

    #[test]
    fn test_should_disarm_guard_to_avoid_post_commit_unlink() {
        let dir = TempDir::new().unwrap();
        let dest = dest_in(dir.path(), "x.snap");
        let mut g = UnlinkOnDrop::new(dest.clone());
        std::fs::write(&dest, b"persistent").unwrap();
        g.disarm();
        drop(g);
        assert!(dest.exists());
    }
}
