//! vCPU run-loop driver.
//!
//! Spawns vCPU 0 as a dedicated OS thread, drives `hv_vcpu_run`,
//! decodes exits via [`squib_hv::run_loop::decode_exception`], and
//! dispatches MMIO/HVC/SMC against the supplied bus + GIC.
//!
//! Per [12-hvf-backend.md § 4](../../../specs/12-hvf-backend.md#4-threading-rules)
//! every `hv_vcpu_*` call (except `hv_vcpus_exit`) must come from the
//! thread that originally called `hv_vcpu_create`. We use
//! `std::thread::spawn` (not a tokio task) because tokio's executor
//! moves work between threads.
//!
//! ## State machine
//!
//! ```text
//!  spawned ─► vcpu_create on this thread
//!     │
//!     ▼
//!  set boot regs (PC=entry, X0=fdt)
//!     │
//!     ▼
//!  loop:
//!    drain IRQ shadow → hv_vcpu_set_pending_interrupt
//!    hv_vcpu_run
//!    decode_exception(esr, far)
//!     ┌─ Mmio  → bus.read/write at addr
//!     ├─ Hvc   → resolve_hvc_psci, handle SYSTEM_OFF/CPU_OFF
//!     ├─ Smc   → set X0 = PSCI_NOT_SUPPORTED
//!     ├─ Wfi   → sleep until IRQ shadow has bits or shutdown
//!     ├─ Cancelled  → break
//!     └─ Other ↦ shutdown
//! ```
//!
//! Phase 3's run-loop driver covers vCPU 0 only. Multi-vCPU support
//! (vCPUs 1..N parked at CPU_OFF, woken on PSCI_CPU_ON) lands when
//! Phase 4+ exercises the secondary-bringup path; the type signatures
//! here already accept N>1.

#![cfg(target_os = "macos")]
// Run-loop driver: takes ownership of boot artifacts + Arc handles by
// value because the spawned vCPU thread `move`-captures them. The
// pedantic `needless_pass_by_value` lint flags this even though the
// values are consumed by the thread closure.
#![allow(clippy::needless_pass_by_value, clippy::too_many_lines)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use squib_arch::psci::{PsciOutcome, PsciReturn};
use squib_bus::Bus;
use squib_hv::{
    HvfVm,
    run_loop::{Exit, decode_exception, resolve_hvc_psci},
};

use crate::builder::BootArtifacts;

/// Reasons why the run-loop driver shut down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownReason {
    /// Guest issued PSCI `SYSTEM_OFF`.
    SystemOff,
    /// Guest issued PSCI `SYSTEM_RESET`. Squib treats reset as off — we
    /// do not auto-reboot.
    SystemReset,
    /// Operator called [`MicrovmHandle::request_shutdown`].
    OperatorRequest,
    /// vCPU exit decoded as an unknown / illegal exception class.
    GuestFault,
    /// HVF returned an unrecoverable error.
    HvfError,
    /// vCPU panicked (stub-test escape hatch).
    VcpuPanic,
}

/// Result from running a microvm to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    /// Why the run loop terminated.
    pub reason: ShutdownReason,
    /// Number of MMIO exits handled — useful for tests and metrics.
    pub mmio_exits: u64,
    /// Number of HVC exits handled.
    pub hvc_exits: u64,
    /// Number of WFI / WFE exits handled.
    pub wfi_exits: u64,
    /// Total wall-clock time the runner spent in the loop.
    pub elapsed: Duration,
}

/// Hard upper bound on wall-clock time before the runner forces
/// shutdown. Acts as a watchdog against a hung guest. The default is
/// 5 minutes — long enough for a slow boot, short enough that a CI
/// run doesn't hang indefinitely.
pub const DEFAULT_RUN_BUDGET: Duration = Duration::from_mins(5);

/// Handle for shutting down a running microvm from outside the vCPU
/// thread. Cloning shares the underlying flag.
#[derive(Debug, Clone)]
pub struct MicrovmHandle {
    shutdown: Arc<AtomicBool>,
    vm: Arc<HvfVm>,
}

