//! aarch64 architecture support for squib.
//!
//! `squib-arch` is the single source of truth for every architecture-specific constant
//! and protocol the boot path depends on. It is consumed by the HVF backend
//! (`squib-hv`), the FDT builder (`squib-fdt`), the kernel loader (`squib-loader`), and
//! the VMM boot orchestrator (`squib-vmm`). If a constant is in this crate, no other
//! crate is allowed to invent it.
//!
//! See [13-arch-and-boot.md](../../../specs/13-arch-and-boot.md) for the full design.
//!
//! # Modules
//!
//! - [`layout`] — fixed memory layout (D22) with const overlap-checks and the page geometry.
//! - [`gic`] — [`IntId`] newtype that pins the FDT-cell ↔ raw-INTID mapping.
//! - [`esr`] — ESR_EL2 decoder; never panics on a malformed input.
//! - [`psci`] — PSCI 1.1 dispatch table; unknown IDs return `NOT_SUPPORTED`.
//! - [`regs`] — the `Reg` enum (X0..X30, SP, PC, PSTATE) and `set_boot_regs` helper.
//! - [`sysregs`] — curated sysreg subset (~100 regs we touch).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// aarch64 sysreg / PSCI / GIC / FDT identifiers are upper-case by convention; surrounding
// every one with backticks bloats the docs without adding clarity.
#![allow(clippy::doc_markdown)]

pub mod esr;
pub mod gic;
pub mod layout;
pub mod psci;
pub mod regs;
pub mod sysregs;

pub use esr::{EsrDecoded, decode as decode_esr};
pub use gic::{IntId, IntIdError, Trigger};
pub use layout::{LayoutOverlap, MEMORY_LAYOUT, MemoryLayout, PageGeometry};
pub use psci::{PsciFunction, PsciOutcome, PsciReturn, dispatch as dispatch_psci};
pub use regs::{BOOT_PSTATE, BootRegs, Reg};
pub use sysregs::SysReg;
