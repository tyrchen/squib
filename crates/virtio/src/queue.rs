//! Split-ring virtqueue handling.
//!
//! Implements the classic split-ring layout from
//! [virtio v1.2 § 2.7][spec]. The packed-ring (`VIRTIO_F_RING_PACKED`,
//! [`crate::feature_bits::RING_PACKED`]) is intentionally not implemented for
//! 1.0 — split-ring is what every Firecracker-compat guest driver uses, and
//! the simpler queue handler keeps the device modules small.
//!
//! ## Layout (per spec § 2.7)
//!
//! ```text
//! Descriptor table:    desc_table_addr,  16 * queue_size bytes
//! Available ring:      avail_ring_addr,  6 + 2 * queue_size bytes
//! Used ring:           used_ring_addr,   6 + 8 * queue_size bytes
//! ```
//!
//! Each descriptor:
//!
//! ```text
//! 0x00 u64  addr     guest-physical address of the buffer
//! 0x08 u32  len      buffer length in bytes
//! 0x0C u16  flags    (NEXT | WRITE | INDIRECT)
//! 0x0E u16  next     descriptor table index of the next descriptor
//! ```
//!
//! Available ring:
//!
//! ```text
//! 0x00 u16  flags
//! 0x02 u16  idx        producer (driver) cursor
//! 0x04 [u16; queue_size] ring   indices of descriptor heads
//! ```
//!
//! Used ring:
//!
//! ```text
//! 0x00 u16  flags
//! 0x02 u16  idx                        producer (device) cursor
//! 0x04 [(u32, u32); queue_size] ring   (head_index, total_bytes_written)
//! ```
//!
//! ## Concurrency
//!
//! The device handler thread "owns" the queue's `last_avail_idx` cursor; the
//! guest driver is the sole writer of `avail.idx`. The transport's queue
//! state lives behind the device's mutex, so race-free across threads.
//!
//! [spec]: https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-340007

use squib_core::{GuestAddress, GuestMemory};
use thiserror::Error;

/// Maximum allowed `queue_size`. Per spec § 2.7 it must be a power of two
/// ≤ 32768. Squib's per-device modules cap below this for memory pressure
/// reasons; the spec ceiling is exposed so a future device that wants to push
/// it can.
pub const MAX_QUEUE_SIZE: u16 = 32768;

/// Type-safe wrapper around a queue cursor index. Wraps modulo `queue_size`
/// at the call site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueIndex(pub u16);

/// Descriptor flag: another descriptor follows in the chain (`next` field is
/// valid).
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Descriptor flag: descriptor is device-write (the device fills it).
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/// Descriptor flag: this descriptor's buffer holds an indirect descriptor
/// table.
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// Per-queue setup state and runtime cursor.
///
/// The transport mutates `size`, `*_addr`, and `ready` in response to guest
/// MMIO writes during driver init. The device handler mutates
/// `next_avail_idx` as it consumes descriptor heads.
#[derive(Debug, Clone)]
pub struct Queue {
    /// Maximum descriptor count negotiated at boot (the device exposes this
    /// as `QueueNumMax`).
    pub max_size: u16,
    /// Active descriptor count. Bounded by `max_size`. Set by the driver
    /// writing `QueueNum`.
    pub size: u16,
    /// `true` once the driver writes `QueueReady = 1`.
    pub ready: bool,
    /// Guest-physical address of the descriptor table.
    pub desc_table_addr: GuestAddress,
    /// Guest-physical address of the available ring.
    pub avail_ring_addr: GuestAddress,
    /// Guest-physical address of the used ring.
    pub used_ring_addr: GuestAddress,
    /// Device-side cursor into the available ring.
    pub next_avail_idx: u16,
    /// Device-side cursor into the used ring (the next slot to write to).
    pub next_used_idx: u16,
}

impl Queue {
    /// Build a fresh queue with the given maximum descriptor count.
    #[must_use]
    pub fn new(max_size: u16) -> Self {
        Self {
            max_size,
            size: max_size,
            ready: false,
            desc_table_addr: GuestAddress(0),
            avail_ring_addr: GuestAddress(0),
            used_ring_addr: GuestAddress(0),
            next_avail_idx: 0,
            next_used_idx: 0,
        }
    }

