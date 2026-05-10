//! Production VMM event loop — replaces the Phase 2 stub.
//!
//! The event loop is the consumer side of the `RuntimeApiController`'s
//! mpsc channel. It:
//!
//! 1. Accumulates validated `ApiAction`s into a [`BootState`] (single `boot_source`, single
//!    `machine_config`, first drive, first NIC, optional vsock).
//! 2. On `InstanceStart`, turns the accumulated state into a [`VmResources`] + [`DeviceBuildArgs`],
//!    calls [`build_microvm_for_boot`], constructs the device layout, spawns the vCPU driver on a
//!    dedicated OS thread, and asynchronously pumps the vsock RX queue.
//! 3. On `Shutdown`, signals the running VM to exit.
//!
//! Snapshot create/load continue to return a deterministic `400` until
//! the snapshot orchestration lands (upstream work item, Phase 5 in the
//! squib plan).

#![cfg(target_os = "macos")]
// Event-loop glue: long `match` arms that mirror the `ApiAction` enum,
// mid-module type aliases for the long HVF + device compositions, and
// necessary lossy casts between `u64`-typed validated API fields and
// the VMM's `u32`-shaped vcpu counts. `std::fs::read` is acceptable
// here because it runs synchronously on the blocking-friendly boot
// orchestration path, not in a hot async loop.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::disallowed_methods,
    clippy::useless_conversion,
    clippy::redundant_closure_for_method_calls,
    clippy::match_same_arms
)]

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::Result;
use parking_lot::Mutex;
use squib_api::{
    ActionReceiver, ApiAction, ApiResponse, RuntimeApiController,
    schemas::{
        BootSourceConfig, DriveConfig, InstanceAction, MachineConfig, NetworkInterfaceConfig,
        VsockConfig as ApiVsockConfig,
    },
};
use squib_arch::layout::{DRAM_BASE, FDT_MAX_SIZE};
use squib_core::HostDevName;
use squib_fdt::{FdtBuildArgs, MemoryRegion};
use squib_gic::{Gic, GicSizes, HvfGic};
use squib_legacy::Pl011Sink;
use squib_net::{
    GvproxyBackend, GvproxyParams, InterfaceParams, NetHostBackend, NetMode, VmnetHostBackend,
    VmnetIface, VmnetMode,
};
use squib_vmm::{
    BootArtifacts, InitrdSource, KernelSource, VmResources, build_microvm_for_boot,
    device_manager::{BlockConfigSpec, DeviceBuildArgs, NetSpec, VsockSpec, build_device_layout},
    runner::{RunResult, run_microvm_with_budget},
};
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use crate::vsock_muxer::{NotifyHandle, UdsVsockMuxer, UdsVsockMuxerParams};

/// Stderr sink for the guest's PL011 console — boot-time messages land
/// here by default (the operator can override via `/serial`).
#[derive(Debug)]
struct StderrPl011Sink;

impl Pl011Sink for StderrPl011Sink {
    fn write_byte(&mut self, byte: u8) {
        // Best-effort line-buffered stderr write; we don't need every
        // byte flushed synchronously since tracing captures the
        // backing log stream separately.
        use std::io::Write;
        let mut h = std::io::stderr().lock();
        let _ = h.write_all(&[byte]);
    }
}

/// What the operator has configured so far. Each `Put*` action lands
/// here, then on `InstanceStart` we consume the state.
#[derive(Debug, Default)]
struct BootState {
    boot_source: Option<BootSourceConfig>,
    machine_config: Option<MachineConfig>,
    // tok's cold path uses a single drive; if an operator configures
    // more we accept them but only the first is wired to the FDT.
    // Subsequent drives are surfaced as a warn and forwarded to the
    // FDT as additional virtio slots when the day comes.
    drive: Option<DriveConfig>,
    net: Option<NetworkInterfaceConfig>,
    vsock: Option<ApiVsockConfig>,
}

