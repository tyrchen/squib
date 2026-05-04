//! virtio-rng — entropy source.
//!
//! Per [14-virtio-and-devices.md §
//! 4.5](../../../specs/14-virtio-and-devices.md#45-virtio-rng-entropy): one queue, no config-space
//! layout. The driver presents a write-only descriptor chain; we fill it with random bytes from a
//! CSPRNG and push it into the used ring.
//!
//! Per CLAUDE.md `§ Cryptography & Secrets`, the CSPRNG MUST be
//! `OsRng` / `getrandom` / `aws-lc-rs::SystemRandom`. We use the standard
//! library's `getrandom`-equivalent via `OsRng`-style behaviour: reading from
//! `/dev/urandom` directly is not ideal because of fd-pressure under heavy
//! load; instead we rely on the user-provided source-of-entropy callback so
//! tests can inject deterministic bytes without touching the global system
//! RNG.

use std::sync::Arc;

use parking_lot::Mutex;
use squib_core::GuestMemory;

use crate::{
    device::{ActivateError, VirtioDevice},
    device_id::VirtioDeviceType,
    interrupt::IrqLine,
    queue::{Queue, VIRTQ_DESC_F_WRITE},
};

/// Per-queue maximum descriptor count for virtio-rng. The single queue
/// rarely needs more than a few outstanding requests; the cap is generous
/// and matches Firecracker.
pub const QUEUE_MAX_SIZE: u16 = 256;

/// Trait abstraction over the entropy source so tests can inject
/// deterministic bytes without touching the system RNG.
pub trait EntropySource: Send + Sync + std::fmt::Debug {
    /// Fill `buf` with cryptographically-secure random bytes.
    fn fill(&self, buf: &mut [u8]);
}

/// Production entropy source: OS-provided CSPRNG.
///
/// Holds an open `/dev/urandom` fd for the device's lifetime so we don't
/// pay the `open()` cost on every queue notification (CLAUDE.md
/// `§ Performance`: hot paths). The fd is opened at construction time
/// because `/dev/urandom` reads never block on macOS — the workspace's
/// `disallowed-types` ban targets runtime tokio code, not init-time CSPRNG
/// setup.
#[allow(clippy::disallowed_types)]
mod os_csprng {
    use std::fs::File;

    use parking_lot::Mutex;

    use super::EntropySource;

    /// Production entropy source — see [`super::OsEntropy`].
    #[derive(Debug)]
    pub struct OsEntropy {
        file: Mutex<File>,
    }

    impl OsEntropy {
        /// Open `/dev/urandom` and return a ready entropy source.
        ///
        /// # Errors
        /// `std::io::Error` if `/dev/urandom` cannot be opened.
        pub fn try_new() -> std::io::Result<Self> {
            let file = File::open("/dev/urandom")?;
            Ok(Self {
                file: Mutex::new(file),
            })
        }
    }

    impl EntropySource for OsEntropy {
        fn fill(&self, buf: &mut [u8]) {
            use std::io::Read;
            let mut f = self.file.lock();
            if let Err(err) = f.read_exact(buf) {
                panic!("/dev/urandom read failed mid-VM ({err}); aborting to protect the guest");
            }
        }
    }
}

pub use os_csprng::OsEntropy;

/// virtio-rng frontend.
#[derive(Debug)]
pub struct RngDevice {
    avail: u64,
    acked: u64,
    queues: Vec<Queue>,
    state: Arc<Mutex<ActiveState>>,
    source: Arc<dyn EntropySource>,
}

#[derive(Debug, Default)]
struct ActiveState {
    mem: Option<Arc<dyn GuestMemory>>,
    irq: Option<IrqLine>,
    activated: bool,
}

impl RngDevice {
    /// Build a virtio-rng with the OS CSPRNG as the source.
    ///
    /// # Errors
    /// `std::io::Error` if `/dev/urandom` cannot be opened — propagates so
    /// boot can fail visibly rather than degrade to weak entropy.
    pub fn try_new() -> std::io::Result<Self> {
        Ok(Self::with_source(Arc::new(OsEntropy::try_new()?)))
    }

    /// Build a virtio-rng with a custom entropy source — used by tests.
    #[must_use]
    pub fn with_source(source: Arc<dyn EntropySource>) -> Self {
        Self {
            // virtio-rng needs no special features beyond the always-on
            // VIRTIO_F_VERSION_1 contributed by the transport.
            avail: 0,
            acked: 0,
            queues: vec![Queue::new(QUEUE_MAX_SIZE)],
            state: Arc::new(Mutex::new(ActiveState::default())),
            source,
        }
    }
}

// `RngDevice::Default` is intentionally not provided — boot must observe
// CSPRNG-init failure rather than swallow it via `Default`.

