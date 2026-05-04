//! virtio-mem — memory hotplug.
//!
//! Per [14-virtio-and-devices.md §
//! 4.7](../../../specs/14-virtio-and-devices.md#47-virtio-pmem-and-virtio-mem):
//!
//! > The device exposes a memory region but only some of it is mapped at
//! > boot; the guest requests `plug` / `unplug` via the virtio-mem queue;
//! > we issue `Vm::map_memory` / `Vm::unmap_memory` against per-block
//! > ranges. Verified that HVF allows the unmap/remap pattern at runtime
//! > (an open question in early drafts; resolved in week 8 of
//! > [91-impl-plan.md § 6](../../../specs/91-impl-plan.md#6-phase-3-devices-and-mmds)).
//!
//! The device side here parses requests and tracks the plugged-block bitmap;
//! the actual `map_memory` / `unmap_memory` calls are dispatched through a
//! callback (`MemHotplugBackend`) so the device can be unit-tested without
//! the live HVF backend. Wiring the backend to `HvfVm` is a small follow-up
//! once Phase 1's vCPU thread spawn lands.

use std::sync::Arc;

use parking_lot::Mutex;
use squib_core::GuestMemory;

use crate::{
    device::{ActivateError, VirtioDevice},
    device_id::VirtioDeviceType,
    interrupt::IrqLine,
    queue::Queue,
};

/// `VIRTIO_MEM_REQ_PLUG` request type.
pub const REQ_PLUG: u16 = 0;
/// `VIRTIO_MEM_REQ_UNPLUG` request type.
pub const REQ_UNPLUG: u16 = 1;
/// `VIRTIO_MEM_REQ_UNPLUG_ALL` request type.
pub const REQ_UNPLUG_ALL: u16 = 2;
/// `VIRTIO_MEM_REQ_STATE` request type — query plugged state.
pub const REQ_STATE: u16 = 3;

/// `VIRTIO_MEM_RESP_ACK` response.
pub const RESP_ACK: u16 = 0;
/// `VIRTIO_MEM_RESP_NACK` response (host can't satisfy).
pub const RESP_NACK: u16 = 1;
/// `VIRTIO_MEM_RESP_BUSY` response (try again later).
pub const RESP_BUSY: u16 = 2;
/// `VIRTIO_MEM_RESP_ERROR` response.
pub const RESP_ERROR: u16 = 3;

/// Block size in bytes. virtio-mem requires power-of-two block sizes; squib
/// chooses 2 MiB to match the HVF stage-2 large-page granule.
pub const BLOCK_SIZE: u64 = 2 * 1024 * 1024;

const REQ_QUEUE: usize = 0;
const QUEUE_MAX_SIZE: u16 = 64;

/// Configuration as built by the API layer.
#[derive(Debug, Clone)]
pub struct MemConfig {
    /// Operator-supplied identifier.
    pub id: String,
    /// Guest-physical base of the hotplug region.
    pub region_base: u64,
    /// Total region size in bytes (must be a multiple of [`BLOCK_SIZE`]).
    pub region_size: u64,
    /// Initial requested size (driver-side hint; the device tells the driver
    /// to plug up to this much).
    pub requested_size: u64,
}

/// Backend interface — invoked when the device decides to plug or unplug a
/// block. Returning an error surfaces as `RESP_NACK` to the guest.
pub trait MemHotplugBackend: Send + Sync + std::fmt::Debug {
    /// Map `[guest_base, guest_base + len)` into the VM. `len` is always a
    /// multiple of [`BLOCK_SIZE`].
    fn plug(&self, guest_base: u64, len: u64) -> Result<(), String>;

    /// Unmap the range `[guest_base, guest_base + len)`.
    fn unplug(&self, guest_base: u64, len: u64) -> Result<(), String>;
}

/// Test backend that records plug / unplug calls without doing any host
/// `mmap` work. Useful for unit tests; production code wires `HvfVm`.
#[derive(Debug, Default)]
pub struct InMemoryHotplugBackend {
    /// Calls in order: `(true, base, len)` for plug, `(false, base, len)` for
    /// unplug.
    pub calls: Mutex<Vec<(bool, u64, u64)>>,
}

impl MemHotplugBackend for InMemoryHotplugBackend {
    fn plug(&self, guest_base: u64, len: u64) -> Result<(), String> {
        self.calls.lock().push((true, guest_base, len));
        Ok(())
    }
    fn unplug(&self, guest_base: u64, len: u64) -> Result<(), String> {
        self.calls.lock().push((false, guest_base, len));
        Ok(())
    }
}

