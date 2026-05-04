//! Memory file writer / reader — Full dump and sparse-of-dirty.
//!
//! Per [16-snapshots.md § 3](../../../specs/16-snapshots.md#3-memory-file) the memory
//! file `<id>.mem` carries one of two shapes:
//!
//! - **Full** — `pwrite` every page in `[ram_start, ram_end)` to a dense file.
//! - **Sparse-of-dirty** — `pwrite` only dirty pages at their page-aligned offsets; the filesystem
//!   stores natural holes for unmodified pages (`SEEK_HOLE` reads them back as zero).
//!
//! Both share the [`MemoryWriter`] API: it wraps a [`crate::atomic::AtomicWriter`]
//! so the staged file is atomically renamed onto the destination on commit.

use std::io::{Seek, SeekFrom, Write};

use crate::{atomic::AtomicWriter, dirty::DirtyBitmap, error::SnapshotError};

/// Snapshot type — the on-disk shape that determines whether the memory file is
/// dense or sparse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySnapshotKind {
    /// Dump every byte of the tracked range to the file.
    Full,
    /// Write only dirty pages, leaving filesystem holes for the rest.
    Diff,
}

/// Reader trait the memory writer pulls page bytes from.
///
/// The trait is intentionally minimal so unit tests can stand in an in-memory
/// `Vec<u8>` and the production path can wrap an `applevisor::Memory` view.
pub trait PageReader {
    /// Fill `buf` from the source at `offset_from_ram_start`.
    ///
    /// `offset_from_ram_start = 0` corresponds to `ram_start`.
    ///
    /// # Errors
    /// Anything the underlying source surfaces; the writer wraps it into
    /// [`SnapshotError::MemoryIo`].
    fn read_at(&self, offset_from_ram_start: u64, buf: &mut [u8]) -> std::io::Result<()>;
}

/// In-memory `Vec<u8>` page reader for tests.
#[derive(Debug)]
pub struct VecPageReader {
    bytes: Vec<u8>,
}

impl VecPageReader {
    /// Build a reader from `Vec<u8>` (typically `ram_size` long, zero-filled with
    /// markers planted at known offsets).
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Mutable byte access for tests planting payload at specific offsets.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

impl PageReader for VecPageReader {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "offset > usize::MAX")
        })?;
        let end = start.checked_add(buf.len()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "offset overflow")
        })?;
        if end > self.bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "read past end of memory",
            ));
        }
        buf.copy_from_slice(&self.bytes[start..end]);
        Ok(())
    }
}

/// Memory file writer — wraps an [`AtomicWriter`] so the staged file is renamed onto
/// the destination on `commit`.
#[derive(Debug)]
pub struct MemoryWriter {
    inner: AtomicWriter,
    ram_size: u64,
    page_size: u64,
}

impl MemoryWriter {
    /// Open a memory writer for the destination path.
    ///
    /// `ram_size` is the logical size; for Full snapshots the file ends up exactly
    /// this large, for Diff it ends up sparse with the same logical extent.
    /// `page_size` is the **memory-file** page size — typically the host page
    /// (16 KiB on Apple Silicon) so the file aligns with `pwrite` granularity.
    ///
    /// # Errors
    /// As [`AtomicWriter::open`].
    pub fn open(
        dest: &std::path::Path,
        ram_size: u64,
        page_size: u64,
    ) -> Result<Self, SnapshotError> {
        Ok(Self {
            inner: AtomicWriter::open(dest)?,
            ram_size,
            page_size,
        })
    }

    /// The on-disk extent the file represents.
    #[must_use]
    pub fn ram_size(&self) -> u64 {
        self.ram_size
    }

    /// The memory-file page size in bytes.
    #[must_use]
    pub fn page_size(&self) -> u64 {
        self.page_size
    }

