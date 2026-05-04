//! ARM PL011 UART r1p5 — minimal MMIO emulation.
//!
//! Squib emulates exactly the subset of the PL011 register set that
//! aarch64 Linux's `pl011` driver touches during early boot and steady-
//! state console traffic. We intentionally do not implement DMA, FIFO
//! depth manipulation, modem control, or break detection — squib's
//! console is byte-stream-oriented and the host write path absorbs the
//! data immediately.
//!
//! ## Register layout (offsets from MMIO base)
//!
//! | Offset | Name | Width | RW | Notes |
//! |--------|------|-------|----|-------|
//! | `0x000` | DR | 32 | RW | Data register (TX bottom 8 bits, RX bottom 8 bits) |
//! | `0x004` | RSR / ECR | 32 | RW | Receive status / error clear (zeros, accept any write) |
//! | `0x018` | FR | 32 | R | Flag register (TX_FIFO_EMPTY=0x80, RX_FIFO_EMPTY=0x10, TX_FIFO_FULL=0x20) |
//! | `0x020` | ILPR | 32 | RW | IrDA low-power counter (unused; accept any) |
//! | `0x024` | IBRD | 32 | RW | Integer baud rate (accept any) |
//! | `0x028` | FBRD | 32 | RW | Fractional baud rate (accept any) |
//! | `0x02C` | LCRH | 32 | RW | Line control (accept any) |
//! | `0x030` | CR | 32 | RW | Control (accept any) |
//! | `0x034` | IFLS | 32 | RW | Interrupt FIFO level select (accept any) |
//! | `0x038` | IMSC | 32 | RW | Interrupt mask (we honour RX bit) |
//! | `0x03C` | RIS | 32 | R | Raw interrupt status |
//! | `0x040` | MIS | 32 | R | Masked interrupt status (RIS & IMSC) |
//! | `0x044` | ICR | 32 | W | Interrupt clear |
//! | `0x048` | DMACR | 32 | RW | DMA control (accept any) |
//! | `0xFE0..0xFFC` | PeriphID/PCellID | 32 | R | PL011 r1p5 magic — needed by Linux probe |
//!
//! Linux's `drivers/tty/serial/amba-pl011.c` matches the device by reading
//! `PeriphID0..PeriphID3` (`0xFE0..0xFEC`) and `PCellID0..PCellID3`
//! (`0xFF0..0xFFC`); the constants are pinned to the r1p5 silicon
//! revision. Without these the driver refuses to bind.

use std::sync::Arc;

use parking_lot::Mutex;
use squib_arch::IntId;
use squib_bus::BusDevice;
use squib_gic::Gic;

/// Width of the MMIO window claimed on the bus. Linux's PL011 driver
/// `mmaps` 0x1000 and probes within; we honour the full page.
pub const PL011_MMIO_REGION_BYTES: u64 = 0x1000;

/// `FR` — TX_FIFO_EMPTY: bit 7. We always report TX as empty (host write
/// is synchronous so there's nothing buffered).
const FR_TX_EMPTY: u32 = 1 << 7;
/// `FR` — TX_FIFO_FULL: bit 5. Always 0 (the host write absorbs every byte).
const _FR_TX_FULL: u32 = 1 << 5;
/// `FR` — RX_FIFO_EMPTY: bit 4. Set when the RX queue is empty.
const FR_RX_EMPTY: u32 = 1 << 4;
/// `FR` — BUSY: bit 3. Always 0.
const _FR_BUSY: u32 = 1 << 3;

/// `IMSC` — RX interrupt mask: bit 4. When set, RX-ready raises an IRQ.
const IMSC_RX: u32 = 1 << 4;
/// `RIS` / `MIS` — RX interrupt status: bit 4.
const INT_RX: u32 = 1 << 4;