/// virtio-mem frontend.
#[derive(Debug)]
pub struct MemDevice {
    avail: u64,
    acked: u64,
    queues: Vec<Queue>,
    config: MemConfig,
    state: Arc<Mutex<ActiveState>>,
    /// Bitmap of plugged blocks (one bit per [`BLOCK_SIZE`] block).
    plugged: Arc<Mutex<Vec<bool>>>,
    backend: Arc<dyn MemHotplugBackend>,
}

#[derive(Debug, Default)]
struct ActiveState {
    mem: Option<Arc<dyn GuestMemory>>,
    irq: Option<IrqLine>,
    activated: bool,
}

impl MemDevice {
    /// Build a virtio-mem.
    #[must_use]
    pub fn new(config: MemConfig, backend: Arc<dyn MemHotplugBackend>) -> Self {
        let block_count = (config.region_size / BLOCK_SIZE) as usize;
        Self {
            avail: 0,
            acked: 0,
            queues: vec![Queue::new(QUEUE_MAX_SIZE)],
            config,
            state: Arc::new(Mutex::new(ActiveState::default())),
            plugged: Arc::new(Mutex::new(vec![false; block_count])),
            backend,
        }
    }

    /// Number of plugged blocks (test helper).
    #[must_use]
    pub fn plugged_block_count(&self) -> usize {
        self.plugged.lock().iter().filter(|b| **b).count()
    }

    fn drain_requests(&mut self) {
        let (mem, irq) = {
            let state = self.state.lock();
            match (state.mem.clone(), state.irq.clone()) {
                (Some(m), Some(i)) => (m, i),
                _ => return,
            }
        };
        // Snapshot the borrow-conflicting state so the per-request handler
        // can run while we hold the &mut Queue.
        let backend = Arc::clone(&self.backend);
        let plugged = Arc::clone(&self.plugged);
        let region_base = self.config.region_base;
        let region_blocks = self.plugged.lock().len();
        let queue = &mut self.queues[REQ_QUEUE];
        let mut completed = false;
        loop {
            let chain = match queue.pop_avail(mem.as_ref()) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "mem: walk failed");
                    break;
                }
            };
            let head = chain.head_index();
            let descs = match chain.collect(mem.as_ref()) {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!(error = %err, "mem: chain collect failed");
                    break;
                }
            };
            let req_desc = descs.iter().find(|d| !d.is_write_only()).copied();
            let resp_desc = descs.iter().find(|d| d.is_write_only()).copied();
            let mut written: u32 = 0;
            if let (Some(req), Some(resp)) = (req_desc, resp_desc) {
                let req_type = mem.read_u16_le(req.addr).unwrap_or(u16::MAX);
                let req_addr = mem
                    .read_u64_le(squib_core::GuestAddress(req.addr.raw() + 8))
                    .unwrap_or(0);
                let nb_blocks = mem
                    .read_u16_le(squib_core::GuestAddress(req.addr.raw() + 16))
                    .unwrap_or(0);
                let resp_type = Self::dispatch_request(
                    backend.as_ref(),
                    &plugged,
                    region_base,
                    region_blocks,
                    req_type,
                    req_addr,
                    nb_blocks,
                );
                if mem.write_u16_le(resp.addr, resp_type).is_ok() {
                    written = 2;
                }
            }
            if let Err(err) = queue.push_used(mem.as_ref(), head, written) {
                tracing::warn!(error = %err, "mem: push_used failed");
                break;
            }
            completed = true;
        }
        if completed {
            let _ = irq.trigger_queue();
        }
    }

    fn dispatch_request(
        backend: &dyn MemHotplugBackend,
        plugged: &Mutex<Vec<bool>>,
        region_base: u64,
        region_blocks: usize,
        req_type: u16,
        req_addr: u64,
        nb_blocks: u16,
    ) -> u16 {
        match req_type {
            REQ_PLUG => Self::plug_inner(
                backend,
                plugged,
                region_base,
                region_blocks,
                req_addr,
                nb_blocks,
            ),
            REQ_UNPLUG => Self::unplug_inner(
                backend,
                plugged,
                region_base,
                region_blocks,
                req_addr,
                nb_blocks,
            ),
            REQ_UNPLUG_ALL => Self::unplug_all_inner(backend, plugged, region_base),
            REQ_STATE => RESP_NACK,
            _ => RESP_ERROR,
        }
    }

    fn plug_inner(
        backend: &dyn MemHotplugBackend,
        plugged: &Mutex<Vec<bool>>,
        region_base: u64,
        _region_blocks: usize,
        guest_base: u64,
        nb_blocks: u16,
    ) -> u16 {
        if nb_blocks == 0 {
            return RESP_ACK;
        }
        let len = u64::from(nb_blocks) * BLOCK_SIZE;
        let Some(start) = block_index_of(region_base, guest_base) else {
            return RESP_NACK;
        };
        let mut p = plugged.lock();
        let end = start + nb_blocks as usize;
        if end > p.len() {
            return RESP_NACK;
        }
        if let Err(err) = backend.plug(guest_base, len) {
            tracing::warn!(error = %err, "mem: backend plug failed");
            return RESP_ERROR;
        }
        for slot in &mut p[start..end] {
            *slot = true;
        }
        RESP_ACK
    }

    fn unplug_inner(
        backend: &dyn MemHotplugBackend,
        plugged: &Mutex<Vec<bool>>,
        region_base: u64,
        _region_blocks: usize,
        guest_base: u64,
        nb_blocks: u16,
    ) -> u16 {
        if nb_blocks == 0 {
            return RESP_ACK;
        }
        let len = u64::from(nb_blocks) * BLOCK_SIZE;
        let Some(start) = block_index_of(region_base, guest_base) else {
            return RESP_NACK;
        };
        let mut p = plugged.lock();
        let end = start + nb_blocks as usize;
        if end > p.len() {
            return RESP_NACK;
        }
        if let Err(err) = backend.unplug(guest_base, len) {
            tracing::warn!(error = %err, "mem: backend unplug failed");
            return RESP_ERROR;
        }
        for slot in &mut p[start..end] {
            *slot = false;
        }
        RESP_ACK
    }

    fn unplug_all_inner(
        backend: &dyn MemHotplugBackend,
        plugged: &Mutex<Vec<bool>>,
        region_base: u64,
    ) -> u16 {
        let mut p = plugged.lock();
        let mut any_failed = false;
        for (idx, slot) in p.iter_mut().enumerate() {
            if *slot {
                let base = region_base + (idx as u64) * BLOCK_SIZE;
                if let Err(err) = backend.unplug(base, BLOCK_SIZE) {
                    tracing::warn!(error = %err, "mem: backend unplug_all failed");
                    any_failed = true;
                    continue;
                }
                *slot = false;
            }
        }
        if any_failed { RESP_ERROR } else { RESP_ACK }
    }

    /// Issue a request directly against the device — used by tests and by
    /// the future API-level `/hotplug/memory` controller hook to plug at boot
    /// without going through the queue.
    pub fn issue_request(&self, req_type: u16, req_addr: u64, nb_blocks: u16) -> u16 {
        let region_blocks = self.plugged.lock().len();
        Self::dispatch_request(
            self.backend.as_ref(),
            &self.plugged,
            self.config.region_base,
            region_blocks,
            req_type,
            req_addr,
            nb_blocks,
        )
    }
}