    /// Set the negotiated descriptor count, clamping to `max_size` and to a
    /// power of two as the spec requires.
    pub fn set_size(&mut self, requested: u16) {
        // Spec § 2.7.4: the driver writes a power-of-two value ≤
        // QueueNumMax. We silently clamp invalid values rather than refusing
        // — Firecracker behaviour, observed by mainline virtio drivers.
        let clamped = requested.min(self.max_size);
        self.size = if clamped == 0 {
            1
        } else if clamped.is_power_of_two() {
            clamped
        } else {
            // Round down to the previous power of two; never zero. Use
            // `checked_next_power_of_two` to defend against a future bump of
            // `MAX_QUEUE_SIZE` past 32768 — overflow returns `None` here, in
            // which case we fall back to the largest representable power of
            // two `≤ u16::MAX` (`1 << 15`).
            clamped
                .checked_next_power_of_two()
                .map_or(1u16 << 15, |p| (p >> 1).max(1))
        };
    }

    /// `true` when every required address is set and `ready = 1`.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.ready
            && self.size > 0
            && self.size <= self.max_size
            && self.desc_table_addr.raw() != 0
            && self.avail_ring_addr.raw() != 0
            && self.used_ring_addr.raw() != 0
    }

    /// Reset the queue cursors and clear the ready flag. Called when the
    /// driver writes `Status = INIT`.
    pub fn reset(&mut self) {
        self.size = self.max_size;
        self.ready = false;
        self.desc_table_addr = GuestAddress(0);
        self.avail_ring_addr = GuestAddress(0);
        self.used_ring_addr = GuestAddress(0);
        self.next_avail_idx = 0;
        self.next_used_idx = 0;
    }

    /// Read `avail.idx` from guest memory.
    fn avail_idx<M: GuestMemory + ?Sized>(&self, mem: &M) -> Result<u16, QueueError> {
        let addr = GuestAddress(self.avail_ring_addr.raw() + 2);
        Ok(mem.read_u16_le(addr)?)
    }

    /// Read the descriptor head index at `avail.ring[i % size]`.
    fn avail_ring_entry<M: GuestMemory + ?Sized>(
        &self,
        mem: &M,
        i: u16,
    ) -> Result<u16, QueueError> {
        let off = u64::from(i % self.size) * 2;
        let addr = GuestAddress(self.avail_ring_addr.raw() + 4 + off);
        Ok(mem.read_u16_le(addr)?)
    }

    /// Pop the next descriptor head from the available ring.
    ///
    /// Returns `Ok(None)` if the driver has not made any new descriptors
    /// available since the last call.
    ///
    /// # Errors
    /// [`QueueError`] for any underlying memory access failure.
    pub fn pop_avail<M: GuestMemory + ?Sized>(
        &mut self,
        mem: &M,
    ) -> Result<Option<DescriptorChain>, QueueError> {
        if !self.is_valid() {
            return Err(QueueError::QueueNotReady);
        }
        let driver_idx = self.avail_idx(mem)?;
        if driver_idx == self.next_avail_idx {
            return Ok(None);
        }
        let head_idx = self.avail_ring_entry(mem, self.next_avail_idx)?;
        if head_idx >= self.size {
            return Err(QueueError::DescriptorOutOfRange {
                head: head_idx,
                size: self.size,
            });
        }
        self.next_avail_idx = self.next_avail_idx.wrapping_add(1);
        Ok(Some(DescriptorChain {
            desc_table_addr: self.desc_table_addr,
            queue_size: self.size,
            head_index: head_idx,
            current: head_idx,
            length: 0,
            done: false,
        }))
    }

    /// Push a completed descriptor chain into the used ring.
    ///
    /// `head_index` is the descriptor index returned by `pop_avail`;
    /// `bytes_written` is the total bytes the device wrote into device-write
    /// descriptors of the chain (zero for purely device-read chains).
    ///
    /// # Errors
    /// [`QueueError`] for any underlying memory access failure.
    pub fn push_used<M: GuestMemory + ?Sized>(
        &mut self,
        mem: &M,
        head_index: u16,
        bytes_written: u32,
    ) -> Result<(), QueueError> {
        if !self.is_valid() {
            return Err(QueueError::QueueNotReady);
        }
        let slot = self.next_used_idx % self.size;
        let elem_addr = GuestAddress(self.used_ring_addr.raw() + 4 + u64::from(slot) * 8);
        mem.write_u32_le(elem_addr, u32::from(head_index))?;
        mem.write_u32_le(GuestAddress(elem_addr.raw() + 4), bytes_written)?;
        self.next_used_idx = self.next_used_idx.wrapping_add(1);
        // Publish the new device cursor.
        let used_idx_addr = GuestAddress(self.used_ring_addr.raw() + 2);
        mem.write_u16_le(used_idx_addr, self.next_used_idx)?;
        Ok(())
    }
}

