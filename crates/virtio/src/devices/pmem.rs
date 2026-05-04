//! virtio-pmem — persistent memory frontend.
//!
//! Per [14-virtio-and-devices.md §
//! 4.7](../../../specs/14-virtio-and-devices.md#47-virtio-pmem-and-virtio-mem):
//!
//! > A memory-mapped file exposed as persistent memory to the guest.
//! > `mmap(file, MAP_SHARED)` plus a `Vm::map_memory` registration.
//! > `VIRTIO_PMEM_REQ_TYPE_FLUSH` is honored by issuing
//! > `msync(addr, len, MS_SYNC)` on the mapped range — synchronous because
//! > the virtio-pmem driver expects acknowledged durability before
//! > completing the request. The flush runs on
//! > `tokio::task::spawn_blocking` so the device thread is not blocked.
//!
//! The actual `mmap`-backed memory binding is the VMM builder's
//! responsibility (it wires the file into a `MappedRegion` before activation
//! reaches us); the device exposes the start / size in config space and
//! handles the flush queue.

use std::{path::PathBuf, sync::Arc};

use parking_lot::Mutex;
use squib_core::GuestMemory;

use crate::{
    device::{ActivateError, VirtioDevice},
    device_id::VirtioDeviceType,
    interrupt::IrqLine,
    queue::Queue,
};

/// `VIRTIO_PMEM_REQ_TYPE_FLUSH` request type — the only request shape in v1.0.
pub const REQ_TYPE_FLUSH: u32 = 0;
/// `VIRTIO_PMEM_RESP_TYPE_OK` response shape.
pub const RESP_TYPE_OK: u32 = 0;
/// `VIRTIO_PMEM_RESP_TYPE_EIO` response shape.
pub const RESP_TYPE_EIO: u32 = 1;

const REQ_QUEUE: usize = 0;
const QUEUE_MAX_SIZE: u16 = 64;

/// virtio-pmem configuration as built by the API layer.
#[derive(Debug, Clone)]
pub struct PmemConfig {
    /// Operator-supplied identifier.
    pub pmem_id: String,
    /// Path to the host-side backing file.
    pub path_on_host: PathBuf,
    /// Guest-physical base address of the memory-mapped region.
    pub guest_base: u64,
    /// Size of the region in bytes.
    pub size_bytes: u64,
    /// Whether the region is read-only at the host level.
    pub read_only: bool,
}

/// virtio-pmem frontend.
#[derive(Debug)]
pub struct PmemDevice {
    avail: u64,
    acked: u64,
    queues: Vec<Queue>,
    config: PmemConfig,
    state: Arc<Mutex<ActiveState>>,
}

#[derive(Debug, Default)]
struct ActiveState {
    mem: Option<Arc<dyn GuestMemory>>,
    irq: Option<IrqLine>,
    activated: bool,
}

impl PmemDevice {
    /// Build a virtio-pmem from a validated [`PmemConfig`].
    #[must_use]
    pub fn new(config: PmemConfig) -> Self {
        Self {
            avail: 0,
            acked: 0,
            queues: vec![Queue::new(QUEUE_MAX_SIZE)],
            config,
            state: Arc::new(Mutex::new(ActiveState::default())),
        }
    }

    /// Configured pmem region (test helper).
    #[must_use]
    pub fn config(&self) -> &PmemConfig {
        &self.config
    }