    /// Write a Full dump: every byte of `[0, ram_size)` from `reader` is written.
    ///
    /// We chunk in `page_size` units so the writer never holds the whole memory in
    /// a buffer; this matches the production path where `applevisor::Memory::read`
    /// is the upstream of `read_at`.
    ///
    /// # Errors
    /// [`SnapshotError::MemoryIo`] for any read or write failure.
    pub fn write_full<R: PageReader>(&mut self, reader: &R) -> Result<(), SnapshotError> {
        let mut buf = vec![
            0u8;
            usize::try_from(self.page_size).map_err(|_| {
                SnapshotError::MemoryIo(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "page_size > usize::MAX",
                ))
            })?
        ];
        let mut offset = 0u64;
        while offset < self.ram_size {
            let chunk = (self.ram_size - offset).min(self.page_size);
            let chunk_usize = usize::try_from(chunk).map_err(|_| {
                SnapshotError::MemoryIo(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "chunk > usize::MAX",
                ))
            })?;
            let buf_slice = &mut buf[..chunk_usize];
            reader
                .read_at(offset, buf_slice)
                .map_err(SnapshotError::MemoryIo)?;
            self.inner
                .file_mut()
                .write_all(buf_slice)
                .map_err(SnapshotError::MemoryIo)?;
            offset += chunk;
        }
        Ok(())
    }

    /// Write a Diff dump: only the pages in `dirty` are pwritten at their offsets.
    ///
    /// The file's logical extent ends up at `ram_size`. Filesystem-level holes will
    /// cover the unmodified pages on APFS, HFS+, and exFAT (per § 7 — "sparse file
    /// cross-FS"); on NFS we still produce the right *bytes*, just dense.
    ///
    /// `dirty` must cover the same `[ram_start, ram_size)` range as the writer, but
    /// may have a different `page_size`: we re-chunk the writes accordingly so the
    /// memory file stays at `self.page_size` granularity even when the bitmap was
    /// rebuilt under the adaptive heuristic.
    ///
    /// # Errors
    /// [`SnapshotError::MemoryIo`] for any read or write failure.
    pub fn write_diff<R: PageReader>(
        &mut self,
        reader: &R,
        dirty: &DirtyBitmap,
    ) -> Result<u64, SnapshotError> {
        if dirty.ram_size() != self.ram_size {
            return Err(SnapshotError::InvalidPath(format!(
                "diff bitmap covers {} bytes, memory file expects {}",
                dirty.ram_size(),
                self.ram_size
            )));
        }
        let bitmap_page = dirty.page_size();
        let target_page = self.page_size;
        if bitmap_page < target_page || !bitmap_page.is_multiple_of(target_page) {
            return Err(SnapshotError::InvalidPath(format!(
                "diff bitmap page ({bitmap_page}) must be a multiple of memory-file page \
                 ({target_page})",
            )));
        }
        // Pre-extend the file to the logical extent so writes past the high
        // water mark stay inside the file. Filesystem-level holes on APFS / HFS+
        // / exFAT cover unmodified pages.
        self.inner
            .file_mut()
            .set_len(self.ram_size)
            .map_err(SnapshotError::MemoryIo)?;
        let pages_per_block = bitmap_page / target_page;
        let buf_len = usize::try_from(target_page).map_err(|_| {
            SnapshotError::MemoryIo(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "page_size > usize::MAX",
            ))
        })?;
        let mut buf = vec![0u8; buf_len];
        let mut pages_written: u64 = 0;
        // Peek (don't drain) so the caller can retry on commit failure; the
        // canonical drain-after-commit pattern lives in `save::commit_diff`.
        for bitmap_page_idx in 0..dirty.page_count() {
            if !dirty.is_dirty_by_index(bitmap_page_idx) {
                continue;
            }
            let block_byte_offset = bitmap_page_idx * bitmap_page;
            for sub in 0..pages_per_block {
                let target_offset = block_byte_offset + sub * target_page;
                if target_offset >= self.ram_size {
                    break;
                }
                let chunk = (self.ram_size - target_offset).min(target_page);
                let chunk_usize = usize::try_from(chunk).map_err(|_| {
                    SnapshotError::MemoryIo(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "chunk > usize::MAX",
                    ))
                })?;
                let slice = &mut buf[..chunk_usize];
                reader
                    .read_at(target_offset, slice)
                    .map_err(SnapshotError::MemoryIo)?;
                self.inner
                    .file_mut()
                    .seek(SeekFrom::Start(target_offset))
                    .map_err(SnapshotError::MemoryIo)?;
                self.inner
                    .file_mut()
                    .write_all(slice)
                    .map_err(SnapshotError::MemoryIo)?;
                pages_written += 1;
            }
        }
        Ok(pages_written)
    }

    /// fsync + rename the memory file onto the destination.
    ///
    /// # Errors
    /// As [`AtomicWriter::commit`].
    pub fn commit(self) -> Result<(), SnapshotError> {
        self.inner.commit()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    fn build_reader(size: usize) -> VecPageReader {
        let mut r = VecPageReader::new(vec![0u8; size]);
        // Plant a marker every 4 KiB so we can verify offsets.
        let bytes = r.bytes_mut();
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = (i % 256) as u8;
        }
        r
    }

    fn dest_in(dir: &Path, name: &str) -> std::path::PathBuf {
        dir.join(name)
    }

    #[test]
    fn test_should_write_full_dump_dense() {
        let dir = TempDir::new().unwrap();
        let dest = dest_in(dir.path(), "x.mem");
        let ram_size = 64 * 1024;
        let reader = build_reader(ram_size);
        let mut w = MemoryWriter::open(&dest, ram_size as u64, 16 * 1024).unwrap();
        w.write_full(&reader).unwrap();
        w.commit().unwrap();
        let written = std::fs::read(&dest).unwrap();
        assert_eq!(written.len(), ram_size);
        assert_eq!(written, reader.bytes);
    }

    #[test]
    fn test_should_write_diff_only_dirty_pages() {
        let dir = TempDir::new().unwrap();
        let dest = dest_in(dir.path(), "x.mem");
        let ram_size: u64 = 64 * 1024;
        let page_size: u64 = 16 * 1024;
        let reader = build_reader(usize::try_from(ram_size).unwrap());
        let bm = DirtyBitmap::new(0, ram_size, page_size).unwrap();
        bm.set_dirty_by_index(1); // page 1 (16 KiB..32 KiB)
        bm.set_dirty_by_index(3); // page 3 (48 KiB..64 KiB)

        let mut w = MemoryWriter::open(&dest, ram_size, page_size).unwrap();
        let pages = w.write_diff(&reader, &bm).unwrap();
        w.commit().unwrap();
        assert_eq!(pages, 2);

        let written = std::fs::read(&dest).unwrap();
        assert_eq!(written.len() as u64, ram_size);
        // Pages 0 and 2 must be zeroed (we never wrote them); pages 1 and 3
        // must carry the original markers.
        assert!(written[0..page_size as usize].iter().all(|&b| b == 0));
        assert!(
            written[page_size as usize..(2 * page_size) as usize]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == ((i + page_size as usize) % 256) as u8),
            "diff did not preserve dirty page 1's markers",
        );
        assert!(
            written[(2 * page_size) as usize..(3 * page_size) as usize]
                .iter()
                .all(|&b| b == 0)
        );
    }

    #[test]
    fn test_should_unwrite_diff_when_bitmap_covers_finer_units() {
        // Bitmap at 2 MiB granularity but file at 16 KiB granularity.
        let dir = TempDir::new().unwrap();
        let dest = dest_in(dir.path(), "x.mem");
        let ram_size: u64 = 4 * 1024 * 1024;
        let target_page = 16 * 1024u64;
        let bitmap_page = 2 * 1024 * 1024u64;
        let reader = build_reader(usize::try_from(ram_size).unwrap());
        let bm = DirtyBitmap::new(0, ram_size, bitmap_page).unwrap();
        bm.set_dirty_by_index(1); // 2 MiB block 1 → 16 KiB pages 128..256

        let mut w = MemoryWriter::open(&dest, ram_size, target_page).unwrap();
        let pages = w.write_diff(&reader, &bm).unwrap();
        w.commit().unwrap();
        assert_eq!(pages, bitmap_page / target_page);

        let written = std::fs::read(&dest).unwrap();
        // First 2 MiB block must be zeroed (clean).
        assert!(written[..bitmap_page as usize].iter().all(|&b| b == 0));
        // Second 2 MiB block must carry the markers.
        let expected_byte = |i: usize| (i % 256) as u8;
        let block = bitmap_page as usize;
        for (i, byte) in written[block..block * 2].iter().enumerate() {
            assert_eq!(*byte, expected_byte(block + i));
        }
    }
}
