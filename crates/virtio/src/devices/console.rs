//! virtio-console — serial frontend.
//!
//! Per [14-virtio-and-devices.md §
//! 4.6](../../../specs/14-virtio-and-devices.md#46-virtio-console-serial): the UART backend writes
//! to a regular file, a FIFO (`mkfifo` works on macOS), or `stdout` per the `/serial` PUT config.
//! Same shape as Firecracker.
//!
//! Only the basic two-port flavour is implemented for 1.0 — receive and
//! transmit queues for port 0. The multi-port extension behind
//! `VIRTIO_CONSOLE_F_MULTIPORT` is documented as future work.

use std::{io::Write, sync::Arc};

use parking_lot::Mutex;
use squib_core::GuestMemory;

use crate::{
    device::{ActivateError, VirtioDevice},
    device_id::VirtioDeviceType,
    interrupt::IrqLine,
    queue::Queue,
};

/// `VIRTIO_CONSOLE_F_SIZE` — driver may read terminal columns/rows from
/// config-space.
pub const F_SIZE: u64 = 1 << 0;
/// `VIRTIO_CONSOLE_F_MULTIPORT` — multi-port mode (not implemented in 1.0).
pub const F_MULTIPORT: u64 = 1 << 1;

/// Receive-queue index (host → guest).
pub const RX_QUEUE: usize = 0;
/// Transmit-queue index (guest → host).
pub const TX_QUEUE: usize = 1;

const QUEUE_MAX_SIZE: u16 = 64;

/// Where the console output goes.
pub enum ConsoleSink {
    /// Discard all output.
    Discard,
    /// Append to a host-side `Write` (`stdout`, file, FIFO).
    Writer(Box<dyn Write + Send>),
}

impl std::fmt::Debug for ConsoleSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discard => f.write_str("ConsoleSink::Discard"),
            Self::Writer(_) => f.write_str("ConsoleSink::Writer(<dyn Write + Send>)"),
        }
    }
}

/// virtio-console device.
#[derive(Debug)]
pub struct ConsoleDevice {
    avail: u64,
    acked: u64,
    queues: Vec<Queue>,
    sink: Arc<Mutex<ConsoleSink>>,
    state: Arc<Mutex<ActiveState>>,
}

#[derive(Debug, Default)]
struct ActiveState {
    mem: Option<Arc<dyn GuestMemory>>,
    irq: Option<IrqLine>,
    activated: bool,
}

impl ConsoleDevice {
    /// Build a console with a custom sink.
    #[must_use]
    pub fn new(sink: ConsoleSink) -> Self {
        Self {
            avail: 0,
            acked: 0,
            queues: vec![Queue::new(QUEUE_MAX_SIZE), Queue::new(QUEUE_MAX_SIZE)],
            sink: Arc::new(Mutex::new(sink)),
            state: Arc::new(Mutex::new(ActiveState::default())),
        }
    }

    /// Build a console that discards output. Useful for tests and headless
    /// boots.
    #[must_use]
    pub fn discard() -> Self {
        Self::new(ConsoleSink::Discard)
    }

    fn drain_tx(&mut self) {
        let (mem, irq) = {
            let state = self.state.lock();
            match (state.mem.clone(), state.irq.clone()) {
                (Some(m), Some(i)) => (m, i),
                _ => return,
            }
        };
        let queue = &mut self.queues[TX_QUEUE];
        let mut completed = false;
        loop {
            let chain = match queue.pop_avail(mem.as_ref()) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "console: tx walk failed");
                    break;
                }
            };
            let head = chain.head_index();
            let descs = match chain.collect(mem.as_ref()) {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!(error = %err, "console: tx collect failed");
                    break;
                }
            };
            let mut written: u32 = 0;
            for desc in descs {
                if desc.is_write_only() {
                    continue;
                }
                let len = desc.len as usize;
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u8; len];
                if let Err(err) = mem.read(desc.addr, &mut buf) {
                    tracing::warn!(error = %err, "console: tx read from guest failed");
                    continue;
                }
                let mut sink = self.sink.lock();
                match &mut *sink {
                    ConsoleSink::Discard => {}
                    ConsoleSink::Writer(w) => {
                        if let Err(err) = w.write_all(&buf) {
                            tracing::warn!(error = %err, "console: tx host write failed");
                        }
                    }
                }
                written = written.saturating_add(desc.len);
            }
            if let Err(err) = queue.push_used(mem.as_ref(), head, written) {
                tracing::warn!(error = %err, "console: push_used failed");
                break;
            }
            completed = true;
        }
        if completed {
            let _ = irq.trigger_queue();
        }
    }
}

impl VirtioDevice for ConsoleDevice {
    fn device_type(&self) -> VirtioDeviceType {
        VirtioDeviceType::Console
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
        const SIZES: &[u16] = &[QUEUE_MAX_SIZE, QUEUE_MAX_SIZE];
        SIZES
    }
    fn queues(&self) -> &[Queue] {
        &self.queues
    }
    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }
    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        for b in data.iter_mut() {
            *b = 0;
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
        if queue_index as usize == TX_QUEUE {
            self.drain_tx();
        }
        // RX queue: host-side input wakes it up; that path lives in the VMM
        // event loop, not here. The driver may still post empty descriptors
        // to RX which we leave parked until input arrives.
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use squib_arch::IntId;
    use squib_core::{GuestAddress, SliceGuestMemory};
    use squib_gic::Gic;

    use super::*;

    /// Sink wrapper that exposes the captured bytes for assertions.
    #[derive(Debug, Default)]
    struct CapturedSink(Arc<Mutex<Vec<u8>>>);
    impl Write for CapturedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
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

    #[test]
    fn test_should_offer_two_queues_one_each_direction() {
        let dev = ConsoleDevice::discard();
        assert_eq!(dev.queue_max_sizes().len(), 2);
    }

    #[test]
    fn test_should_forward_tx_descriptors_to_host_sink() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturedSink(buf.clone());
        let mut dev = ConsoleDevice::new(ConsoleSink::Writer(Box::new(sink)));
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        let q = &mut dev.queues_mut()[TX_QUEUE];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        // Buffer "hi\n" at 0x4000_2000.
        mem.write(GuestAddress(0x4000_2000), b"hi\n").unwrap();
        let base = 0x4000_0000u64;
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), 3).unwrap();
        mem.write_u16_le(GuestAddress(base + 12), 0).unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();
        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(TX_QUEUE as u16);
        assert_eq!(buf.lock().as_slice(), b"hi\n");
    }

    #[test]
    fn test_discard_sink_is_a_noop() {
        let mut dev = ConsoleDevice::discard();
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        dev.activate(mem.clone(), line()).unwrap();
        dev.process_queue(TX_QUEUE as u16); // no panic with empty queue setup.
    }

    /// Keep imports referenced.
    #[test]
    fn test_constants_referenced() {
        let _ = F_SIZE;
        let _ = F_MULTIPORT;
        let _ = Cursor::new(Vec::<u8>::new());
    }
}