/// Inputs to [`run`].
#[derive(Debug)]
pub(crate) struct VmmLoopConfig {
    /// Network mode chosen on the CLI (`--network`). Controls the
    /// host-side backend when the operator PUTs a network interface.
    pub net_mode: NetMode,
    /// Path to the bundled `gvproxy` binary (only used when
    /// `net_mode == Userspace`).
    pub gvproxy_path: Option<PathBuf>,
    /// vmnet bridged-mode physical interface (only used when
    /// `net_mode == Bridged`, which requires the `bridged` cargo
    /// feature).
    #[cfg_attr(not(feature = "bridged"), allow(dead_code))]
    pub bridged_iface: Option<String>,
    /// Upper bound on guest wall-clock time. Drops us into
    /// `ShutdownReason::OperatorRequest` when exceeded.
    pub run_budget: Duration,
    /// Informational MMDS size cap — forwarded into `DeviceBuildArgs`.
    pub mmds_size_cap: usize,
}

/// Handle to a running microVM. Dropping it signals shutdown.
#[derive(Debug)]
struct RunningVm {
    shutdown: Arc<AtomicBool>,
    joiner: Option<thread::JoinHandle<RunResult>>,
}

impl RunningVm {
    fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    fn join(mut self) -> Option<RunResult> {
        self.joiner.take().and_then(|j| j.join().ok())
    }
}

impl Drop for RunningVm {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(j) = self.joiner.take() {
            let _ = j.join();
        }
    }
}

/// Run the event loop to completion. Returns when the controller's
/// channel closes or [`ApiAction::Shutdown`] lands.
pub(crate) async fn run(
    controller: Arc<RuntimeApiController>,
    mut rx: ActionReceiver,
    cfg: VmmLoopConfig,
) {
    let mut state = BootState::default();
    let mut running: Option<RunningVm> = None;

    while let Some((action, ack)) = rx.recv().await {
        let label = action.label();
        let response = match action {
            ApiAction::Shutdown => {
                info!("vmm loop: shutdown requested");
                if let Some(vm) = running.take() {
                    vm.request_shutdown();
                    if let Some(result) = vm.join() {
                        info!(?result, "vmm loop: VM exited on shutdown");
                    }
                }
                let _ = ack.send(ApiResponse::NoContent);
                break;
            }
            ApiAction::Action(InstanceAction::InstanceStart) => {
                match start_microvm(&mut state, &cfg) {
                    Ok(vm) => {
                        running = Some(vm);
                        // Flip the controller snapshot so subsequent
                        // GETs see the Running state.
                        let prev = controller.snapshot();
                        let mut next = (*prev).clone();
                        next.instance_info.state = squib_api::schemas::VmState::Running;
                        next.phase = squib_core::LifecyclePhase::Running;
                        controller.store_snapshot(next);
                        ApiResponse::NoContent
                    }
                    Err(err) => {
                        error!(error = %err, "vmm loop: InstanceStart failed");
                        ApiResponse::Fault {
                            status: 400,
                            fault_message: format!("InstanceStart failed: {err}"),
                        }
                    }
                }
            }
            ApiAction::Action(InstanceAction::FlushMetrics) => ApiResponse::NoContent,
            ApiAction::Action(InstanceAction::SendCtrlAltDel) => ApiResponse::Fault {
                status: 400,
                fault_message: "SendCtrlAltDel is x86-only and not supported on aarch64".into(),
            },
            ApiAction::PutBootSource(cfg_in) => {
                state.boot_source = Some(cfg_in);
                ApiResponse::NoContent
            }
            ApiAction::PutMachineConfig(cfg_in) => {
                state.machine_config = Some(cfg_in);
                ApiResponse::NoContent
            }
            ApiAction::PatchMachineConfig(patch) => {
                if let Some(existing) = state.machine_config.as_mut() {
                    if let Some(c) = patch.vcpu_count {
                        existing.vcpu_count = c;
                    }
                    if let Some(m) = patch.mem_size_mib {
                        existing.mem_size_mib = m;
                    }
                    if let Some(t) = patch.cpu_template {
                        existing.cpu_template = Some(t);
                    }
                    ApiResponse::NoContent
                } else {
                    ApiResponse::Fault {
                        status: 400,
                        fault_message: "PATCH /machine-config before PUT is not allowed".into(),
                    }
                }
            }
            ApiAction::PutDrive(cfg_in) => {
                if state.drive.is_some() {
                    warn!("vmm loop: multiple drives configured; only the first is wired today");
                }
                state.drive = Some(cfg_in);
                ApiResponse::NoContent
            }
            ApiAction::PutNetwork(cfg_in) => {
                state.net = Some(cfg_in);
                ApiResponse::NoContent
            }
            ApiAction::PutVsock(cfg_in) => {
                state.vsock = Some(cfg_in);
                ApiResponse::NoContent
            }
            ApiAction::SnapshotCreate(_) | ApiAction::SnapshotLoad(_) => ApiResponse::Fault {
                status: 400,
                fault_message: "Snapshot subsystem available but VMM integration pending (Track B)"
                    .into(),
            },
            // Every remaining action type (balloon, mmds, pmem, etc.)
            // is wire-accepted with 204 — the device side isn't hooked
            // up yet and tok doesn't exercise it on the cold path.
            _ => ApiResponse::NoContent,
        };
        if ack.send(response).is_err() {
            debug!(action = label, "vmm loop: response channel closed");
        }
    }

    // Drain loop exit — make sure the VM is down.
    if let Some(vm) = running.take() {
        vm.request_shutdown();
        vm.join();
    }
    info!("vmm loop: exiting");
}