/// Iterator over the descriptors in a single chain (head + linked descriptors
/// via `NEXT`).
#[derive(Debug)]
pub struct DescriptorChain {
    desc_table_addr: GuestAddress,
    queue_size: u16,
    /// Index of the head descriptor; needed when pushing the chain into the
    /// used ring.
    head_index: u16,
    /// Index of the descriptor to be returned next (or the head if no
    /// descriptors have been read yet).
    current: u16,
    /// Number of descriptors yielded so far. Bounded by `queue_size` to
    /// detect cycles.
    length: u16,
    /// Set once a descriptor without `NEXT` has been yielded.
    done: bool,
}

/// One descriptor entry from a chain: a single guest buffer with its flags.
#[derive(Debug, Clone, Copy)]
pub struct Descriptor {
    /// Guest-physical address of the buffer.
    pub addr: GuestAddress,
    /// Buffer length in bytes.
    pub len: u32,
    /// `VIRTQ_DESC_F_*` flags.
    pub flags: u16,
    /// Next descriptor index (only valid if `flags & NEXT`).
    pub next: u16,
}

impl Descriptor {
    /// `true` if the device should treat this buffer as device-write (it
    /// fills the buffer) rather than device-read (it consumes the buffer).
    #[must_use]
    pub fn is_write_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }

    /// `true` if a successor descriptor exists.
    #[must_use]
    pub fn has_next(&self) -> bool {
        self.flags & VIRTQ_DESC_F_NEXT != 0
    }
}

impl DescriptorChain {
    /// Index of the head descriptor — pass to `Queue::push_used` when
    /// completing the chain.
    #[must_use]
    pub fn head_index(&self) -> u16 {
        self.head_index
    }

    /// Read the next descriptor in the chain, if any.
    ///
    /// Returns `Ok(None)` when the chain is fully traversed.
    ///
    /// # Errors
    /// - [`QueueError::DescriptorOutOfRange`] if `next` exceeds `queue_size`.
    /// - [`QueueError::ChainTooLong`] if the chain length exceeds `queue_size` (cycle protection).
    /// - [`QueueError::Memory`] for any descriptor table read failure.
    pub fn next_descriptor<M: GuestMemory + ?Sized>(
        &mut self,
        mem: &M,
    ) -> Result<Option<Descriptor>, QueueError> {
        if self.done {
            return Ok(None);
        }
        if self.current >= self.queue_size {
            return Err(QueueError::DescriptorOutOfRange {
                head: self.current,
                size: self.queue_size,
            });
        }
        if self.length >= self.queue_size {
            return Err(QueueError::ChainTooLong {
                size: self.queue_size,
            });
        }
        let off = u64::from(self.current) * 16;
        let base = GuestAddress(self.desc_table_addr.raw() + off);
        let addr = mem.read_u64_le(base)?;
        let len = mem.read_u32_le(GuestAddress(base.raw() + 8))?;
        let flags = mem.read_u16_le(GuestAddress(base.raw() + 12))?;
        let next = mem.read_u16_le(GuestAddress(base.raw() + 14))?;
        let desc = Descriptor {
            addr: GuestAddress(addr),
            len,
            flags,
            next,
        };
        self.length = self.length.wrapping_add(1);
        if desc.has_next() {
            self.current = next;
        } else {
            self.done = true;
        }
        Ok(Some(desc))
    }

