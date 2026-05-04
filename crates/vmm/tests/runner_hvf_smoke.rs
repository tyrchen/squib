//! Real-HVF integration test for the run-loop driver.
//!
//! Boots a hand-coded aarch64 stub program that:
//!
//! 1. Writes `O`, `K`, `\n` to PL011 `DR` (0x0E0A_0000).
//! 2. Issues `HVC #0` with X0 = `PSCI_SYSTEM_OFF` (0x8400_0008).
//!
//! Verifies the host-side runner:
//! - Spawned the vCPU thread and set boot regs.
//! - Dispatched the three MMIO writes to the bus → PL011 `DR` → captured-bytes sink.
//! - Decoded the HVC as PSCI `SYSTEM_OFF` and shut the VM down cleanly.
//!
//! Gated on the `make hvf-test` target because real HVF requires the
//! `com.apple.security.hypervisor` entitlement.

#![cfg(target_os = "macos")]
// One-shot test: writes a tiny stub kernel image, boots, captures output,
// removes the file. Sync `std::fs::*` / `as u16` casts are fine here.
#![allow(
    clippy::doc_markdown,
    clippy::cast_lossless,
    clippy::vec_init_then_push,
    clippy::disallowed_methods,
    clippy::uninlined_format_args
)]

use std::sync::Arc;

use parking_lot::Mutex;
use squib_arch::{IntId, layout};
use squib_bus::{BusBuilder, BusDevice};
use squib_gic::{Gic, GicError, GicSizes};
use squib_legacy::Pl011;
use squib_vmm::{
    BootArtifacts, build_microvm_for_boot,
    runner::{ShutdownReason, run_microvm},
};

/// Physical entry address — 0x80200000 (DRAM_BASE + KERNEL_LOAD_OFFSET).
const ENTRY_PC: u64 = 0x8020_0000;

/// Captured-bytes sink the test asserts against.
#[derive(Debug, Clone, Default)]
struct CapturedSink(Arc<Mutex<Vec<u8>>>);

impl squib_legacy::Pl011Sink for CapturedSink {
    fn write_byte(&mut self, byte: u8) {
        self.0.lock().push(byte);
    }
}

/// Stub GIC for the PL011 — real HVF GIC integration isn't needed for
/// this test (PL011's IRQ line is unused without RX traffic).
#[derive(Debug, Default)]
struct StubGic;

impl Gic for StubGic {
    fn pulse_spi(&self, _intid: IntId) -> Result<(), GicError> {
        Ok(())
    }
    fn set_spi_level(&self, _intid: IntId, _level: bool) -> Result<(), GicError> {
        Ok(())
    }
    fn save_state(&self) -> Result<Vec<u8>, GicError> {
        Ok(Vec::new())
    }
    fn restore_state(&self, _data: &[u8]) -> Result<(), GicError> {
        Ok(())
    }
}