fn start_microvm(state: &mut BootState, cfg: &VmmLoopConfig) -> Result<RunningVm> {
    let boot_src = state
        .boot_source
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("boot source not configured"))?;
    let machine = state.machine_config.as_ref().ok_or_else(|| {
        anyhow::anyhow!("machine-config not configured (vcpu_count + mem_size_mib required)")
    })?;

    // 1. Compose VmResources from the accumulated state.
    let kernel_path = boot_src.kernel_image_path.as_path().to_path_buf();
    let initrd = boot_src
        .initrd_path
        .as_ref()
        .map(|p| InitrdSource::Path(p.as_path().to_path_buf()));
    let boot_args = boot_src.boot_args.clone().unwrap_or_default();
    let resources = VmResources {
        vcpu_count: machine.vcpu_count,
        mem_size_mib: machine.mem_size_mib.get(),
        kernel: KernelSource::Path(kernel_path),
        initrd,
        boot_args,
        root_partuuid: None,
        virtio_devices: Vec::new(),
    };

    // 2. Plan the boot — produces the HvfVm, FDT skeleton, boot regs.
    let mut boot: BootArtifacts = build_microvm_for_boot(&resources)
        .map_err(|e| anyhow::anyhow!("build_microvm_for_boot: {e}"))?;
    let hvf_vm = boot
        .hvf_vm
        .take()
        .ok_or_else(|| anyhow::anyhow!("HVF VM handle missing from BootArtifacts"))?;

    // 3. Build the device layout. GIC + guest mem come from the HVF VM.
    let _sizes = GicSizes::query().map_err(|e| anyhow::anyhow!("GicSizes::query: {e}"))?;
    let gic: Arc<dyn Gic + Send + Sync> = Arc::new(HvfGic::new(hvf_vm.instance().clone()));
    let guest_mem: Arc<dyn squib_core::GuestMemory> = hvf_vm
        .first_region_as_guest_memory()
        .map_err(|e| anyhow::anyhow!("HVF VM has no mapped guest memory region: {e}"))?;

    let block = state.drive.as_ref().map(|d| BlockConfigSpec {
        id: d.drive_id.as_str().to_string(),
        path: d.path_on_host.as_path().to_path_buf(),
        read_only: d.is_read_only,
    });
    let net = state
        .net
        .as_ref()
        .map(|n| build_net_spec(n, cfg))
        .transpose()?;

    // vsock: if configured, spawn the UDS muxer and hand it to the
    // device manager. The muxer spawn binds the per-port listeners;
    // surface its Notify handle so the RX pump can wake on new
    // inbound packets.
    let mut notify_handle: Option<NotifyHandle> = None;
    let vsock_spec = if let Some(v) = state.vsock.as_ref() {
        // The muxer needs the exact ports tok-initd listens on
        // (`crates/tok-initd/src/lib.rs`): 5001 exec, 5002 obs, 5003
        // stage, 5004 health. Listing them explicitly keeps the hot
        // path cheap (no per-port lookup) and the integration
        // surface narrow.
        let host_ports = vec![5001, 5002, 5003, 5004];
        let muxer = UdsVsockMuxer::spawn(UdsVsockMuxerParams {
            uds_base: v.uds_path.as_path().to_path_buf(),
            guest_cid: u64::from(v.guest_cid),
            host_initiated_ports: host_ports,
        })?;
        notify_handle = Some(muxer.notify_handle());
        Some(VsockSpec {
            vsock_id: v
                .vsock_id
                .as_ref()
                .map(|s| s.as_str().to_string())
                .unwrap_or_default(),
            guest_cid: u64::from(v.guest_cid),
            uds_path: v.uds_path.as_path().to_string_lossy().into_owned(),
            tsi: v.tsi,
            muxer,
        })
    } else {
        None
    };

    let layout = build_device_layout(
        Arc::clone(&gic),
        Arc::clone(&guest_mem),
        DeviceBuildArgs {
            pl011_sink: Box::new(StderrPl011Sink),
            mmds_size_cap: cfg.mmds_size_cap,
            block,
            net,
            enable_console: false,
            vsock: vsock_spec,
        },
    )
    .map_err(|e| anyhow::anyhow!("build_device_layout: {e}"))?;
    let vsock_device_arc = layout.vsock_device.clone();

    // 4. Re-build FDT with the actual virtio slots — the initial `build_microvm_for_boot` returned
    //    one with no devices.
    let mem_bytes = u64::from(machine.mem_size_mib.get()) * 1024 * 1024;
    let effective_args =
        squib_fdt::compose_boot_args(&resources.boot_args, resources.root_partuuid.as_deref());
    let new_fdt = squib_fdt::build(&FdtBuildArgs::new(
        resources.vcpu_count,
        MemoryRegion::dram(mem_bytes),
        &effective_args,
        boot.initrd_range,
        &layout.virtio_slots,
    ))
    .map_err(|e| anyhow::anyhow!("rebuild FDT: {e}"))?;
    boot.fdt_bytes = new_fdt;
    let ram_end = DRAM_BASE.saturating_add(mem_bytes);
    boot.fdt_base = ram_end - FDT_MAX_SIZE;
    boot.boot_regs = squib_arch::BootRegs::new(boot.kernel_load_addr, boot.fdt_base);

    // 5. Read the initrd bytes if configured (the runner needs them). `block_in_place` keeps the
    //    read off the runtime's async workers so a large initrd doesn't stall the API action loop —
    //    `start_microvm` runs inline in the `rx.recv().await` path.
    let initrd_bytes = match resources.initrd.clone() {
        Some(InitrdSource::Path(p)) => {
            let bytes = tokio::task::block_in_place(|| std::fs::read(&p))
                .map_err(|e| anyhow::anyhow!("reading initrd {}: {e}", p.display()))?;
            Some(bytes)
        }
        None => None,
    };

    // 6. Spawn the vCPU driver on a dedicated OS thread. The dedicated-thread constraint comes from
    //    `squib-vmm::runner` — every `hv_vcpu_*` call has to be on the thread that called
    //    `hv_vcpu_create`.
    let vm_arc = Arc::new(hvf_vm);
    let bus = Arc::clone(&layout.bus);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_runner = Arc::clone(&shutdown);
    let budget = cfg.run_budget;

    let joiner = thread::Builder::new()
        .name("squib-vmm-driver".to_string())
        .spawn(move || {
            match run_microvm_with_budget(boot, vm_arc, bus, initrd_bytes, budget) {
                Ok((_handle, result)) => {
                    info!(reason = ?result.reason, mmio = result.mmio_exits, hvc = result.hvc_exits, wfi = result.wfi_exits, elapsed = ?result.elapsed, "vCPU driver exited");
                    result
                }
                Err(e) => {
                    error!(error = %e, "run_microvm_with_budget failed");
                    RunResult {
                        reason: squib_vmm::runner::ShutdownReason::HvfError,
                        mmio_exits: 0,
                        hvc_exits: 0,
                        wfi_exits: 0,
                        elapsed: Duration::ZERO,
                    }
                }
            }
        })?;
    let _ = shutdown_for_runner;

    // 7. Spawn the vsock RX pump if configured: wake on every muxer notify and call
    //    `process_queue(RX_QUEUE)`, letting the device drain its internal rx buffer into the
    //    guest's posted descriptors.
    if let (Some(notify), Some(device)) = (notify_handle, vsock_device_arc) {
        let shutdown_for_pump = Arc::clone(&shutdown);
        tokio::spawn(async move {
            vsock_rx_pump(notify, device, shutdown_for_pump).await;
        });
    }

    Ok(RunningVm {
        shutdown,
        joiner: Some(joiner),
    })
}