    /// Drain the chain into a `Vec<Descriptor>`. Convenience for short
    /// chains where the caller wants to pre-collect.
    ///
    /// # Errors
    /// Same as [`Self::next_descriptor`].
    pub fn collect<M: GuestMemory + ?Sized>(
        mut self,
        mem: &M,
    ) -> Result<Vec<Descriptor>, QueueError> {
        let mut out = Vec::with_capacity(usize::from(self.queue_size).min(8));
        while let Some(d) = self.next_descriptor(mem)? {
            out.push(d);
        }
        Ok(out)
    }
}

/// Queue / descriptor / ring errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum QueueError {
    /// Driver attempted to use the queue before posting `QueueReady = 1`.
    #[error("queue not ready")]
    QueueNotReady,
    /// A descriptor index exceeded the negotiated `queue_size`.
    #[error("descriptor index {head} >= queue_size {size}")]
    DescriptorOutOfRange {
        /// The offending index.
        head: u16,
        /// Negotiated `queue_size`.
        size: u16,
    },
    /// A descriptor chain length exceeded `queue_size` (cycle protection).
    #[error("descriptor chain length exceeds queue_size {size} (cycle?)")]
    ChainTooLong {
        /// Negotiated `queue_size`.
        size: u16,
    },
    /// Guest memory access failed.
    #[error("queue memory error: {0}")]
    Memory(#[from] squib_core::Error),
}

#[cfg(test)]
mod tests {
    use squib_core::SliceGuestMemory;

    use super::*;

