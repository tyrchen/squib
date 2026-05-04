//! Apple Hypervisor.framework binding for squib.
//!
//! `squib-hv` is **the** unsafe boundary in the workspace alongside `squib-net::sys`
//! (per [12-hvf-backend.md § 1](../../../specs/12-hvf-backend.md)). Every other crate
//! carries `#![forbid(unsafe_code)]`. The applevisor crate (`applevisor = "1.0"`,
//! `default-features = false`, `features = ["macos-15-0"]`) already wraps the raw HVF
//! C API in safe Rust, so this crate at present holds no `unsafe` blocks of its own —
//! the safety boundary is expressed structurally, not by reaching into raw syscalls.
//!
//! # Modules
//!
//! - [`vmm`] — the [`HvfHypervisor`] singleton initialiser and the [`HvfVm`] handle.
//! - [`vcpu`] — [`HvfVcpu`] with the thread-affinity check and IRQ shadow bitset (per-vCPU
//!   `Box<[AtomicU64]>` per [71-performance-budgets.md § 4]).
//! - [`run_loop`] — the vCPU run-loop dispatcher that translates HVF exits into the portable
//!   [`VmExit`](squib_core::VmExit) algebra.
//! - [`irq`] — [`IrqShadow`] bitset implementation; pulled out for unit-testability and so devices
//!   that need to inject interrupts have a stable, lock-free target.

#![cfg_attr(target_os = "macos", deny(unsafe_op_in_unsafe_fn))]
#![warn(missing_docs)]
// `Arc<Mutex<applevisor::Memory>>` is the device-side handle to a
// guest memory region. The unsafe `impl Send + Sync` we add for
// `HvfGuestMemory` makes the wrapper thread-safe with documented
// invariants; clippy's `arc_with_non_send_sync` lint trips because
// `Memory` itself is `!Send` (raw `*const c_void` host pointer). The
// rationale is the same as the `Send`/`Sync` impls on `HvfVm` —
// see `vmm.rs` SAFETY block.
#![allow(clippy::arc_with_non_send_sync)]
// Hardware names (HVF, ESR_EL2, MMIO, etc.) and applevisor symbol names are common in
// our docs; backticking each adds noise.
#![allow(clippy::doc_markdown)]

pub mod irq;
pub mod run_loop;
pub mod vcpu;
pub mod vmm;

pub use irq::{IrqShadow, MAX_TRACKED_INTID};
pub use run_loop::{Exit, RunLoopDispatch, decode_exception};
pub use vcpu::{HvfVcpu, ThreadAffinityError};
pub use vmm::{HvfGuestMemory, HvfHypervisor, HvfVm, InitError, MappedRegion};
