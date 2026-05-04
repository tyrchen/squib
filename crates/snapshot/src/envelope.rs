//! `Snapshot<Data>` envelope, header, and CRC64 framing.
//!
//! Layout per [10-data-model.md § 6.1](../../../specs/10-data-model.md#61-state-file-idsnap):
//!
//! ```text
//! | bitcode::serialize(Snapshot<MicrovmState>)   variable, no length prefix
//! | crc64                                        u64 LE, ECMA-182, over the bitcode bytes
//! ```
//!
//! The implementation mirrors `vendors/firecracker/src/vmm/src/snapshot/mod.rs` so a
//! `firecracker --describe-snapshot` against a squib-produced file succeeds
//! structurally (D5).

use std::io::{Read, Write};

use semver::Version;
use serde::{Serialize, de::DeserializeOwned};

use crate::error::SnapshotError;

/// aarch64 magic id; matches upstream Firecracker
/// (`vendors/firecracker/src/vmm/src/snapshot/mod.rs:44`).
pub const SNAPSHOT_MAGIC_AARCH64: u64 = 0x0710_1984_AAAA_0000;

/// Snapshot version this build emits and accepts.
///
/// Pinned to upstream Firecracker `SNAPSHOT_VERSION` (D15). Bumped in lockstep with
/// upstream minor releases. Major bump invalidates older state files.
pub const SNAPSHOT_VERSION: Version = Version::new(5, 0, 0);

/// 10 MiB upper bound on serialized state payload, matching upstream's DoS guard
/// (`SNAPSHOT_DESERIALIZATION_BYTES_LIMIT`). Real microVMs sit well below 1 MiB; the
/// limit only ever fires on a malicious or corrupt file.
pub const SNAPSHOT_DESERIALIZATION_BYTES_LIMIT: usize = 10_000_000;

/// Architecture-specific magic for this build.
#[must_use]
pub const fn arch_magic() -> u64 {
    SNAPSHOT_MAGIC_AARCH64
}

/// On-disk header. Lives **inside** the bitcode envelope (D5) — *not* as a raw byte
/// prefix, which would break upstream Firecracker compatibility.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SnapshotHdr {
    /// Architecture-specific magic value.
    pub magic: u64,
    /// Snapshot data version.
    pub version: Version,
}

/// Outer envelope wrapping a state blob.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot<Data> {
    /// Header (magic + version).
    pub header: SnapshotHdr,
    /// Wrapped state data.
    pub data: Data,
}

impl<Data> Snapshot<Data> {
    /// Construct a fresh envelope around `data`, stamped with this build's magic +
    /// version.
    #[must_use]
    pub fn new(data: Data) -> Self {
        Self {
            header: SnapshotHdr {
                magic: arch_magic(),
                version: SNAPSHOT_VERSION,
            },
            data,
        }
    }

    /// Borrow the embedded version.
    #[must_use]
    pub fn version(&self) -> &Version {
        &self.header.version
    }
}

impl<Data: Serialize> Snapshot<Data> {
    /// Save `self` into `writer` as `bitcode(envelope) || crc64-le`.
    ///
    /// The CRC is computed over the bitcode bytes only (the trailing 8 bytes are
    /// not folded into the checksum) — same shape as upstream Firecracker so a
    /// `firecracker --describe-snapshot` succeeds.
    ///
    /// # Errors
    /// [`SnapshotError::Bitcode`] for an encoding failure; [`SnapshotError::Io`]
    /// for a writer-side I/O failure.
    pub fn save<W: Write>(&self, writer: &mut W) -> Result<(), SnapshotError> {
        let mut crc_writer = Crc64Writer::new(writer);
        let encoded =
            bitcode::serialize(self).map_err(|e| SnapshotError::Bitcode(e.to_string()))?;
        crc_writer.write_all(&encoded)?;
        let crc = crc_writer.checksum();
        crc_writer.into_inner().write_all(&crc.to_le_bytes())?;
        Ok(())
    }
}

impl<Data: DeserializeOwned> Snapshot<Data> {
    /// Load and validate the envelope from a `Read`.
    ///
    /// Validates, in order:
    /// 1. Size is below the DoS-guard limit.
    /// 2. Trailing 8-byte CRC matches the body.
    /// 3. Magic equals the architecture-specific value.
    /// 4. Major version matches; minor version ≤ ours.
    ///
    /// The "load_without_crc_check" variant is exposed for `--describe-snapshot`
    /// against a possibly-corrupt file: an operator wants to see the version *even
    /// if* the CRC has been clobbered, so the describe path can downgrade CRC
    /// errors to warnings.
    ///
    /// # Errors
    /// [`SnapshotError::SizeLimitExceeded`], [`SnapshotError::TooShort`],
    /// [`SnapshotError::CrcMismatch`], [`SnapshotError::MagicMismatch`],
    /// [`SnapshotError::VersionMismatch`], [`SnapshotError::Bitcode`],
    /// [`SnapshotError::Io`].
    pub fn load<R: Read>(reader: &mut R) -> Result<Self, SnapshotError> {
        let buf = read_with_limit(reader, SNAPSHOT_DESERIALIZATION_BYTES_LIMIT)?;
        Self::load_from_slice(&buf)
    }

