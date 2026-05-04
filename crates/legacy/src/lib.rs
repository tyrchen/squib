//! Legacy aarch64 platform devices.
//!
//! Squib's "legacy" devices are the non-virtio peripherals the
//! aarch64-Linux-on-microvm platform expects: a PL011 UART for early
//! console output, ARM Generic Timer (handled in HVF directly), and
//! eventually a power-controller stub for PSCI off / reset.
//!
//! Per [13-arch-and-boot.md § 2](../../../specs/13-arch-and-boot.md#2-memory-layout-concrete)
//! and D22, these live below the virtio-MMIO region:
//!
//! | Address | Device | Notes |
//! |---------|--------|-------|
//! | `0x0E0A_0000` | PL011 UART | INTID 33 (FDT SPI cell 1) |
//! | `0x080A_0000..0x0E0A_0000` | GIC redistributors | (in `squib-gic`) |
//!
//! ## Module layout
//!
//! - [`pl011`] — ARM PL011 UART r1p5 emulation: `BusDevice` impl + sink trait.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// PL011 register layout: casts between u8 / u32 / u64 are part of the
// wire contract (e.g. DR is a u32 register but the meaningful payload
// is one byte). Disable the pedantic truncation lints for the same
// reasons as `squib-virtio` — these are domain-correct.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    // PL011 register names like `TX_FIFO_EMPTY` and `IrDA` show up in
    // docs; backticking each one is noise.
    clippy::doc_markdown
)]

pub mod pl011;

pub use pl011::{Pl011, Pl011Sink};