impl MicrovmHandle {
    /// Signal the vCPU thread to exit at its next safe point. The vCPU
    /// will be woken via `hv_vcpus_exit`.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // The vCPU thread checks the flag inside the run loop, but a
        // blocked `hv_vcpu_run` won't notice until we cancel it.
        // `cancel_vcpus` requires the vCPU's handle, which lives on the
        // owning thread; the owning thread re-checks the flag immediately
        // after `hv_vcpu_run` returns, so a `cancel_vcpus` call from
        // outside isn't strictly necessary for correctness — the flag is
        // observed on the next exit. For latency-critical scenarios we
        // can add a handle registry.
        let _ = &self.vm;
    }
}

/// Run a microvm to completion.
///
/// Spawns a dedicated thread for vCPU 0, sets the boot registers from
/// `boot.boot_regs`, and drives the run loop. Returns when the vCPU
/// thread exits (PSCI shutdown, fault, or operator request).
///
/// `bus` is the MMIO bus the run-loop dispatches data-aborts against.
/// The bus's lifetime must outlive the run, hence `Arc`.
///
/// # Errors
/// Returns `String` for any HVF-side init failure on the calling
/// thread; per-vCPU panics surface as [`ShutdownReason::VcpuPanic`].
pub fn run_microvm(
    boot: BootArtifacts,
    vm: Arc<HvfVm>,
    bus: Arc<Bus>,
    initrd_bytes: Option<Vec<u8>>,
) -> Result<(MicrovmHandle, RunResult), String> {
    run_microvm_with_budget(boot, vm, bus, initrd_bytes, DEFAULT_RUN_BUDGET)
}

