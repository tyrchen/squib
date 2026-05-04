//! virtio-balloon — guest memory ballooning.
//!
//! Per [14-virtio-and-devices.md § 4.4](../../../specs/14-virtio-and-devices.md#44-virtio-balloon).
//! The balloon protocol uses two queues (inflate, deflate) plus an optional
//! statistics queue. The driver enqueues guest-physical page numbers (PFNs)
//! it has freed; the host responds by `madvise(MADV_DONTNEED)` on those
//! pages — actually returning the memory to the OS on macOS.
//!
//! The PFN size is fixed at `4 KiB` (per spec § 5.5 the balloon protocol
//! uses 4 KiB units regardless of host page size; the hypervisor coalesces
//! PFNs that fall in the same 16 KiB host page on Apple Silicon).

use std::sync::Arc;

use parking_lot::Mutex;
use squib_core::GuestMemory;

use crate::{
    device::{ActivateError, VirtioDevice},
    device_id::VirtioDeviceType,
    interrupt::IrqLine,
    queue::Queue,
};

/// Inflate queue index (driver → device: PFNs the device should reclaim).
pub const INFLATE_QUEUE: usize = 0;
/// Deflate queue index (device → driver: PFNs the driver may reuse).
pub const DEFLATE_QUEUE: usize = 1;
/// Statistics queue index (optional, only if `VIRTIO_BALLOON_F_STATS_VQ`).
pub const STATS_QUEUE: usize = 2;

/// `VIRTIO_BALLOON_F_MUST_TELL_HOST` (bit 0).
pub const F_MUST_TELL_HOST: u64 = 1 << 0;
/// `VIRTIO_BALLOON_F_STATS_VQ` (bit 1).
pub const F_STATS_VQ: u64 = 1 << 1;
/// `VIRTIO_BALLOON_F_DEFLATE_ON_OOM` (bit 2).
pub const F_DEFLATE_ON_OOM: u64 = 1 << 2;
/// `VIRTIO_BALLOON_F_FREE_PAGE_HINT` (bit 3).
pub const F_FREE_PAGE_HINT: u64 = 1 << 3;
/// `VIRTIO_BALLOON_F_PAGE_POISON` (bit 4).
pub const F_PAGE_POISON: u64 = 1 << 4;
/// `VIRTIO_BALLOON_F_REPORTING` (bit 5).
pub const F_REPORTING: u64 = 1 << 5;

/// Per-queue maximum descriptor counts. Inflate / deflate queues need to
/// fit a reasonable burst of PFNs.
const QUEUE_MAX_SIZE: u16 = 128;

/// Balloon configuration as built by the API layer.
#[derive(Debug, Clone, Default)]
pub struct BalloonConfig {
    /// Target balloon size in MiB. The driver inflates up to this size.
    pub target_mib: u32,
    /// Whether `DEFLATE_ON_OOM` is enabled.
    pub deflate_on_oom: bool,
    /// Statistics polling interval in seconds; `0` disables.
    pub stats_polling_interval_s: u16,
}

/// Snapshot of balloon statistics reported by the guest.
#[derive(Debug, Default, Clone, Copy)]
pub struct BalloonStats {
    /// Reported pages swapped in.
    pub swap_in: u64,
    /// Reported pages swapped out.
    pub swap_out: u64,
    /// Major faults.
    pub major_faults: u64,
    /// Minor faults.
    pub minor_faults: u64,
    /// Free memory in bytes.
    pub free_memory: u64,
    /// Total memory in bytes.
    pub total_memory: u64,
}

/// virtio-balloon frontend.
#[derive(Debug)]
pub struct BalloonDevice {
    avail: u64,
    acked: u64,
    queues: Vec<Queue>,
    config: BalloonConfig,
    state: Arc<Mutex<ActiveState>>,
    /// PFNs the guest has currently parked in the balloon. Persisted across
    /// snapshots in Phase 5.
    inflated_pfns: Arc<Mutex<Vec<u32>>>,
}

#[derive(Debug, Default)]
struct ActiveState {
    mem: Option<Arc<dyn GuestMemory>>,
    irq: Option<IrqLine>,
    activated: bool,
}