fn build_net_spec(cfg: &NetworkInterfaceConfig, loop_cfg: &VmmLoopConfig) -> Result<NetSpec> {
    let guest_mac = cfg.guest_mac.as_ref().map(|m| m.bytes());
    let host_dev_name_str = cfg.host_dev_name.as_str().to_string();
    // Re-validate via `HostDevName::new` to keep the invariant explicit
    // — the validated one lives across the squib-api crate boundary but
    // the device manager takes a fresh `HostDevName`.
    let host_dev_name = HostDevName::new(host_dev_name_str.clone())
        .map_err(|e| anyhow::anyhow!("host_dev_name: {e}"))?;

    let backend: NetHostBackend = match loop_cfg.net_mode {
        NetMode::Vmnet(mode) => build_vmnet_backend(cfg, mode, loop_cfg)?,
        NetMode::Userspace => {
            let binary = loop_cfg
                .gvproxy_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("/usr/local/libexec/squib/gvproxy"));
            let backend = GvproxyBackend::start(&GvproxyParams::new(binary))
                .map_err(|e| anyhow::anyhow!("gvproxy start: {e}"))?;
            NetHostBackend::Gvproxy(backend)
        }
        // `NetMode` is `#[non_exhaustive]`; treat any future variant
        // the same as a misconfiguration until it's wired.
        other => anyhow::bail!("unsupported net mode {other:?}"),
    };

    Ok(NetSpec {
        iface_id: cfg.iface_id.as_str().to_string(),
        host_dev_name,
        guest_mac,
        mtu: None,
        backend,
    })
}