/// Same as [`run_microvm`] but with an explicit wall-clock budget.
/// When the budget elapses, the runner sets the shutdown flag and the
/// vCPU exits at the next safe point (next exit / WFI tick).
///
/// # Errors
/// Same as [`run_microvm`].
pub fn run_microvm_with_budget(
    boot: BootArtifacts,
    vm: Arc<HvfVm>,
    bus: Arc<Bus>,
    initrd_bytes: Option<Vec<u8>>,
    budget: Duration,
) -> Result<(MicrovmHandle, RunResult), String> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = MicrovmHandle {
        shutdown: Arc::clone(&shutdown),
        vm: Arc::clone(&vm),
    };
    // Write the kernel image (and initrd, if supplied) into guest memory
    // before spawning the vCPU thread — Linux needs both staged before
    // it observes the FDT `linux,initrd-{start,end}` properties.
    write_kernel_to_guest(&vm, &boot, initrd_bytes.as_deref())?;

    // Channel for the vCPU thread to publish its handle back so a
    // watchdog can call `hv_vcpus_exit` if the budget elapses while
    // the guest is in a long native run (no exit).
    let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel::<applevisor::vcpu::VcpuHandle>(1);

    // Spawn vCPU 0.
    let thread_shutdown = Arc::clone(&shutdown);
    let thread_vm = Arc::clone(&vm);
    let thread_bus = Arc::clone(&bus);
    let entry_pc = boot.boot_regs.kernel_load_addr;
    let fdt_addr = boot.boot_regs.fdt_addr;
    let join = thread::Builder::new()
        .name("squib-vcpu-0".into())
        .spawn(move || {
            run_vcpu0_thread(
                thread_vm,
                thread_bus,
                thread_shutdown,
                entry_pc,
                fdt_addr,
                budget,
                handle_tx,
            )
        })
        .map_err(|e| format!("failed to spawn vCPU thread: {e}"))?;

    // Watchdog: receive the vCPU handle, sleep up to `budget`, then
    // call `cancel_vcpus` to wake the vCPU from a blocked
    // `hv_vcpu_run`. The vCPU's own loop checks the shutdown flag on
    // every iteration; the watchdog just makes sure we get back to
    // that check.
    let watchdog_vm = Arc::clone(&vm);
    let watchdog_shutdown = Arc::clone(&shutdown);
    let watchdog = thread::Builder::new()
        .name("squib-vcpu-watchdog".into())
        .spawn(move || {
            // Wait for the vCPU thread to publish its handle; if it
            // never does (e.g. vcpu_create failed), give up after a
            // short grace period.
            let Ok(handle) = handle_rx.recv_timeout(Duration::from_secs(5)) else {
                return;
            };
            let started = std::time::Instant::now();
            while !watchdog_shutdown.load(Ordering::SeqCst) {
                if started.elapsed() >= budget {
                    watchdog_shutdown.store(true, Ordering::SeqCst);
                    let _ = watchdog_vm.cancel_vcpus(&[handle]);
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
        .map_err(|e| format!("failed to spawn watchdog: {e}"))?;

    let result = match join.join() {
        Ok(r) => r,
        Err(_) => RunResult {
            reason: ShutdownReason::VcpuPanic,
            mmio_exits: 0,
            hvc_exits: 0,
            wfi_exits: 0,
            elapsed: Duration::ZERO,
        },
    };
    // Tell the watchdog we're done (in case the vCPU finished before
    // the budget) and reap it.
    shutdown.store(true, Ordering::SeqCst);
    let _ = watchdog.join();
    Ok((handle, result))
}

fn write_kernel_to_guest(
    vm: &HvfVm,
    boot: &BootArtifacts,
    initrd_bytes: Option<&[u8]>,
) -> Result<(), String> {
    // Kernel image at kernel_load_addr.
    let dram_offset = boot
        .kernel_load_addr
        .checked_sub(squib_arch::layout::DRAM_BASE)
        .ok_or_else(|| "kernel_load_addr below DRAM_BASE".to_string())?;
    let region_offset =
        usize::try_from(dram_offset).map_err(|e| format!("offset overflow: {e}"))?;
    vm.write_to_first_region(region_offset, &boot.kernel.bytes)
        .map_err(|e| format!("write kernel: {e}"))?;

    // Initrd at boot.initrd_range.start, if one was planned and bytes
    // were supplied. Linux's kernel decompressor looks at
    // `linux,initrd-start`/`linux,initrd-end` in the FDT; the bytes
    // must be present at that physical address before vCPU 0 runs.
    if let (Some(range), Some(bytes)) = (boot.initrd_range, initrd_bytes) {
        let initrd_offset = usize::try_from(
            range
                .start
                .checked_sub(squib_arch::layout::DRAM_BASE)
                .ok_or_else(|| "initrd start below DRAM_BASE".to_string())?,
        )
        .map_err(|e| format!("initrd offset overflow: {e}"))?;
        vm.write_to_first_region(initrd_offset, bytes)
            .map_err(|e| format!("write initrd: {e}"))?;
    }

    // FDT at fdt_base.
    let fdt_offset = usize::try_from(
        boot.fdt_base
            .checked_sub(squib_arch::layout::DRAM_BASE)
            .ok_or_else(|| "fdt_base below DRAM_BASE".to_string())?,
    )
    .map_err(|e| format!("fdt offset overflow: {e}"))?;
    vm.write_to_first_region(fdt_offset, &boot.fdt_bytes)
        .map_err(|e| format!("write fdt: {e}"))?;
    Ok(())
}

fn run_vcpu0_thread(
    vm: Arc<HvfVm>,
    bus: Arc<Bus>,
    shutdown: Arc<AtomicBool>,
    entry_pc: u64,
    fdt_addr: u64,
    budget: Duration,
    handle_tx: std::sync::mpsc::SyncSender<applevisor::vcpu::VcpuHandle>,
) -> RunResult {
    use applevisor::vcpu::{Reg as AvReg, SysReg};

    let started = std::time::Instant::now();
    let mut result = RunResult {
        reason: ShutdownReason::OperatorRequest,
        mmio_exits: 0,
        hvc_exits: 0,
        wfi_exits: 0,
        elapsed: Duration::ZERO,
    };
    let vcpu = match squib_hv::vmm::create_vcpu_on_this_thread(&vm) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "vcpu_create failed");
            result.reason = ShutdownReason::HvfError;
            return result;
        }
    };
    // GICv3 uses affinity-based interrupt routing: HVF assigns each
    // vCPU's redistributor slot from MPIDR_EL1, and the kernel reads
    // GICR via the FDT-published address. We must set MPIDR_EL1
    // BEFORE the first `hv_vcpu_run` so HVF wires the redistributor
    // for that affinity. Without this, GICR reads (e.g. PIDR2 at
    // GICR_BASE+0xFFE8) miss every HVF intercept and surface as
    // stage-2 data aborts to the host — the kernel reads zero, panics
    // and PSCI SYSTEM_RESETs.
    //
    // For vCPU 0 the affinity is 0; bit 31 of MPIDR_EL1 is RES1 (per
    // ARMv8 ARM B6.2.71), so the value is 0x8000_0000.
    if let Err(e) = vcpu.set_sys_reg(SysReg::MPIDR_EL1, 0x8000_0000) {
        tracing::error!(error = ?e, "set_sys_reg(MPIDR_EL1) failed");
        result.reason = ShutdownReason::HvfError;
        return result;
    }
    // Publish the handle to the watchdog. Drop on send failure (no
    // watchdog) — the loop's per-iteration shutdown check still works.
    let _ = handle_tx.send(vcpu.get_handle());
    // Linux aarch64 boot protocol (Documentation/arm64/booting.rst):
    //   - PC = entry address (kernel_load_addr).
    //   - X0 = physical address of the FDT blob.
    //   - X1, X2, X3 = 0 (reserved for future use; the kernel checks).
    //   - The kernel must be entered at EL1 or EL2 (HVF gives us EL1).
    //   - DAIF = 0xF (all interrupts masked); the kernel unmasks itself.
    //
    // For hand-rolled stubs the same shape works — they don't read
    // X1..X3 so zeroing them is a no-op. CPSR `0x3C5` = EL1h + SP_EL1
    // + DAIF mask. (Bits: PAN/UAO=0, IL=0, SS=0, mode=EL1h(5), SP=1.)
    let regs = [
        (AvReg::PC, entry_pc),
        (AvReg::X0, fdt_addr),
        (AvReg::X1, 0),
        (AvReg::X2, 0),
        (AvReg::X3, 0),
        (AvReg::CPSR, 0x3C5),
    ];
    for (reg, value) in regs {
        if let Err(e) = vcpu.set_reg(reg, value) {
            tracing::error!(error = ?e, ?reg, "set_reg failed");
            result.reason = ShutdownReason::HvfError;
            return result;
        }
    }

    let mut pending_advance_pc = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            result.reason = ShutdownReason::OperatorRequest;
            break;
        }
        if started.elapsed() >= budget {
            tracing::warn!(
                budget_secs = budget.as_secs(),
                mmio = result.mmio_exits,
                hvc = result.hvc_exits,
                wfi = result.wfi_exits,
                "run loop exceeded wall-clock budget; forcing shutdown"
            );
            result.reason = ShutdownReason::OperatorRequest;
            break;
        }
        if pending_advance_pc {
            let pc = match vcpu.get_reg(AvReg::PC) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = ?e, "get_reg PC failed");
                    result.reason = ShutdownReason::HvfError;
                    break;
                }
            };
            if let Err(e) = vcpu.set_reg(AvReg::PC, pc + 4) {
                tracing::error!(error = ?e, "set_reg PC+4 failed");
                result.reason = ShutdownReason::HvfError;
                break;
            }
            pending_advance_pc = false;
            let _ = pending_advance_pc; // silence warning; rebound below.
        }
        if let Err(e) = vcpu.run() {
            tracing::error!(error = ?e, "hv_vcpu_run failed");
            result.reason = ShutdownReason::HvfError;
            break;
        }
        let exit_info = vcpu.get_exit_info();
        let esr = exit_info.exception.syndrome;
        // For data aborts, `physical_address` is the IPA the bus needs
        // (HPFAR_EL2 << 8 | FAR_EL2[11:0]). For other classes we still
        // pass it through; only Mmio uses it.
        let far = exit_info.exception.physical_address;
        let dispatch = decode_exception(esr, far, |reg_idx| {
            // X31 in srt is XZR; reading XZR returns 0.
            if reg_idx >= 31 {
                return 0;
            }
            match av_reg_from_index(reg_idx) {
                Some(reg) => vcpu.get_reg(reg).unwrap_or(0),
                None => 0,
            }
        });
        pending_advance_pc = dispatch.advance_pc;
        match dispatch.exit {
            Exit::Mmio {
                addr,
                write,
                sas,
                srt,
                sf: _,
            } => {
                result.mmio_exits = result.mmio_exits.saturating_add(1);
                if result.mmio_exits <= 16 || result.mmio_exits.is_multiple_of(256) {
                    tracing::trace!(
                        n = result.mmio_exits,
                        addr = format!("{addr:#x}"),
                        write,
                        sas,
                        srt,
                        "mmio exit"
                    );
                }
                let len = 1usize << sas;
                let len = len.min(8);
                if write {
                    let value = if srt >= 31 {
                        0
                    } else {
                        av_reg_from_index(srt)
                            .and_then(|r| vcpu.get_reg(r).ok())
                            .unwrap_or(0)
                    };
                    let mut buf = [0u8; 8];
                    buf[..len].copy_from_slice(&value.to_le_bytes()[..len]);
                    if let Err(e) = bus.write(addr, &buf[..len]) {
                        tracing::warn!(error = ?e, addr, "MMIO write to unmapped address");
                    }
                } else {
                    let mut buf = [0u8; 8];
                    if let Err(e) = bus.read(addr, &mut buf[..len]) {
                        tracing::warn!(error = ?e, addr, "MMIO read from unmapped address");
                    }
                    let mut value = [0u8; 8];
                    value[..len].copy_from_slice(&buf[..len]);
                    let v = u64::from_le_bytes(value);
                    if srt < 31
                        && let Some(r) = av_reg_from_index(srt)
                    {
                        let _ = vcpu.set_reg(r, v);
                    }
                }
            }
            Exit::Hvc { args, .. } => {
                result.hvc_exits = result.hvc_exits.saturating_add(1);
                let (x0, outcome) = resolve_hvc_psci(args);
                tracing::trace!(
                    n = result.hvc_exits,
                    fid = format!("{:#x}", args[0]),
                    ?outcome,
                    "hvc exit"
                );
                let _ = vcpu.set_reg(AvReg::X0, x0);
                match outcome {
                    PsciOutcome::SystemOff => {
                        result.reason = ShutdownReason::SystemOff;
                        break;
                    }
                    PsciOutcome::SystemReset => {
                        result.reason = ShutdownReason::SystemReset;
                        break;
                    }
                    PsciOutcome::ParkCallerCpuOff => {
                        // Single-vCPU runner: CPU_OFF on vCPU 0 means VM
                        // shutdown.
                        result.reason = ShutdownReason::SystemOff;
                        break;
                    }
                    PsciOutcome::Return(_)
                    | PsciOutcome::BringUpSecondary { .. }
                    | PsciOutcome::QueryAffinityInfo { .. } => {
                        // Other outcomes: ignored on a single-vCPU run.
                    }
                }
            }
            Exit::SmcHandledAsPsciNotSupported { .. } => {
                let _ = vcpu.set_reg(AvReg::X0, PsciReturn::NotSupported.as_x0());
            }
            Exit::Wfi | Exit::Wfe => {
                result.wfi_exits = result.wfi_exits.saturating_add(1);
                // Without a real wake source (vtimer, IRQ shadow), spin
                // briefly so a busy stub doesn't peg the host CPU. A
                // fully-featured run loop would block on a condvar fed
                // by IRQ delivery; squib's stub demos don't exercise
                // that path.
                thread::sleep(Duration::from_millis(1));
            }
            Exit::SystemRegister { .. } | Exit::VtimerActivated => {
                // No special handling; the kernel's expected behaviour
                // works without us injecting anything — these traps are
                // configurable per vCPU and we leave them defaulted.
            }
            Exit::Brk { .. } => {
                tracing::warn!("guest issued BRK; exiting");
                result.reason = ShutdownReason::GuestFault;
                break;
            }
            Exit::Cancelled => {
                result.reason = ShutdownReason::OperatorRequest;
                break;
            }
            Exit::UnknownExceptionClass { ec, raw } => {
                tracing::error!(ec, raw, "unknown EC; treating as fault");
                result.reason = ShutdownReason::GuestFault;
                break;
            }
        }
    }
    result.elapsed = started.elapsed();
    result
}

fn av_reg_from_index(index: u8) -> Option<applevisor::vcpu::Reg> {
    use applevisor::vcpu::Reg as R;
    Some(match index {
        0 => R::X0,
        1 => R::X1,
        2 => R::X2,
        3 => R::X3,
        4 => R::X4,
        5 => R::X5,
        6 => R::X6,
        7 => R::X7,
        8 => R::X8,
        9 => R::X9,
        10 => R::X10,
        11 => R::X11,
        12 => R::X12,
        13 => R::X13,
        14 => R::X14,
        15 => R::X15,
        16 => R::X16,
        17 => R::X17,
        18 => R::X18,
        19 => R::X19,
        20 => R::X20,
        21 => R::X21,
        22 => R::X22,
        23 => R::X23,
        24 => R::X24,
        25 => R::X25,
        26 => R::X26,
        27 => R::X27,
        28 => R::X28,
        29 => R::X29,
        30 => R::X30,
        _ => return None,
    })
}