    fn mem() -> SliceGuestMemory {
        SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x1_0000)
    }

    fn setup_queue(mem: &SliceGuestMemory) -> Queue {
        let mut q = Queue::new(64);
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        // Wipe rings to deterministic state.
        let zeros = vec![0u8; 0x100];
        mem.write(q.desc_table_addr, &zeros).unwrap();
        mem.write(q.avail_ring_addr, &zeros).unwrap();
        mem.write(q.used_ring_addr, &zeros).unwrap();
        q
    }

    fn write_desc(
        mem: &SliceGuestMemory,
        q: &Queue,
        idx: u16,
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        let base = q.desc_table_addr.raw() + u64::from(idx) * 16;
        mem.write_u32_le(GuestAddress(base), addr as u32).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), (addr >> 32) as u32)
            .unwrap();
        mem.write_u32_le(GuestAddress(base + 8), len).unwrap();
        mem.write_u16_le(GuestAddress(base + 12), flags).unwrap();
        mem.write_u16_le(GuestAddress(base + 14), next).unwrap();
    }

    #[test]
    fn test_should_clamp_queue_size_to_power_of_two() {
        let mut q = Queue::new(256);
        q.set_size(200);
        assert_eq!(q.size, 128);
        q.set_size(256);
        assert_eq!(q.size, 256);
        q.set_size(1024); // > max
        assert_eq!(q.size, 256);
    }

    #[test]
    fn test_should_reject_pop_before_ready() {
        let m = mem();
        let mut q = Queue::new(8);
        let err = q.pop_avail(&m).unwrap_err();
        assert!(matches!(err, QueueError::QueueNotReady));
    }

    #[test]
    fn test_should_yield_no_descriptor_when_avail_idx_unchanged() {
        let m = mem();
        let mut q = setup_queue(&m);
        assert!(q.pop_avail(&m).unwrap().is_none());
    }

    #[test]
    fn test_should_walk_single_descriptor_chain() {
        let m = mem();
        let mut q = setup_queue(&m);
        write_desc(&m, &q, 0, 0x1234, 16, 0, 0);
        m.write_u16_le(GuestAddress(q.avail_ring_addr.raw() + 4), 0)
            .unwrap();
        m.write_u16_le(GuestAddress(q.avail_ring_addr.raw() + 2), 1)
            .unwrap();
        let chain = q.pop_avail(&m).unwrap().expect("chain available");
        let descs = chain.collect(&m).unwrap();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].addr.raw(), 0x1234);
        assert_eq!(descs[0].len, 16);
    }

    #[test]
    fn test_should_walk_two_descriptor_chain() {
        let m = mem();
        let mut q = setup_queue(&m);
        write_desc(&m, &q, 0, 0x1000, 16, VIRTQ_DESC_F_NEXT, 1);
        write_desc(&m, &q, 1, 0x2000, 32, VIRTQ_DESC_F_WRITE, 0);
        m.write_u16_le(GuestAddress(q.avail_ring_addr.raw() + 4), 0)
            .unwrap();
        m.write_u16_le(GuestAddress(q.avail_ring_addr.raw() + 2), 1)
            .unwrap();
        let chain = q.pop_avail(&m).unwrap().expect("chain available");
        let descs = chain.collect(&m).unwrap();
        assert_eq!(descs.len(), 2);
        assert_eq!(descs[0].addr.raw(), 0x1000);
        assert!(!descs[0].is_write_only());
        assert!(descs[0].has_next());
        assert_eq!(descs[1].addr.raw(), 0x2000);
        assert!(descs[1].is_write_only());
        assert!(!descs[1].has_next());
    }

    #[test]
    fn test_should_reject_descriptor_index_out_of_range() {
        let m = mem();
        let mut q = setup_queue(&m);
        // Head index 9 with queue_size 8 → out of range.
        m.write_u16_le(GuestAddress(q.avail_ring_addr.raw() + 4), 9)
            .unwrap();
        m.write_u16_le(GuestAddress(q.avail_ring_addr.raw() + 2), 1)
            .unwrap();
        let err = q.pop_avail(&m).unwrap_err();
        assert!(matches!(err, QueueError::DescriptorOutOfRange { .. }));
    }

    #[test]
    fn test_should_break_descriptor_chain_cycle_with_chain_too_long() {
        let m = mem();
        let mut q = setup_queue(&m);
        // 0 → 1 → 0 → 1 → ... cycle.
        write_desc(&m, &q, 0, 0x1000, 16, VIRTQ_DESC_F_NEXT, 1);
        write_desc(&m, &q, 1, 0x2000, 16, VIRTQ_DESC_F_NEXT, 0);
        m.write_u16_le(GuestAddress(q.avail_ring_addr.raw() + 4), 0)
            .unwrap();
        m.write_u16_le(GuestAddress(q.avail_ring_addr.raw() + 2), 1)
            .unwrap();
        let chain = q.pop_avail(&m).unwrap().expect("chain available");
        let err = chain.collect(&m).unwrap_err();
        assert!(matches!(err, QueueError::ChainTooLong { .. }));
    }

    #[test]
    fn test_should_publish_used_ring_with_head_index_and_byte_count() {
        let m = mem();
        let mut q = setup_queue(&m);
        q.push_used(&m, 5, 128).unwrap();
        // First used elem at used_ring + 4.
        let elem_addr = GuestAddress(q.used_ring_addr.raw() + 4);
        let head = m.read_u32_le(elem_addr).unwrap();
        let len = m.read_u32_le(GuestAddress(elem_addr.raw() + 4)).unwrap();
        assert_eq!(head, 5);
        assert_eq!(len, 128);
        // used.idx now == 1.
        let used_idx = m
            .read_u16_le(GuestAddress(q.used_ring_addr.raw() + 2))
            .unwrap();
        assert_eq!(used_idx, 1);
    }

    #[test]
    fn test_should_reset_queue_back_to_max_size_and_unready() {
        let mut q = Queue::new(8);
        q.size = 4;
        q.ready = true;
        q.next_avail_idx = 3;
        q.reset();
        assert_eq!(q.size, 8);
        assert!(!q.ready);
        assert_eq!(q.next_avail_idx, 0);
    }
}