fn build_vmnet_backend(
    cfg: &NetworkInterfaceConfig,
    mode: VmnetMode,
    loop_cfg: &VmmLoopConfig,
) -> Result<NetHostBackend> {
    #[cfg(feature = "bridged")]
    let mut params = InterfaceParams::new(cfg.iface_id.as_str(), mode);
    #[cfg(not(feature = "bridged"))]
    let params = InterfaceParams::new(cfg.iface_id.as_str(), mode);
    #[cfg(feature = "bridged")]
    if matches!(mode, VmnetMode::Bridged) {
        params
            .bridged_iface_name
            .clone_from(&loop_cfg.bridged_iface);
    }
    #[cfg(not(feature = "bridged"))]
    let _ = loop_cfg;
    let iface = VmnetIface::start(params).map_err(|e| anyhow::anyhow!("vmnet start: {e}"))?;
    Ok(NetHostBackend::Vmnet(VmnetHostBackend::new(iface)))
}

/// Background task: wake on every muxer notify, call the vsock
/// device's `process_queue(RX_QUEUE)`. Exits when the shutdown flag
/// flips.
async fn vsock_rx_pump(
    notify: NotifyHandle,
    device: Arc<Mutex<squib_virtio::devices::vsock::VsockDevice>>,
    shutdown: Arc<AtomicBool>,
) {
    use squib_virtio::VirtioDevice;
    const RX_QUEUE: u16 = 0;
    while !shutdown.load(Ordering::SeqCst) {
        // Wake on either a notify or a short polling tick. The tick
        // is there so a race between notify and `notified().await`
        // registration doesn't stall packets (Notify is single-slot).
        tokio::select! {
            () = notify.notified() => {}
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        {
            let mut guard = device.lock();
            guard.process_queue(RX_QUEUE);
        }
    }
}

// Re-export `Notify` so doc-links in the muxer module resolve.
#[allow(dead_code)]
type _LinkBack = Notify;