    /// Load + validate from an in-memory slice. The slice must include the trailing
    /// 8-byte CRC.
    ///
    /// # Errors
    /// Same as [`Self::load`].
    pub fn load_from_slice(buf: &[u8]) -> Result<Self, SnapshotError> {
        if buf.len() > SNAPSHOT_DESERIALIZATION_BYTES_LIMIT {
            return Err(SnapshotError::SizeLimitExceeded {
                limit: SNAPSHOT_DESERIALIZATION_BYTES_LIMIT,
            });
        }
        if buf.len() < 8 {
            return Err(SnapshotError::TooShort);
        }
        // Upstream's CRC trick: `crc64(0, full_buf) == 0` iff the trailing 8 bytes
        // are the LE checksum of the preceding bytes. We verify CRC first because
        // a magic mismatch is meaningless on corrupted bytes.
        if crc64::crc64(0, buf) != 0 {
            return Err(SnapshotError::CrcMismatch);
        }
        let (data_buf, _crc_buf) = buf.split_at(buf.len() - 8);
        Self::load_without_crc_check(data_buf)
    }

    /// Decode the envelope without checking the CRC.
    ///
    /// Used by `--describe-snapshot` to emit a useful diagnostic on a CRC-clobbered
    /// file (the version + magic are still meaningful). Production load paths must
    /// go through [`Self::load`] / [`Self::load_from_slice`] which validate the CRC
    /// first.
    ///
    /// # Errors
    /// [`SnapshotError::SizeLimitExceeded`], [`SnapshotError::Bitcode`],
    /// [`SnapshotError::MagicMismatch`], [`SnapshotError::VersionMismatch`].
    pub fn load_without_crc_check(data_buf: &[u8]) -> Result<Self, SnapshotError> {
        if data_buf.len() > SNAPSHOT_DESERIALIZATION_BYTES_LIMIT {
            return Err(SnapshotError::SizeLimitExceeded {
                limit: SNAPSHOT_DESERIALIZATION_BYTES_LIMIT,
            });
        }
        let snapshot: Self =
            bitcode::deserialize(data_buf).map_err(|e| SnapshotError::Bitcode(e.to_string()))?;
        if snapshot.header.magic != arch_magic() {
            return Err(SnapshotError::MagicMismatch {
                found: snapshot.header.magic,
                expected: arch_magic(),
            });
        }
        if snapshot.header.version.major != SNAPSHOT_VERSION.major
            || snapshot.header.version.minor > SNAPSHOT_VERSION.minor
        {
            return Err(SnapshotError::VersionMismatch {
                found: snapshot.header.version.clone(),
                expected: SNAPSHOT_VERSION,
            });
        }
        Ok(snapshot)
    }
}

/// Read up to `limit` bytes from `reader`. Returns
/// [`SnapshotError::SizeLimitExceeded`] when the input exceeds the limit.
///
/// We read `limit + 1` bytes so a file exactly at the limit succeeds and a file one
/// byte over surfaces the dedicated error rather than `bitcode`'s opaque truncation.
fn read_with_limit<R: Read>(reader: &mut R, limit: usize) -> Result<Vec<u8>, SnapshotError> {
    let mut buf = Vec::new();
    let read_cap = u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX);
    let bytes = reader.take(read_cap).read_to_end(&mut buf)?;
    if bytes > limit {
        return Err(SnapshotError::SizeLimitExceeded { limit });
    }
    Ok(buf)
}

/// Streaming CRC-64 (ECMA-182) computer that wraps a writer.
///
/// Same shape as upstream's `CRC64Writer` (`vendors/firecracker/src/vmm/src/snapshot/crc.rs`).
#[derive(Debug)]
pub struct Crc64Writer<W> {
    writer: W,
    crc: u64,
}

impl<W: Write> Crc64Writer<W> {
    /// Construct a new wrapper over `writer`.
    pub fn new(writer: W) -> Self {
        Self { writer, crc: 0 }
    }

    /// The current CRC64 over all bytes written so far.
    #[must_use]
    pub fn checksum(&self) -> u64 {
        self.crc
    }