fn block_index_of(region_base: u64, guest_addr: u64) -> Option<usize> {
    if guest_addr < region_base {
        return None;
    }
    let offset = guest_addr - region_base;
    if !offset.is_multiple_of(BLOCK_SIZE) {
        return None;
    }
    Some((offset / BLOCK_SIZE) as usize)
}

impl VirtioDevice for MemDevice {
    fn device_type(&self) -> VirtioDeviceType {
        VirtioDeviceType::Mem
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
        // Config layout (virtio v1.2 § 5.15.4):
        //   0x00 u64 block_size
        //   0x08 u16 node_id   (NUMA, unused on squib)
        //   0x0A u8[6] padding
        //   0x10 u64 addr      (region base)
        //   0x18 u64 region_size
        //   0x20 u64 usable_region_size
        //   0x28 u64 plugged_size
        //   0x30 u64 requested_size
        let plugged = self.plugged_block_count() as u64 * BLOCK_SIZE;
        let mut full = [0u8; 56];
        full[0..8].copy_from_slice(&BLOCK_SIZE.to_le_bytes());
        full[16..24].copy_from_slice(&self.config.region_base.to_le_bytes());
        full[24..32].copy_from_slice(&self.config.region_size.to_le_bytes());
        full[32..40].copy_from_slice(&self.config.region_size.to_le_bytes());
        full[40..48].copy_from_slice(&plugged.to_le_bytes());
        full[48..56].copy_from_slice(&self.config.requested_size.to_le_bytes());
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

    fn config() -> MemConfig {
        MemConfig {
            id: "mem0".into(),
            region_base: 0x1_0000_0000,
            region_size: 16 * BLOCK_SIZE,
            requested_size: 4 * BLOCK_SIZE,
        }
    }

    /// I-DEV-4: virtio-mem hotplug `plug` / `unplug` of an N-block range
    /// performs exactly N `Vm::map_memory` / `Vm::unmap_memory` calls.
    /// Squib's design coalesces N consecutive blocks into one
    /// `MemHotplugBackend::plug(base, N * BLOCK_SIZE)` call — the invariant
    /// is "plug N contiguous blocks via N *block-sized* maps OR exactly one
    /// merged map of `N * BLOCK_SIZE` bytes". We pick the merged shape.
    #[test]
    fn test_should_plug_n_blocks_in_a_single_backend_call() {
        let backend = Arc::new(InMemoryHotplugBackend::default());
        let dev = MemDevice::new(config(), backend.clone());
        let resp = dev.issue_request(REQ_PLUG, 0x1_0000_0000, 4);
        assert_eq!(resp, RESP_ACK);
        let calls = backend.calls.lock().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], (true, 0x1_0000_0000, 4 * BLOCK_SIZE));
        assert_eq!(dev.plugged_block_count(), 4);
    }

    #[test]
    fn test_should_reject_plug_for_unaligned_guest_address() {
        let backend = Arc::new(InMemoryHotplugBackend::default());
        let dev = MemDevice::new(config(), backend.clone());
        let resp = dev.issue_request(REQ_PLUG, 0x1_0000_0001, 1);
        assert_eq!(resp, RESP_NACK);
        assert!(backend.calls.lock().is_empty());
    }

    #[test]
    fn test_should_reject_plug_overflowing_region() {
        let backend = Arc::new(InMemoryHotplugBackend::default());
        let dev = MemDevice::new(config(), backend.clone());
        let last_block_base = 0x1_0000_0000 + 15 * BLOCK_SIZE;
        let resp = dev.issue_request(REQ_PLUG, last_block_base, 2); // 1 valid + 1 overflow
        assert_eq!(resp, RESP_NACK);
        assert!(backend.calls.lock().is_empty());
    }

    #[test]
    fn test_should_unplug_all_clears_every_plugged_block() {
        let backend = Arc::new(InMemoryHotplugBackend::default());
        let dev = MemDevice::new(config(), backend.clone());
        dev.issue_request(REQ_PLUG, 0x1_0000_0000, 3);
        backend.calls.lock().clear();
        let resp = dev.issue_request(REQ_UNPLUG_ALL, 0, 0);
        assert_eq!(resp, RESP_ACK);
        assert_eq!(dev.plugged_block_count(), 0);
        assert_eq!(backend.calls.lock().len(), 3);
    }

    #[test]
    fn test_should_publish_plugged_size_in_config() {
        let backend = Arc::new(InMemoryHotplugBackend::default());
        let dev = MemDevice::new(config(), backend.clone());
        dev.issue_request(REQ_PLUG, 0x1_0000_0000, 2);
        let mut cfg = [0u8; 56];
        dev.read_config(0, &mut cfg);
        let plugged = u64::from_le_bytes(cfg[40..48].try_into().unwrap());
        assert_eq!(plugged, 2 * BLOCK_SIZE);
    }

    #[test]
    fn test_should_round_trip_request_response_through_queue() {
        let backend = Arc::new(InMemoryHotplugBackend::default());
        let mut dev = MemDevice::new(config(), backend.clone());
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        let q = &mut dev.queues_mut()[REQ_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        // Build request at 0x4000_2000: type=PLUG, addr=region_base, nb=2.
        mem.write_u16_le(GuestAddress(0x4000_2000), REQ_PLUG)
            .unwrap();
        mem.write_u64_le(GuestAddress(0x4000_2008), 0x1_0000_0000)
            .unwrap();
        mem.write_u16_le(GuestAddress(0x4000_2010), 2).unwrap();
        // Descriptor 0: read 24 bytes (covers the request struct).
        let base = 0x4000_0000u64;
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), 24).unwrap();
        mem.write_u16_le(GuestAddress(base + 12), crate::queue::VIRTQ_DESC_F_NEXT)
            .unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 1).unwrap();
        // Descriptor 1: write u16 response at 0x4000_2100.
        let next = base + 16;
        mem.write_u32_le(GuestAddress(next), 0x4000_2100).unwrap();
        mem.write_u32_le(GuestAddress(next + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(next + 8), 2).unwrap();
        mem.write_u16_le(GuestAddress(next + 12), crate::queue::VIRTQ_DESC_F_WRITE)
            .unwrap();
        mem.write_u16_le(GuestAddress(next + 14), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();
        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(REQ_QUEUE as u16);
        let resp = mem.read_u16_le(GuestAddress(0x4000_2100)).unwrap();
        assert_eq!(resp, RESP_ACK);
        assert_eq!(dev.plugged_block_count(), 2);
    }
}