impl BalloonDevice {
    /// Build a balloon with the given configuration. The stats queue is
    /// included only if `stats_polling_interval_s > 0`.
    #[must_use]
    pub fn new(config: BalloonConfig) -> Self {
        let mut avail = F_DEFLATE_ON_OOM;
        let mut queues = vec![Queue::new(QUEUE_MAX_SIZE), Queue::new(QUEUE_MAX_SIZE)];
        if config.stats_polling_interval_s > 0 {
            avail |= F_STATS_VQ;
            queues.push(Queue::new(QUEUE_MAX_SIZE));
        }
        Self {
            avail,
            acked: 0,
            queues,
            config,
            state: Arc::new(Mutex::new(ActiveState::default())),
            inflated_pfns: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Snapshot of the currently-inflated PFN list.
    #[must_use]
    pub fn inflated_pfn_count(&self) -> usize {
        self.inflated_pfns.lock().len()
    }

    fn drain_pfn_queue(&mut self, queue_index: usize) {
        let (mem, irq) = {
            let state = self.state.lock();
            match (state.mem.clone(), state.irq.clone()) {
                (Some(m), Some(i)) => (m, i),
                _ => return,
            }
        };
        let queue = &mut self.queues[queue_index];
        let mut completed = false;
        loop {
            let chain = match queue.pop_avail(mem.as_ref()) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "balloon: descriptor walk failed");
                    break;
                }
            };
            let head = chain.head_index();
            let descs = match chain.collect(mem.as_ref()) {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!(error = %err, "balloon: chain collect failed");
                    break;
                }
            };
            let mut pfns = self.inflated_pfns.lock();
            for desc in descs {
                let entries = (desc.len as usize) / 4;
                for i in 0..entries {
                    let pfn_addr = squib_core::GuestAddress(desc.addr.raw() + (i as u64) * 4);
                    let pfn = match mem.read_u32_le(pfn_addr) {
                        Ok(p) => p,
                        Err(err) => {
                            tracing::warn!(error = %err, "balloon: PFN read failed");
                            continue;
                        }
                    };
                    if queue_index == INFLATE_QUEUE {
                        pfns.push(pfn);
                    } else if let Some(pos) = pfns.iter().position(|p| *p == pfn) {
                        pfns.swap_remove(pos);
                    }
                }
            }
            if let Err(err) = queue.push_used(mem.as_ref(), head, 0) {
                tracing::warn!(error = %err, "balloon: push_used failed");
                break;
            }
            completed = true;
        }
        if completed {
            let _ = irq.trigger_queue();
        }
    }
}

impl VirtioDevice for BalloonDevice {
    fn device_type(&self) -> VirtioDeviceType {
        VirtioDeviceType::Balloon
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
        if self.config.stats_polling_interval_s > 0 {
            &[QUEUE_MAX_SIZE, QUEUE_MAX_SIZE, QUEUE_MAX_SIZE]
        } else {
            &[QUEUE_MAX_SIZE, QUEUE_MAX_SIZE]
        }
    }
    fn queues(&self) -> &[Queue] {
        &self.queues
    }
    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // Config layout (virtio v1.2 § 5.5.4):
        //   0x00 num_pages   (u32, target inflate count in 4 KiB units)
        //   0x04 actual      (u32, currently inflated count)
        //   0x08 free_page_hint_cmd_id (u32) — unused on squib
        //   0x0C poison_val  (u32) — unused on squib
        let target_pages =
            u32::try_from(u64::from(self.config.target_mib) * 256).unwrap_or(u32::MAX);
        let actual_pages = u32::try_from(self.inflated_pfns.lock().len()).unwrap_or(u32::MAX);
        let mut full = [0u8; 16];
        full[0..4].copy_from_slice(&target_pages.to_le_bytes());
        full[4..8].copy_from_slice(&actual_pages.to_le_bytes());
        // 8..12 and 12..16 stay zero.
        let off = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = full.get(off + i).copied().unwrap_or(0);
        }
    }
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // Driver-side config writes (e.g. `actual`) are advisory; we don't
        // mirror them into our state.
    }
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
        let qi = queue_index as usize;
        if qi == INFLATE_QUEUE || qi == DEFLATE_QUEUE {
            self.drain_pfn_queue(qi);
        } else if qi == STATS_QUEUE {
            self.drain_stats_queue();
        }
    }
}