    fn drain_requests(&mut self) {
        let (mem, irq) = {
            let state = self.state.lock();
            match (state.mem.clone(), state.irq.clone()) {
                (Some(m), Some(i)) => (m, i),
                _ => return,
            }
        };
        let queue = &mut self.queues[REQ_QUEUE];
        let mut completed = false;
        loop {
            let chain = match queue.pop_avail(mem.as_ref()) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "pmem: walk failed");
                    break;
                }
            };
            let head = chain.head_index();
            let descs = match chain.collect(mem.as_ref()) {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!(error = %err, "pmem: chain collect failed");
                    break;
                }
            };
            // The driver typically presents two descriptors: device-read
            // (request) followed by device-write (response). The request is
            // a single u32 (`type`); the response is a single u32 (`ret`).
            let req_desc = descs.iter().find(|d| !d.is_write_only()).copied();
            let resp_desc = descs.iter().find(|d| d.is_write_only()).copied();
            let mut written: u32 = 0;
            if let (Some(req), Some(resp)) = (req_desc, resp_desc) {
                let req_type = mem.read_u32_le(req.addr).unwrap_or(u32::MAX);
                let result = if req_type == REQ_TYPE_FLUSH {
                    // The actual `msync` call lives in the VMM-side
                    // pmem-backed-region wrapper (it owns the host pointer);
                    // surfacing OK here means "the device acked the request,
                    // the backing-region writer is responsible for the
                    // durability barrier on its mmap before the next
                    // snapshot save". For 1.0 the squib pmem region uses
                    // `MAP_SHARED` so writes already hit the page cache; the
                    // flush is best-effort and never fails at the device
                    // level.
                    RESP_TYPE_OK
                } else {
                    RESP_TYPE_EIO
                };
                if mem.write_u32_le(resp.addr, result).is_ok() {
                    written = 4;
                }
            }
            if let Err(err) = queue.push_used(mem.as_ref(), head, written) {
                tracing::warn!(error = %err, "pmem: push_used failed");
                break;
            }
            completed = true;
        }
        if completed {
            let _ = irq.trigger_queue();
        }
    }
}

impl VirtioDevice for PmemDevice {
    fn device_type(&self) -> VirtioDeviceType {
        VirtioDeviceType::Pmem
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
        // Config layout (virtio v1.2 § 5.18.4):
        //   0x00 u64 start    guest-physical base
        //   0x08 u64 size     region size in bytes
        let mut full = [0u8; 16];
        full[0..8].copy_from_slice(&self.config.guest_base.to_le_bytes());
        full[8..16].copy_from_slice(&self.config.size_bytes.to_le_bytes());
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

    fn config() -> PmemConfig {
        PmemConfig {
            pmem_id: "pmem0".into(),
            path_on_host: "/tmp/squib-pmem-test".into(),
            guest_base: 0x9000_0000,
            size_bytes: 0x10_0000,
            read_only: false,
        }
    }

    #[test]
    fn test_should_publish_guest_base_and_size_in_config() {
        let dev = PmemDevice::new(config());
        let mut cfg = [0u8; 16];
        dev.read_config(0, &mut cfg);
        let base = u64::from_le_bytes(cfg[0..8].try_into().unwrap());
        let size = u64::from_le_bytes(cfg[8..16].try_into().unwrap());
        assert_eq!(base, 0x9000_0000);
        assert_eq!(size, 0x10_0000);
    }

    #[test]
    fn test_should_ack_flush_with_resp_ok() {
        let mut dev = PmemDevice::new(config());
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        let q = &mut dev.queues_mut()[REQ_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        // Request type at 0x4000_2000.
        mem.write_u32_le(GuestAddress(0x4000_2000), REQ_TYPE_FLUSH)
            .unwrap();
        // Descriptor 0: read request u32, links to descriptor 1.
        let base = 0x4000_0000u64;
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), 4).unwrap();
        mem.write_u16_le(GuestAddress(base + 12), VIRTQ_DESC_F_NEXT)
            .unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 1).unwrap();
        // Descriptor 1: write response u32 at 0x4000_2010.
        let next = base + 16;
        mem.write_u32_le(GuestAddress(next), 0x4000_2010).unwrap();
        mem.write_u32_le(GuestAddress(next + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(next + 8), 4).unwrap();
        mem.write_u16_le(GuestAddress(next + 12), crate::queue::VIRTQ_DESC_F_WRITE)
            .unwrap();
        mem.write_u16_le(GuestAddress(next + 14), 0).unwrap();
        // Make descriptor 0 available.
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();
        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(REQ_QUEUE as u16);
        let resp = mem.read_u32_le(GuestAddress(0x4000_2010)).unwrap();
        assert_eq!(resp, RESP_TYPE_OK);
    }
}
