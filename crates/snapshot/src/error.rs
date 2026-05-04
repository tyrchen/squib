//! `SnapshotError` — wire-stable error variants for the snapshot subsystem.
//!
//! Per [11-runtime-core.md § 6](../../../specs/11-runtime-core.md#6-error-types) the
//! variants map 1:1 to the `fault_message` strings the API server emits on
//! `PUT /snapshot/create` / `PUT /snapshot/load` failures. Renaming a variant is a
//! compat-suite golden change (I-RC-8 in 11 § 7), so the variants and their `Display`
//! shapes are the public contract.
//!
//! The variant set in this enum is **richer** than the spec's exemplar — it also
//! carries the per-step failure modes the implementation needs (`InvalidPath`,
//! `MemoryWrite`, `Bitcode`). Each is mapped through to a single wire-shape category
//! by [`SnapshotError::wire_message`] so the public surface stays stable.

use std::path::PathBuf;

use semver::Version;
use thiserror::Error;

/// Errors produced by the snapshot subsystem.
///
/// Variants and their `Display` shapes are wire-stable per I-RC-8: the API layer
/// surfaces `to_string()` verbatim into the `fault_message` body. Renaming a variant
/// or its message is a compat-suite golden change.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SnapshotError {
    /// A vCPU did not acknowledge the quiesce request within the timeout.
    ///
    /// Surfaces as `503 Service Unavailable` on `PUT /snapshot/create`. The save
    /// is aborted; the previous on-disk pair (if any) is untouched.
    #[error("a vCPU did not ack quiesce within the timeout")]
    QuiesceTimeout,

    /// Snapshot magic mismatch — the file is not a squib-/Firecracker-compatible
    /// state file.
    #[error("snapshot magic mismatch (file: {found:#x}, expected: {expected:#x})")]
    MagicMismatch {
        /// Magic value read from the file header.
        found: u64,
        /// Magic value squib expected (architecture-specific).
        expected: u64,
    },

    /// Snapshot version is not loadable by this squib build.
    ///
    /// The compat rule mirrors upstream: `major` must match exactly; `minor` must
    /// be ≤ ours; `patch` is unrestricted.
    #[error("snapshot version {found} is incompatible with squib's {expected}")]
    VersionMismatch {
        /// Version embedded in the file.
        found: Version,
        /// Version this squib build emits.
        expected: Version,
    },

    /// CRC64 of the file body does not match its trailing checksum.
    #[error("snapshot CRC64 mismatch")]
    CrcMismatch,

    /// The file is shorter than the trailing 8-byte CRC.
    #[error("snapshot file is too short to contain a CRC trailer")]
    TooShort,

    /// Snapshot deserializes to a structurally compatible state, but the contents
    /// (sysreg subset, GIC blob shape) are from a different VMM.
    #[error("snapshot is from a different VMM (sysreg or GIC blob shape mismatch)")]
    Incompatible,

    /// Atomic-commit failed: the temp file wrote successfully but `rename(2)` did
    /// not complete. The previous destination pair (if any) is left untouched.
    #[error("atomic commit (rename) failed: {0}")]
    AtomicCommitFailed(#[source] std::io::Error),

    /// The user-supplied destination path and the temp-file directory live on
    /// different filesystems, so `rename(2)` could not be atomic.
    ///
    /// Pre-flight check; surfaces *before* any data is written. The remediation is
    /// for the operator to point the snapshot at a path on the same filesystem as
    /// the temp directory (or vice-versa).
    #[error(
        "snapshot temp-file path is on a different filesystem from the destination \
         (dest={dest:?}, temp_dir={temp_dir:?}); rename(2) cannot be atomic across mounts"
    )]
    AtomicCommitCrossFs {
        /// User-supplied destination path.
        dest: PathBuf,
        /// Directory in which the temp file would have been created.
        temp_dir: PathBuf,
    },

    /// Operator handed the API a path that did not pass boundary validation
    /// (NUL byte, oversized, traversal).
    #[error("invalid snapshot path: {0}")]
    InvalidPath(String),

    /// `bitcode` failed to encode or decode the snapshot envelope.
    ///
    /// `Display` matches [`Self::wire_message`] so the API server's
    /// `fault_message` body is byte-equal to the rendered `to_string()`.
    #[error("snapshot encoding error: {0}")]
    Bitcode(String),

    /// The file is larger than the squib deserialization size limit.
    #[error("snapshot exceeds {limit} byte deserialization limit")]
    SizeLimitExceeded {
        /// The configured limit.
        limit: usize,
    },

    /// Memory file write failed (sparse pwrite, full dump, or fsync).
    #[error("memory file I/O error: {0}")]
    MemoryIo(#[source] std::io::Error),

    /// Generic I/O error during state file read/write or fsync.
    #[error("snapshot I/O error: {0}")]
    Io(#[source] std::io::Error),
}

impl SnapshotError {
    /// The exact `fault_message` body string this error surfaces to the API.
    ///
    /// Stable per I-RC-8 — renaming a variant or its `Display` shape is a
    /// compat-suite golden change. Single-source-of-truth: this delegates to
    /// `Display`, so the two cannot drift.
    #[must_use]
    pub fn wire_message(&self) -> String {
        self.to_string()
    }
}

impl From<bitcode::Error> for SnapshotError {
    fn from(err: bitcode::Error) -> Self {
        Self::Bitcode(err.to_string())
    }
}

impl From<std::io::Error> for SnapshotError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Result alias used throughout `squib-snapshot`.
pub type Result<T, E = SnapshotError> = core::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_render_quiesce_timeout_with_stable_message() {
        let err = SnapshotError::QuiesceTimeout;
        assert_eq!(err.to_string(), err.wire_message());
        assert_eq!(
            err.wire_message(),
            "a vCPU did not ack quiesce within the timeout"
        );
    }

    #[test]
    fn test_should_format_magic_mismatch_with_hex() {
        let err = SnapshotError::MagicMismatch {
            found: 0xDEAD_BEEF,
            expected: 0x0710_1984_AAAA_0000,
        };
        let s = err.wire_message();
        assert!(s.contains("0xdeadbeef"), "msg = {s}");
        assert!(s.contains("0x7101984aaaa0000"), "msg = {s}");
    }

    #[test]
    fn test_should_classify_io_errors_under_io_variant() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no");
        let err: SnapshotError = io.into();
        assert!(matches!(err, SnapshotError::Io(_)));
    }

    #[test]
    fn test_should_route_atomic_commit_failure_into_dedicated_variant() {
        let io = std::io::Error::other("rename failed");
        let err = SnapshotError::AtomicCommitFailed(io);
        assert!(
            err.wire_message()
                .starts_with("atomic commit (rename) failed:")
        );
    }

    #[test]
    fn test_should_describe_cross_fs_with_paths() {
        let err = SnapshotError::AtomicCommitCrossFs {
            dest: PathBuf::from("/dest/x.snap"),
            temp_dir: PathBuf::from("/other-fs"),
        };
        let s = err.wire_message();
        assert!(s.contains("/dest/x.snap"));
        assert!(s.contains("/other-fs"));
    }
}