/// PrimeCell magic at offsets 0xFE0..0xFFC. These four pairs match the
/// PL011 r1p5 part as documented in DDI0183 § 3.1 and what Linux's
/// `amba-pl011.c` matches against.
const PERIPH_PCELL_ID: [(u64, u32); 8] = [
    (0xFE0, 0x11), // PeriphID0
    (0xFE4, 0x10), // PeriphID1
    (0xFE8, 0x14), // PeriphID2 (revision r1p5)
    (0xFEC, 0x00), // PeriphID3
    (0xFF0, 0x0D), // PCellID0
    (0xFF4, 0xF0),
    (0xFF8, 0x05),
    (0xFFC, 0xB1),
];

/// Sink for emitted bytes. Tests use `Vec<u8>`-backed sinks; production
/// uses `stdout` / a file / a FIFO per the operator's `/serial` config.
pub trait Pl011Sink: Send + std::fmt::Debug {
    /// Emit one byte to the host-side console.
    fn write_byte(&mut self, byte: u8);
}

/// Sink that drops every byte. Default for headless boots.
#[derive(Debug, Default)]
pub struct DiscardSink;

impl Pl011Sink for DiscardSink {
    fn write_byte(&mut self, _byte: u8) {}
}

/// Sink that pushes bytes into a shared `Vec<u8>`. Useful for tests and
/// for capturing output for the integration test.
#[derive(Debug, Clone, Default)]
pub struct CapturedSink {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl CapturedSink {
    /// Build a fresh captured sink and a clone-shareable handle to its
    /// buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of bytes captured so far.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        self.buffer.lock().clone()
    }

    /// Snapshot as a UTF-8 string (lossy on invalid sequences).
    #[must_use]
    pub fn as_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.buffer.lock()).to_string()
    }
}

impl Pl011Sink for CapturedSink {
    fn write_byte(&mut self, byte: u8) {
        self.buffer.lock().push(byte);
    }
}

/// Sink that wraps any `std::io::Write + Send`. Used for `stdout`, file,
/// FIFO output paths.
pub struct WriterSink {
    writer: Box<dyn std::io::Write + Send>,
}

impl WriterSink {
    /// Wrap a writer.
    #[must_use]
    pub fn new(writer: Box<dyn std::io::Write + Send>) -> Self {
        Self { writer }
    }
}

impl std::fmt::Debug for WriterSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriterSink(<dyn Write + Send>)")
    }
}

impl Pl011Sink for WriterSink {
    fn write_byte(&mut self, byte: u8) {
        let _ = self.writer.write_all(&[byte]);
    }
}

/// PL011 emulation.
///
/// The TX path is synchronous: a guest write to `DR` immediately calls
/// `sink.write_byte(b)`. The RX path is event-driven: the host calls
/// [`Pl011::push_rx`] from the operator-side input pipe; that buffers the
/// byte and (if `IMSC_RX` is set) raises an edge-rising IRQ via the GIC.
pub struct Pl011 {
    sink: Box<dyn Pl011Sink>,
    rx_queue: std::collections::VecDeque<u8>,
    /// IMSC — interrupt mask. We only honour RX bit; TX bit is meaningless
    /// because TX is synchronous.
    imsc: u32,
    /// RIS — raw interrupt status; OR-folded into MIS via IMSC.
    ris: u32,
    /// Captured baud / line / control values; we accept and reflect them
    /// so Linux's probe doesn't error on a follow-up read.
    ibrd: u32,
    fbrd: u32,
    lcrh: u32,
    cr: u32,
    iflss: u32,
    dmacr: u32,
    ilpr: u32,
    /// IRQ delivery handle. PL011 in our setup is **level-triggered** (RX
    /// remains asserted until the driver reads the byte); the GIC wrapper
    /// supports both level and edge via `set_spi_level`/`pulse_spi`. Linux's
    /// PL011 driver works with either; we go with level for fidelity to
    /// real silicon.
    gic: Arc<dyn Gic + Send + Sync>,
    intid: IntId,
}

impl Pl011 {
    /// Build a PL011 with a sink and the GIC wiring.
    #[must_use]
    pub fn new(sink: Box<dyn Pl011Sink>, gic: Arc<dyn Gic + Send + Sync>, intid: IntId) -> Self {
        Self {
            sink,
            rx_queue: std::collections::VecDeque::new(),
            imsc: 0,
            ris: 0,
            ibrd: 0,
            fbrd: 0,
            lcrh: 0,
            cr: 0,
            iflss: 0,
            dmacr: 0,
            ilpr: 0,
            gic,
            intid,
        }
    }