/// Build the aarch64 stub program. Returns a `Vec<u8>` ready to be
/// loaded into guest memory at `kernel_load_addr`.
///
/// Pseudo-assembly:
/// ```text
///   movz x0, #0x0000          ; X0 = 0x0000_0000_0000_0000
///   movk x0, #0x0E0A, lsl #16 ; X0 = 0x0000_0000_0E0A_0000  (PL011 base)
///   movz x1, #'O'             ; X1 = 0x4F
///   str  x1, [x0]
///   movz x1, #'K'             ; X1 = 0x4B
///   str  x1, [x0]
///   movz x1, #0x000A          ; X1 = '\n'
///   str  x1, [x0]
///   movz x0, #0x0008          ; X0 = 0x0000_0000_0000_0008
///   movk x0, #0x8400, lsl #16 ; X0 = 0x0000_0000_8400_0008  (PSCI SYSTEM_OFF)
///   hvc  #0
/// 1: b    1b                  ; spin if HVC unexpectedly returns
/// ```
fn build_stub_program() -> Vec<u8> {
    let mut prog: Vec<u32> = Vec::new();
    prog.push(movz_x(0, 0x0000, 0));
    prog.push(movk_x(0, 0x0E0A, 16));
    prog.push(movz_x(1, b'O' as u16, 0));
    prog.push(str_x_xn(1, 0));
    prog.push(movz_x(1, b'K' as u16, 0));
    prog.push(str_x_xn(1, 0));
    prog.push(movz_x(1, 0x000A, 0));
    prog.push(str_x_xn(1, 0));
    prog.push(movz_x(0, 0x0008, 0));
    prog.push(movk_x(0, 0x8400, 16));
    prog.push(hvc(0));
    prog.push(b_back(0)); // spin (offset 0 = self)
    let mut bytes = Vec::with_capacity(prog.len() * 4);
    for word in prog {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes
}

/// `MOVZ Xd, #imm16, LSL #(hw*16)` — wide-zero move.
/// Encoding: `1 10 100101 hw(2) imm16(16) Rd(5)` (sf=1 for X regs).
fn movz_x(rd: u8, imm16: u16, shift: u8) -> u32 {
    let hw = u32::from(shift / 16);
    0xD280_0000 | (hw << 21) | (u32::from(imm16) << 5) | u32::from(rd)
}

/// `MOVK Xd, #imm16, LSL #(hw*16)` — wide-keep move.
/// Encoding: `1 11 100101 hw(2) imm16(16) Rd(5)`.
fn movk_x(rd: u8, imm16: u16, shift: u8) -> u32 {
    let hw = u32::from(shift / 16);
    0xF280_0000 | (hw << 21) | (u32::from(imm16) << 5) | u32::from(rd)
}

/// `STR Xt, [Xn, #imm12*8]` — store 64-bit, scaled offset.
/// Encoding: `1 11 11 0 01 00 imm12(12) Rn(5) Rt(5)`.
fn str_x_xn(rt: u8, rn: u8) -> u32 {
    0xF900_0000 | (u32::from(rn) << 5) | u32::from(rt)
}

/// `HVC #imm16` — hypervisor call.
/// Encoding: `1101 0100 000 imm16(16) 000 10`.
fn hvc(imm16: u16) -> u32 {
    0xD400_0002 | (u32::from(imm16) << 5)
}

/// `B label` with a signed 26-bit offset (in instruction units).
/// `offset_words = 0` means branch to self.
fn b_back(offset_words: i32) -> u32 {
    #[allow(clippy::cast_sign_loss)]
    let masked = (offset_words as u32) & 0x03FF_FFFF;
    0x1400_0000 | masked
}

#[test]
#[ignore = "requires HVF entitlement; run via `make hvf-test`"]
fn test_runner_executes_stub_writes_pl011_then_psci_system_off() {
    // 1. Build the boot artifacts: kernel = our stub, no initrd, 64 MiB DRAM, 1 vCPU.
    let stub = build_stub_program();
    let kernel_path = std::env::temp_dir().join("squib-runner-smoke-kernel.bin");
    std::fs::write(&kernel_path, build_aarch64_image_with_stub(&stub)).unwrap();

    let resources = squib_vmm::resources::VmResources {
        vcpu_count: 1,
        mem_size_mib: 64,
        kernel: squib_vmm::KernelSource::Path(kernel_path.clone()),
        initrd: None,
        boot_args: String::new(),
        root_partuuid: None,
        virtio_devices: Vec::new(),
    };
    let _sizes = GicSizes::query().expect("GicSizes::query");
    let boot: BootArtifacts = build_microvm_for_boot(&resources).expect("build_microvm_for_boot");
    assert_eq!(boot.kernel_load_addr, ENTRY_PC);

    // 2. Build a bus with PL011 at its canonical base.
    let mut builder = BusBuilder::new();
    let gic: Arc<dyn Gic + Send + Sync> = Arc::new(StubGic);
    let sink = CapturedSink::default();
    let pl011 = Pl011::new(
        Box::new(sink.clone()),
        gic.clone(),
        IntId::from_spi_cell(1).unwrap(),
    );
    let pl011_dyn: Arc<Mutex<dyn BusDevice>> = Arc::new(Mutex::new(pl011));
    builder
        .insert(pl011_dyn, layout::PL011_BASE, 0x1000)
        .expect("bus insert PL011");
    let bus = builder.build();

    // 3. Take ownership of the HVF VM from the boot artifacts.
    let mut boot = boot;
    let vm = Arc::new(boot.hvf_vm.take().expect("HVF VM present on macOS"));

    // 4. Run.
    let (_handle, result) = run_microvm(boot, vm, bus, None).expect("run_microvm");

    // 5. The stub should have written `OK\n` via PL011 and exited with SYSTEM_OFF.
    assert_eq!(result.reason, ShutdownReason::SystemOff);
    assert!(result.mmio_exits >= 3, "{:?}", result);
    assert_eq!(result.hvc_exits, 1, "{:?}", result);
    assert_eq!(sink.0.lock().as_slice(), b"OK\n");
    let _ = std::fs::remove_file(kernel_path);
}

/// Wrap a flat aarch64 byte stream in a minimal `Image` boot header so
/// `squib-loader` accepts it. Entry is at offset 0 of the image; we
/// place a `B` instruction there that jumps past the header into the
/// stub code (matching the aarch64 `Image` format's "first instruction
/// is a branch to the kernel entry").
///
/// Layout:
/// - bytes 0..4: `B +16` (branch to offset 0x40 = 16 instructions).
/// - bytes 4..8: zero padding (the second word of the boot header).
/// - bytes 8..16: `text_offset` (0x20_0000) — DRAM-relative load offset.
/// - bytes 16..24: `image_size` (placeholder; patched at end).
/// - bytes 0x38..0x3C: `'ARM\x64'` magic — what the loader recognizes.
/// - bytes 0x40..: the stub program.
fn build_aarch64_image_with_stub(stub: &[u8]) -> Vec<u8> {
    use squib_arch::layout::KERNEL_LOAD_OFFSET;
    let mut image = vec![0u8; 64];
    // First instruction: B +0x40 (16 instructions ahead).
    let branch = b_back(16); // offset = 16 instructions
    image[0..4].copy_from_slice(&branch.to_le_bytes());
    // bytes 4..8 stay zero.
    image[8..16].copy_from_slice(&KERNEL_LOAD_OFFSET.to_le_bytes());
    image[16..24].copy_from_slice(&0u64.to_le_bytes());
    image[0x38..0x3C].copy_from_slice(b"ARM\x64");
    image.extend_from_slice(stub);
    let total_len = image.len() as u64;
    image[16..24].copy_from_slice(&total_len.to_le_bytes());
    image
}

#[test]
fn test_aarch64_instruction_encoders_are_correct() {
    // MOVZ X0, #0xABCD: 0xD280_0000 | (0xABCD << 5) | 0
    //                = 0xD280_0000 | 0x0015_79A0 = 0xD295_79A0.
    assert_eq!(movz_x(0, 0xABCD, 0), 0xD295_79A0);
    // MOVK X0, #0x1234, LSL #16: 0xF280_0000 | (1<<21) | (0x1234<<5)
    //                         = 0xF2A0_0000 | 0x0002_4680 = 0xF2A2_4680.
    assert_eq!(movk_x(0, 0x1234, 16), 0xF2A2_4680);
    // STR X1, [X0] → 0xF900_0001.
    assert_eq!(str_x_xn(1, 0), 0xF900_0001);
    // HVC #0 → 0xD400_0002.
    assert_eq!(hvc(0), 0xD400_0002);
    // B (offset 0) → 0x1400_0000.
    assert_eq!(b_back(0), 0x1400_0000);
}

#[test]
fn test_stub_program_is_well_formed() {
    let stub = build_stub_program();
    // 12 instructions × 4 bytes.
    assert_eq!(stub.len(), 12 * 4);
    // First instruction is MOVZ X0, #0 (= 0xD280_0000).
    let first = u32::from_le_bytes([stub[0], stub[1], stub[2], stub[3]]);
    assert_eq!(first, 0xD280_0000);
}