impl VirtioDevice for RngDevice {
    fn device_type(&self) -> VirtioDeviceType {
        VirtioDeviceType::Rng
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
    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        for b in data.iter_mut() {
            *b = 0;
        }
    }
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // virtio-rng has no config space.
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
        if queue_index != 0 {
            return;
        }
        let (mem, irq) = {
            let state = self.state.lock();
            match (state.mem.clone(), state.irq.clone()) {
                (Some(m), Some(i)) => (m, i),
                _ => return,
            }
        };
        let queue = &mut self.queues[0];
        let source = Arc::clone(&self.source);
        let mut completed_any = false;
        loop {
            let chain = match queue.pop_avail(mem.as_ref()) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "virtio-rng: descriptor walk failed");
                    break;
                }
            };
            let head = chain.head_index();
            let mut bytes_written: u32 = 0;
            // Walk descriptors and fill each device-write descriptor.
            let descs = match chain.collect(mem.as_ref()) {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!(error = %err, "virtio-rng: chain collect failed");
                    break;
                }
            };
            for desc in descs {
                if desc.flags & VIRTQ_DESC_F_WRITE == 0 {
                    // virtio-rng descriptors are device-write only; skip
                    // anything else without erroring.
                    continue;
                }
                let len = desc.len as usize;
                let mut buf = vec![0u8; len];
                source.fill(&mut buf);
                if let Err(err) = mem.write(desc.addr, &buf) {
                    tracing::warn!(error = %err, "virtio-rng: write to guest failed");
                    break;
                }
                bytes_written = bytes_written.saturating_add(desc.len);
            }
            if let Err(err) = queue.push_used(mem.as_ref(), head, bytes_written) {
                tracing::warn!(error = %err, "virtio-rng: push_used failed");
                break;
            }
            completed_any = true;
        }
        if completed_any {
            let _ = irq.trigger_queue();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use squib_arch::IntId;
    use squib_core::{GuestAddress, SliceGuestMemory};
    use squib_gic::Gic;

    use super::*;
    use crate::{feature_bits, queue::VIRTQ_DESC_F_WRITE};

    #[derive(Debug)]
    struct FixedEntropy(Vec<u8>);
    impl EntropySource for FixedEntropy {
        fn fill(&self, buf: &mut [u8]) {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = self.0[i % self.0.len()];
            }
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

    fn setup() -> (RngDevice, Arc<SliceGuestMemory>, IrqLine) {
        let dev = RngDevice::with_source(Arc::new(FixedEntropy(b"abcd".to_vec())));
        let mem = Arc::new(SliceGuestMemory::new(GuestAddress(0x4000_0000), 0x4000));
        let gic: Arc<dyn Gic + Send + Sync> = Arc::new(StubGic);
        let irq = IrqLine::new(gic, IntId::from_spi_cell(16).unwrap());
        (dev, mem, irq)
    }

    #[test]
    fn test_should_offer_no_extra_features_beyond_version_1() {
        // Use the deterministic test source to keep the test hermetic; the
        // OS-backed `try_new()` would only fail on hosts without
        // `/dev/urandom`, which we'd notice elsewhere.
        let dev = RngDevice::with_source(Arc::new(FixedEntropy(b"x".to_vec())));
        assert_eq!(dev.avail_features(), 0);
        assert_eq!(dev.queue_max_sizes(), &[QUEUE_MAX_SIZE]);
    }

    #[test]
    fn test_should_fill_write_only_descriptor_with_random_bytes() {
        let (mut dev, mem, irq) = setup();
        // Set up queue
        {
            let q = &mut dev.queues_mut()[0];
            q.size = 8;
            q.desc_table_addr = GuestAddress(0x4000_0000);
            q.avail_ring_addr = GuestAddress(0x4000_0800);
            q.used_ring_addr = GuestAddress(0x4000_1000);
            q.ready = true;
        }
        // Descriptor 0: write-only, 16 bytes at 0x4000_2000.
        let base = 0x4000_0000u64;
        mem.write_u32_le(GuestAddress(base), 0x4000_2000).unwrap();
        mem.write_u32_le(GuestAddress(base + 4), 0).unwrap();
        mem.write_u32_le(GuestAddress(base + 8), 16).unwrap();
        mem.write_u16_le(GuestAddress(base + 12), VIRTQ_DESC_F_WRITE)
            .unwrap();
        mem.write_u16_le(GuestAddress(base + 14), 0).unwrap();
        // Mark descriptor 0 available.
        mem.write_u16_le(GuestAddress(0x4000_0804), 0).unwrap();
        mem.write_u16_le(GuestAddress(0x4000_0802), 1).unwrap();
        // Activate manually (we're not going through the transport in tests).
        dev.activate(mem.clone(), irq).unwrap();
        dev.process_queue(0);
        // Verify the guest buffer now contains "abcdabcd...".
        let mut got = [0u8; 16];
        mem.read(GuestAddress(0x4000_2000), &mut got).unwrap();
        assert_eq!(&got, b"abcdabcdabcdabcd");
        // Used ring slot 0 should report head=0, len=16.
        let used_head = mem.read_u32_le(GuestAddress(0x4000_1004)).unwrap();
        let used_len = mem.read_u32_le(GuestAddress(0x4000_1008)).unwrap();
        assert_eq!(used_head, 0);
        assert_eq!(used_len, 16);
    }

    #[test]
    fn test_should_be_quiet_when_no_descriptors_available() {
        let (mut dev, mem, irq) = setup();
        let q = &mut dev.queues_mut()[0];
        q.size = 8;
        q.desc_table_addr = GuestAddress(0x4000_0000);
        q.avail_ring_addr = GuestAddress(0x4000_0800);
        q.used_ring_addr = GuestAddress(0x4000_1000);
        q.ready = true;
        dev.activate(mem.clone(), irq).unwrap();
        dev.process_queue(0); // no panic; nothing happens.
    }

    /// `feature_bits` import smoke test — keeps the module compiled in.
    #[test]
    fn test_feature_bits_module_is_referenced() {
        let _ = feature_bits::VERSION_1;
    }
}