    /// Push a byte from the host-side input source into the RX queue.
    /// Returns the new RX queue length.
    pub fn push_rx(&mut self, byte: u8) -> usize {
        self.rx_queue.push_back(byte);
        // RX-ready bit on; if the driver has unmasked it, drive the line.
        self.ris |= INT_RX;
        if self.imsc & IMSC_RX != 0 {
            // Level-triggered: stay asserted while a byte is queued.
            let _ = self.gic.set_spi_level(self.intid, true);
        }
        self.rx_queue.len()
    }

    fn fr_value(&self) -> u32 {
        let mut fr = FR_TX_EMPTY;
        if self.rx_queue.is_empty() {
            fr |= FR_RX_EMPTY;
        }
        fr
    }

    fn read_dr(&mut self) -> u32 {
        if let Some(b) = self.rx_queue.pop_front() {
            // If this empties the queue, drop the RX-ready bit.
            if self.rx_queue.is_empty() {
                self.ris &= !INT_RX;
                let _ = self.gic.set_spi_level(self.intid, false);
            }
            u32::from(b)
        } else {
            0
        }
    }

    fn write_dr(&mut self, value: u32) {
        // Bottom 8 bits are the byte to transmit; upper bits are status
        // flags (parity error, etc.) the guest sets — we ignore them.
        self.sink.write_byte(value as u8);
    }

    fn read_register(&mut self, offset: u64) -> u32 {
        // PrimeCell magic lookup first — short-circuits the more interesting
        // probes Linux runs at boot.
        for &(off, val) in &PERIPH_PCELL_ID {
            if off == offset {
                return val;
            }
        }
        match offset {
            0x000 => self.read_dr(),
            0x004 => 0, // RSR / ECR — no errors
            0x018 => self.fr_value(),
            0x020 => self.ilpr,
            0x024 => self.ibrd,
            0x028 => self.fbrd,
            0x02C => self.lcrh,
            0x030 => self.cr,
            0x034 => self.iflss,
            0x038 => self.imsc,
            0x03C => self.ris,
            0x040 => self.ris & self.imsc,
            0x048 => self.dmacr,
            _ => {
                tracing::trace!(offset, "PL011 read: unknown register, returning 0");
                0
            }
        }
    }

    fn write_register(&mut self, offset: u64, value: u32) {
        match offset {
            0x000 => self.write_dr(value),
            0x004 => {} // RSR / ECR — write clears errors; we have none.
            0x020 => self.ilpr = value,
            0x024 => self.ibrd = value,
            0x028 => self.fbrd = value,
            0x02C => self.lcrh = value,
            0x030 => self.cr = value,
            0x034 => self.iflss = value,
            0x038 => {
                self.imsc = value;
                // Mask change may unblock or block delivery of the current
                // RIS state. Re-evaluate the line.
                let asserted = (self.imsc & self.ris & INT_RX) != 0;
                let _ = self.gic.set_spi_level(self.intid, asserted);
            }
            0x044 => {
                // ICR — clear interrupt status bits. The driver writes the
                // bits it wants to clear.
                self.ris &= !value;
                let asserted = (self.imsc & self.ris & INT_RX) != 0;
                let _ = self.gic.set_spi_level(self.intid, asserted);
            }
            0x048 => self.dmacr = value,
            _ => {
                tracing::trace!(offset, value, "PL011 write: unknown register, ignored");
            }
        }
    }
}

impl std::fmt::Debug for Pl011 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pl011")
            .field("rx_queue_len", &self.rx_queue.len())
            .field("imsc", &format_args!("{:#x}", self.imsc))
            .field("ris", &format_args!("{:#x}", self.ris))
            .field("intid", &self.intid.as_raw())
            .finish_non_exhaustive()
    }
}