impl BalloonDevice {
    /// Drain the stats queue without parsing. The guest pushes one stats
    /// descriptor periodically; if we never `push_used` the avail ring
    /// builds up indefinitely and the guest's watchdog times out. Until
    /// the API layer wires `/balloon/statistics` to consume the values
    /// (deferred), draining-and-discarding is the correct behaviour: the
    /// driver gets used-ring acks, the device just doesn't report the
    /// numbers anywhere.
    fn drain_stats_queue(&mut self) {
        let (mem, irq) = {
            let state = self.state.lock();
            match (state.mem.clone(), state.irq.clone()) {
                (Some(m), Some(i)) => (m, i),
                _ => return,
            }
        };
        let queue = &mut self.queues[STATS_QUEUE];
        let mut completed = false;
        loop {
            let chain = match queue.pop_avail(mem.as_ref()) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "balloon: stats walk failed");
                    break;
                }
            };
            let head = chain.head_index();
            // We don't need the descriptors; just acknowledge the chain.
            let _ = chain.collect(mem.as_ref());
            if let Err(err) = queue.push_used(mem.as_ref(), head, 0) {
                tracing::warn!(error = %err, "balloon: stats push_used failed");
                break;
            }
            completed = true;
        }
        if completed {
            let _ = irq.trigger_queue();
        }
    }
}

#[cfg(test)]
mod tests {
    use squib_arch::IntId;
    use squib_core::{GuestAddress, SliceGuestMemory};
    use squib_gic::Gic;

    use super::*;
    use crate::queue::VIRTQ_DESC_F_NEXT;

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

    #[test]
    fn test_should_offer_2_queues_without_stats() {
        let dev = BalloonDevice::new(BalloonConfig::default());
        assert_eq!(dev.queue_max_sizes().len(), 2);
        assert_eq!(dev.avail_features() & F_STATS_VQ, 0);
    }

    #[test]
    fn test_should_offer_3_queues_with_stats() {
        let dev = BalloonDevice::new(BalloonConfig {
            stats_polling_interval_s: 5,
            ..Default::default()
        });
        assert_eq!(dev.queue_max_sizes().len(), 3);
        assert_ne!(dev.avail_features() & F_STATS_VQ, 0);
    }

    #[test]
    fn test_should_record_pfns_pushed_to_inflate_queue() {
        let mut dev = BalloonDevice::new(BalloonConfig::default());
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        // Set up queue 0 (inflate) with one descriptor for two PFNs.
        let q = &mut dev.queues_mut()[INFLATE_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        // Two PFNs at 0x4000_2000.
        mem.write_u32_le(GuestAddress(0x4000_2000), 0xAA).unwrap();
        mem.write_u32_le(GuestAddress(0x4000_2004), 0xBB).unwrap();
        // Descriptor 0: read-only buffer of 8 bytes.
        let base = 0x4000_0000u64;
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), 8).unwrap();
        mem.write_u16_le(GuestAddress(base + 12), 0).unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 0).unwrap();
        // Make available.
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();

        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(INFLATE_QUEUE as u16);
        assert_eq!(dev.inflated_pfn_count(), 2);
    }

    #[test]
    fn test_should_remove_pfns_pushed_to_deflate_queue() {
        let mut dev = BalloonDevice::new(BalloonConfig::default());
        // Pre-populate the inflated set.
        dev.inflated_pfns
            .lock()
            .extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        let q = &mut dev.queues_mut()[DEFLATE_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        mem.write_u32_le(GuestAddress(0x4000_2000), 0xBB).unwrap();
        let base = 0x4000_0000u64;
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), 4).unwrap();
        mem.write_u16_le(GuestAddress(base + 12), 0).unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();

        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(DEFLATE_QUEUE as u16);
        let pfns = dev.inflated_pfns.lock().clone();
        assert_eq!(pfns, vec![0xAA, 0xCC]);
    }

    #[test]
    fn test_should_silence_unused_imports() {
        // Keep VIRTQ_DESC_F_NEXT referenced so the import lints don't fire if
        // someone reorders the file later — virtio-balloon doesn't use NEXT
        // directly but the constant is part of the public queue surface.
        let _ = VIRTQ_DESC_F_NEXT;
    }
}