    /// Drop the wrapper, returning the underlying writer. Used to write the CRC
    /// trailer **without** including it in the checksum.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: Write> Write for Crc64Writer<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.writer.write(buf)?;
        self.crc = crc64::crc64(self.crc, &buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::state::MicrovmState;

    #[test]
    fn test_should_round_trip_default_state_through_save_and_load() {
        let snapshot = Snapshot::new(MicrovmState::default());
        let mut buf = Vec::new();
        snapshot.save(&mut buf).unwrap();
        let back = Snapshot::<MicrovmState>::load(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(snapshot.header, back.header);
    }

    #[test]
    fn test_should_reject_truncated_crc_trailer() {
        let snapshot = Snapshot::new(MicrovmState::default());
        let mut buf = Vec::new();
        snapshot.save(&mut buf).unwrap();
        let truncated = &buf[..buf.len() - 4];
        assert!(matches!(
            Snapshot::<MicrovmState>::load_from_slice(truncated),
            Err(SnapshotError::CrcMismatch)
        ));
    }

    #[test]
    fn test_should_reject_too_short_buffer() {
        assert!(matches!(
            Snapshot::<MicrovmState>::load_from_slice(&[]),
            Err(SnapshotError::TooShort)
        ));
        assert!(matches!(
            Snapshot::<MicrovmState>::load_from_slice(&[0u8; 4]),
            Err(SnapshotError::TooShort)
        ));
    }

    #[test]
    fn test_should_reject_bit_flipped_state() {
        let snapshot = Snapshot::new(MicrovmState::default());
        let mut buf = Vec::new();
        snapshot.save(&mut buf).unwrap();
        // Flip a body bit (not a CRC trailer bit).
        buf[2] ^= 0x01;
        assert!(matches!(
            Snapshot::<MicrovmState>::load_from_slice(&buf),
            Err(SnapshotError::CrcMismatch)
        ));
    }

    #[test]
    fn test_should_reject_clobbered_crc_trailer() {
        let snapshot = Snapshot::new(MicrovmState::default());
        let mut buf = Vec::new();
        snapshot.save(&mut buf).unwrap();
        let len = buf.len();
        for byte in &mut buf[len - 8..] {
            *byte ^= 0xFF;
        }
        assert!(matches!(
            Snapshot::<MicrovmState>::load_from_slice(&buf),
            Err(SnapshotError::CrcMismatch)
        ));
    }

    #[test]
    fn test_should_reject_wrong_magic_via_load_without_crc_check() {
        let mut snapshot = Snapshot::new(MicrovmState::default());
        snapshot.header.magic = 0xDEAD_BEEF;
        let body = bitcode::serialize(&snapshot).unwrap();
        assert!(matches!(
            Snapshot::<MicrovmState>::load_without_crc_check(&body),
            Err(SnapshotError::MagicMismatch { .. })
        ));
    }

    #[test]
    fn test_should_reject_higher_major_version() {
        let mut snapshot = Snapshot::new(MicrovmState::default());
        snapshot.header.version =
            Version::new(SNAPSHOT_VERSION.major + 1, SNAPSHOT_VERSION.minor, 0);
        let body = bitcode::serialize(&snapshot).unwrap();
        assert!(matches!(
            Snapshot::<MicrovmState>::load_without_crc_check(&body),
            Err(SnapshotError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn test_should_reject_higher_minor_version() {
        let mut snapshot = Snapshot::new(MicrovmState::default());
        snapshot.header.version =
            Version::new(SNAPSHOT_VERSION.major, SNAPSHOT_VERSION.minor + 1, 0);
        let body = bitcode::serialize(&snapshot).unwrap();
        assert!(matches!(
            Snapshot::<MicrovmState>::load_without_crc_check(&body),
            Err(SnapshotError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn test_should_accept_lower_minor_version() {
        // A state file from the previous minor must still load on this build.
        if SNAPSHOT_VERSION.minor == 0 {
            return; // can't test "lower minor" if we're on .0.x
        }
        let mut snapshot = Snapshot::new(MicrovmState::default());
        snapshot.header.version =
            Version::new(SNAPSHOT_VERSION.major, SNAPSHOT_VERSION.minor - 1, 0);
        let body = bitcode::serialize(&snapshot).unwrap();
        let _ok = Snapshot::<MicrovmState>::load_without_crc_check(&body).unwrap();
    }

    #[test]
    fn test_should_accept_arbitrary_patch_version() {
        let mut snapshot = Snapshot::new(MicrovmState::default());
        snapshot.header.version = Version::new(
            SNAPSHOT_VERSION.major,
            SNAPSHOT_VERSION.minor,
            SNAPSHOT_VERSION.patch + 12345,
        );
        let body = bitcode::serialize(&snapshot).unwrap();
        let _ok = Snapshot::<MicrovmState>::load_without_crc_check(&body).unwrap();
    }

    #[test]
    fn test_should_enforce_size_limit_on_load() {
        let huge = vec![0u8; SNAPSHOT_DESERIALIZATION_BYTES_LIMIT + 32];
        assert!(matches!(
            Snapshot::<MicrovmState>::load_from_slice(&huge),
            Err(SnapshotError::SizeLimitExceeded { .. })
        ));
    }

    #[test]
    fn test_should_keep_arch_magic_aarch64_constant() {
        assert_eq!(arch_magic(), 0x0710_1984_AAAA_0000);
    }
}