impl BusDevice for Pl011 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        // PL011 silicon supports byte, halfword, word, and (on AArch64
        // implementations) doubleword reads of the 32-bit registers —
        // the upper bytes are zero-extended. Match that behaviour: read
        // the 32-bit register and zero-fill anything beyond.
        let v = self.read_register(offset);
        let bytes = v.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            *b = bytes.get(i).copied().unwrap_or(0);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        // Accept 1/2/4/8-byte writes; only the bottom 32 bits land in
        // the register. Hand-coded aarch64 stubs commonly use 64-bit
        // STRs and Linux's PL011 driver uses 32-bit `writel`; both
        // shapes converge here.
        let mut buf = [0u8; 8];
        let n = data.len().min(8);
        buf[..n].copy_from_slice(&data[..n]);
        let v = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        self.write_register(offset, v);
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn debug_label(&self) -> &str {
        "PL011"
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use squib_gic::GicError;

    use super::*;

    #[derive(Debug, Default)]
    struct StubGic {
        levels: Mutex<Vec<(u32, bool)>>,
    }

    impl Gic for StubGic {
        fn pulse_spi(&self, _: IntId) -> Result<(), GicError> {
            Ok(())
        }
        fn set_spi_level(&self, intid: IntId, level: bool) -> Result<(), GicError> {
            self.levels.lock().push((intid.as_raw(), level));
            Ok(())
        }
        fn save_state(&self) -> Result<Vec<u8>, GicError> {
            Ok(Vec::new())
        }
        fn restore_state(&self, _data: &[u8]) -> Result<(), GicError> {
            Ok(())
        }
    }

    fn build() -> (Pl011, CapturedSink, Arc<StubGic>) {
        let sink = CapturedSink::new();
        let gic = Arc::new(StubGic::default());
        let pl011 = Pl011::new(
            Box::new(sink.clone()),
            gic.clone() as Arc<dyn Gic + Send + Sync>,
            IntId::from_spi_cell(1).unwrap(), // INTID 33 per D22
        );
        (pl011, sink, gic)
    }

    fn read32(pl011: &mut Pl011, offset: u64) -> u32 {
        let mut buf = [0u8; 4];
        pl011.read(offset, &mut buf);
        u32::from_le_bytes(buf)
    }

    fn write32(pl011: &mut Pl011, offset: u64, value: u32) {
        pl011.write(offset, &value.to_le_bytes());
    }

    #[test]
    fn test_should_expose_pl011_r1p5_periph_pcell_id_for_linux_probe() {
        let (mut pl011, _, _) = build();
        // PeriphID0..3 + PCellID0..3
        assert_eq!(read32(&mut pl011, 0xFE0), 0x11);
        assert_eq!(read32(&mut pl011, 0xFE4), 0x10);
        assert_eq!(read32(&mut pl011, 0xFE8), 0x14);
        assert_eq!(read32(&mut pl011, 0xFEC), 0x00);
        assert_eq!(read32(&mut pl011, 0xFF0), 0x0D);
        assert_eq!(read32(&mut pl011, 0xFF4), 0xF0);
        assert_eq!(read32(&mut pl011, 0xFF8), 0x05);
        assert_eq!(read32(&mut pl011, 0xFFC), 0xB1);
    }

    #[test]
    fn test_should_emit_each_dr_write_to_sink_synchronously() {
        let (mut pl011, sink, _) = build();
        write32(&mut pl011, 0x000, b'H' as u32);
        write32(&mut pl011, 0x000, b'i' as u32);
        write32(&mut pl011, 0x000, b'\n' as u32);
        assert_eq!(sink.snapshot(), b"Hi\n");
    }

    #[test]
    fn test_should_report_rx_empty_until_push_rx() {
        let (mut pl011, _, _) = build();
        let fr = read32(&mut pl011, 0x018);
        assert!(fr & FR_RX_EMPTY != 0);
        pl011.push_rx(b'A');
        let fr = read32(&mut pl011, 0x018);
        assert!(fr & FR_RX_EMPTY == 0);
    }

    #[test]
    fn test_should_drain_rx_byte_via_dr_read() {
        let (mut pl011, _, _) = build();
        pl011.push_rx(b'X');
        let v = read32(&mut pl011, 0x000);
        assert_eq!(v as u8, b'X');
        // RX is empty again.
        let fr = read32(&mut pl011, 0x018);
        assert!(fr & FR_RX_EMPTY != 0);
    }

    #[test]
    fn test_should_drive_gic_line_when_imsc_rx_set_and_data_arrives() {
        let (mut pl011, _, gic) = build();
        // Driver unmasks RX.
        write32(&mut pl011, 0x038, IMSC_RX);
        // Push an RX byte.
        pl011.push_rx(b'Q');
        let levels = gic.levels.lock().clone();
        assert!(
            levels.iter().any(|(intid, lvl)| *intid == 33 && *lvl),
            "expected an intid=33 level=true, got {levels:?}"
        );
    }

    #[test]
    fn test_should_drop_gic_line_after_dr_drains_last_byte() {
        let (mut pl011, _, gic) = build();
        write32(&mut pl011, 0x038, IMSC_RX);
        pl011.push_rx(b'Z');
        gic.levels.lock().clear();
        let _ = read32(&mut pl011, 0x000);
        let levels = gic.levels.lock().clone();
        assert!(
            levels.iter().any(|(intid, lvl)| *intid == 33 && !*lvl),
            "expected an intid=33 level=false, got {levels:?}"
        );
    }

    #[test]
    fn test_should_clear_ris_bit_on_icr_write() {
        let (mut pl011, _, _) = build();
        write32(&mut pl011, 0x038, IMSC_RX);
        pl011.push_rx(b'!');
        // RIS has RX bit.
        let ris = read32(&mut pl011, 0x03C);
        assert!(ris & INT_RX != 0);
        // ICR clears it.
        write32(&mut pl011, 0x044, INT_RX);
        let ris = read32(&mut pl011, 0x03C);
        assert!(ris & INT_RX == 0);
    }

    #[test]
    fn test_should_reflect_baud_and_line_control_writes() {
        let (mut pl011, _, _) = build();
        write32(&mut pl011, 0x024, 26); // IBRD
        write32(&mut pl011, 0x028, 3); // FBRD
        write32(&mut pl011, 0x02C, 0x70); // LCRH = 8N1
        write32(&mut pl011, 0x030, 0x301); // CR = enable + tx + rx
        assert_eq!(read32(&mut pl011, 0x024), 26);
        assert_eq!(read32(&mut pl011, 0x028), 3);
        assert_eq!(read32(&mut pl011, 0x02C), 0x70);
        assert_eq!(read32(&mut pl011, 0x030), 0x301);
    }

    #[test]
    fn test_should_zero_fill_short_reads_without_panicking() {
        let (mut pl011, _, _) = build();
        let mut byte = [0xAAu8; 1];
        pl011.read(0x000, &mut byte);
        assert_eq!(byte, [0u8; 1]);
    }

    #[test]
    fn test_mis_combines_ris_with_imsc() {
        let (mut pl011, _, _) = build();
        pl011.push_rx(b'M');
        // Driver hasn't unmasked yet → MIS = 0.
        assert_eq!(read32(&mut pl011, 0x040), 0);
        write32(&mut pl011, 0x038, IMSC_RX);
        let mis = read32(&mut pl011, 0x040);
        assert_eq!(mis & INT_RX, INT_RX);
    }

    /// Test-only writer that pushes into a shared `Vec<u8>` so we can
    /// assert what `WriterSink` forwarded.
    struct CapWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for CapWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_writer_sink_passes_bytes_to_underlying_writer() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = WriterSink::new(Box::new(CapWriter(buf.clone())));
        let gic: Arc<dyn Gic + Send + Sync> = Arc::new(StubGic::default());
        let mut pl011 = Pl011::new(Box::new(sink), gic, IntId::from_spi_cell(1).unwrap());
        write32(&mut pl011, 0x000, u32::from(b'O'));
        write32(&mut pl011, 0x000, u32::from(b'K'));
        assert_eq!(buf.lock().as_slice(), b"OK");
    }
}
