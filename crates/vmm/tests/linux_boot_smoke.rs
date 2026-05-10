//! Real-Linux boot integration test.
//!
//! Boots the reference VM under HVF: real aarch64 Linux kernel +
//! busybox-shaped initramfs from `examples/reference-vm/build/`.
//! The init script brings up `eth0`, hits the link-local MMDS, and
//! prints the response between `===SQUIB-DEMO===` / `===END===`
//! markers on PL011. The test captures PL011 output and asserts the
//! markers + payload show up.
//!
//! ## Pre-requisites
//!
//! - `examples/reference-vm/build.sh` has been run to produce `examples/reference-vm/build/Image`
//!   and `examples/reference-vm/build/initramfs.cpio.gz`.
//! - The test binary is codesigned with the `com.apple.security.hypervisor` entitlement (`make
//!   hvf-test` wires this).
//!
//! Skips itself with a `eprintln!` warning when either is missing,
//! rather than failing the suite — the HVF-stub path
//! (`runner_hvf_smoke.rs`) still proves the run-loop + bus + PL011
//! integration without requiring the kernel artifacts.

#![cfg(target_os = "macos")]
// Integration test: long single function, mid-test `use` statements
// for namespacing, sync `std::fs::*`. All fine for this gated test.
#![allow(
    clippy::doc_markdown,
    clippy::cast_lossless,
    clippy::disallowed_methods,
    clippy::uninlined_format_args,
    clippy::too_many_lines,
    clippy::items_after_statements
)]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use squib_core::GuestMemory;
use squib_gic::{Gic, GicSizes, HvfGic};
use squib_legacy::Pl011Sink;
use squib_vmm::{
    BootArtifacts, build_microvm_for_boot,
    device_manager::{DeviceBuildArgs, NetSpec, build_device_layout},
    runner::{ShutdownReason, run_microvm_with_budget},
};

/// Captured-bytes sink — push every byte the guest writes to PL011.
#[derive(Debug, Clone, Default)]
struct CapturedSink(Arc<Mutex<Vec<u8>>>);

impl Pl011Sink for CapturedSink {
    fn write_byte(&mut self, byte: u8) {
        self.0.lock().push(byte);
    }
}

fn artifacts_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/reference-vm/build")
}

fn artifact_paths() -> Option<(PathBuf, PathBuf)> {
    let root = artifacts_root();
    let kernel = root.join("Image");
    let initrd = root.join("initramfs.cpio.gz");
    if !kernel.exists() || !initrd.exists() {
        eprintln!(
            "linux_boot_smoke: skipping — reference-vm artifacts not found.\nexpected:\n  {}\n  \
             {}\nrun `examples/reference-vm/build.sh` first.",
            kernel.display(),
            initrd.display()
        );
        return None;
    }
    Some((kernel, initrd))
}

#[test]
#[ignore = "requires HVF entitlement and reference-vm artifacts; run via `make demo`"]
fn test_reference_vm_boots_linux_and_curls_mmds() {
    let Some((kernel_path, initrd_path)) = artifact_paths() else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("squib_vmm=debug")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let initrd_bytes = std::fs::read(&initrd_path).expect("read initrd");

    // 1. Boot args wire the demo init script + force the early console onto our PL011,
    //    panic-on-fault, and tell the kernel to use /init from the initramfs.
    let boot_args = "earlycon=pl011,mmio32,0xe0a0000 console=ttyAMA0 keep_bootcon panic=15 \
                     reboot=k rdinit=/init squib_demo_ip=169.254.169.50 \
                     squib_demo_path=/latest/meta-data/instance-id loglevel=8 ignore_loglevel \
                     printk.devkmsg=on";

    // 2. Plan the boot artifacts (kernel, initrd, FDT placement).
    let resources = squib_vmm::resources::VmResources {
        vcpu_count: 1,
        // 512 MiB — the Firecracker reference kernel is ~16 MiB
        // uncompressed; the initrd planner needs DRAM+256 MiB clearance
        // for the initrd, so 256 MiB total RAM is too tight.
        mem_size_mib: 512,
        kernel: squib_vmm::KernelSource::Path(kernel_path.clone()),
        initrd: Some(squib_vmm::InitrdSource::Path(initrd_path.clone())),
        boot_args: boot_args.into(),
        root_partuuid: None,
        // The DeviceManager constructs the actual virtio devices and
        // returns the slot vector; squib's builder passes them into the
        // FDT. For now we set this empty so the builder picks the
        // default placement; we'll plug devices in below and re-build
        // the FDT.
        virtio_devices: Vec::new(),
    };
    let mut boot: BootArtifacts =
        build_microvm_for_boot(&resources).expect("build_microvm_for_boot");

    // 3. Construct the device layout (PL011 + virtio-rng + virtio-net bound to the MMDS
    //    interceptor). We need the GIC and a guest- memory handle from the live HVF VM.
    let vm = boot.hvf_vm.take().expect("HVF VM present");
    let _sizes = GicSizes::query().expect("GicSizes::query");
    let gic_arc: Arc<dyn Gic + Send + Sync> = Arc::new(HvfGic::new(vm.instance().clone()));
    // Hold a guest-memory handle so we can scan DRAM after the run for
    // the kernel's printk ringbuffer (in case the console was never
    // bound and panic output stayed in memory).
    let guest_mem_post = vm.first_region_as_guest_memory().expect("guest mem post");
    let sink = CapturedSink::default();
    let layout = build_device_layout(
        gic_arc.clone(),
        vm.first_region_as_guest_memory().expect("guest mem"),
        DeviceBuildArgs {
            pl011_sink: Box::new(sink.clone()),
            mmds_size_cap: 8192,
            block: None,
            net: Some(NetSpec::loopback("eth0", "tap0").unwrap()),
            enable_console: false,
            vsock: None,
        },
    )
    .expect("device layout");

    // Seed MMDS with the value the init script will fetch.
    layout
        .mmds
        .mmds()
        .put_json(r#"{"latest":{"meta-data":{"instance-id":"i-squibdemo"}}}"#)
        .expect("seed MMDS");

    // 4. Re-build the FDT now that we know the actual virtio slots. The original `boot.fdt_bytes`
    //    came from the builder with an empty virtio_devices list; we replace it.
    use squib_arch::layout::{DRAM_BASE, FDT_MAX_SIZE};
    use squib_fdt::{FdtBuildArgs, MemoryRegion};
    let mem_bytes = resources.mem_size_mib * 1024 * 1024;
    let memory_region = MemoryRegion::dram(mem_bytes);
    let effective_args =
        squib_fdt::compose_boot_args(&resources.boot_args, resources.root_partuuid.as_deref());
    let new_fdt = squib_fdt::build(&FdtBuildArgs::new(
        resources.vcpu_count,
        memory_region,
        &effective_args,
        boot.initrd_range,
        &layout.virtio_slots,
    ))
    .expect("rebuild FDT");
    // Dump the FDT for inspection (reproducible across runs).
    let fdt_dump = std::env::temp_dir().join("squib-demo.dtb");
    std::fs::write(&fdt_dump, &new_fdt).unwrap();
    eprintln!(
        "FDT dumped to {} ({} bytes)",
        fdt_dump.display(),
        new_fdt.len()
    );
    boot.fdt_bytes = new_fdt;
    // FDT base in the last 2 MiB of RAM, recomputed.
    let ram_end = DRAM_BASE.saturating_add(mem_bytes);
    boot.fdt_base = ram_end - FDT_MAX_SIZE;
    boot.boot_regs = squib_arch::BootRegs::new(boot.kernel_load_addr, boot.fdt_base);

    // 5. Run with a hard wall-clock budget so a hung boot fails the test in a reasonable time. The
    //    runner observes the budget on every loop iteration and exits with `OperatorRequest`.
    let vm_arc = Arc::new(vm);
    let bus = layout.bus.clone();
    let started = Instant::now();
    let (_handle, result) = run_microvm_with_budget(
        boot,
        vm_arc,
        bus,
        Some(initrd_bytes),
        Duration::from_mins(1),
    )
    .expect("run_microvm");
    let elapsed = started.elapsed();
    let captured = sink.0.lock().clone();
    let captured_str = String::from_utf8_lossy(&captured);
    eprintln!(
        "---- run stats: reason={:?} mmio={} hvc={} wfi={} elapsed={:?} ----",
        result.reason, result.mmio_exits, result.hvc_exits, result.wfi_exits, result.elapsed
    );
    eprintln!(
        "---- PL011 capture ({} bytes, {:?}) ----",
        captured.len(),
        elapsed
    );
    eprintln!("{captured_str}");
    eprintln!("---- end PL011 capture ----");

    // The Firecracker reference 6.1 kernel ships without
    // CONFIG_SERIAL_AMBA_PL011_CONSOLE — `earlycon=pl011` finds the
    // entry but the regular console never binds, so PL011 stays
    // silent. Init redirects its stdout/stderr to /dev/kmsg, which
    // lands in the printk ringbuffer in DRAM. Read that out and use it
    // as the source of truth for the assertions below.
    let ringbuffer = harvest_ringbuffer(&*guest_mem_post);
    eprintln!("---- ringbuffer ({} bytes) ----", ringbuffer.len());
    eprintln!("{ringbuffer}");
    eprintln!("---- end ringbuffer ----");
    if captured.is_empty() {
        scan_guest_dram_for_kernel_strings(&*guest_mem_post, mem_bytes);
    }
    // Combine PL011 + ringbuffer so the assertions pass with EITHER
    // source — once a kernel ships with PL011 console support the
    // PL011 capture takes over without test churn.
    let mut combined = captured_str.into_owned();
    combined.push('\n');
    combined.push_str(&ringbuffer);
    let captured_str = combined;

    // 6. Assertions.
    assert!(
        matches!(
            result.reason,
            ShutdownReason::SystemOff | ShutdownReason::SystemReset
        ),
        "expected clean PSCI shutdown, got {:?} after {:?}",
        result.reason,
        elapsed
    );
    assert!(
        captured_str.contains("===SQUIB-DEMO==="),
        "init banner not seen in PL011 output"
    );
    assert!(
        captured_str.contains("i-squibdemo"),
        "MMDS payload not echoed in PL011 output"
    );
    assert!(
        captured_str.contains("===END==="),
        "init END marker not seen — script aborted before MMDS reply?"
    );
}

/// Harvest the kernel's printk ringbuffer (~64 KiB at the address the
/// kernel pins early in boot) into a single newline-joined string.
/// Tests use this as the source of truth on kernel builds that ship
/// without a console driver — `/dev/kmsg` ends up here.
fn harvest_ringbuffer<G: GuestMemory + ?Sized>(mem: &G) -> String {
    use squib_core::GuestAddress;

    const RB_BASE: u64 = 0x8121_6000;
    const RB_SIZE: usize = 64 * 1024;
    let mut rb = vec![0u8; RB_SIZE];
    if mem.read(GuestAddress(RB_BASE), &mut rb).is_err() {
        return String::new();
    }
    let mut out = String::new();
    let mut line = String::new();
    for &b in &rb {
        if b == b'\n' || (0x20..0x7f).contains(&b) {
            if b == b'\n' {
                if !line.is_empty() {
                    out.push_str(&line);
                    out.push('\n');
                    line.clear();
                }
            } else {
                line.push(b as char);
            }
        } else if !line.is_empty() {
            if line.len() >= 8 {
                out.push_str(&line);
                out.push('\n');
            }
            line.clear();
        }
    }
    if !line.is_empty() && line.len() >= 8 {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Scan DRAM for printable runs containing kernel-printk-style markers
/// — used when the early console produced nothing. We page through DRAM
/// in 1 MiB chunks (memory reads are mutex-locked so a single big read
/// blocks the world unnecessarily).
fn scan_guest_dram_for_kernel_strings<G: GuestMemory + ?Sized>(mem: &G, bytes: u64) {
    use squib_arch::layout::DRAM_BASE;
    use squib_core::GuestAddress;

    const CHUNK: usize = 1 << 20;
    const MARKERS: &[&str] = &[
        "Linux version",
        "Booting Linux",
        "Kernel panic",
        "Unable to handle",
        "Bad mode in",
        "End of",
        "PSCI",
        "Internal error",
    ];
    let mut buf = vec![0u8; CHUNK];
    let mut hits = 0usize;
    let total = usize::try_from(bytes).unwrap_or(0);
    let mut offset = 0usize;
    while offset < total {
        let take = CHUNK.min(total - offset);
        let addr = GuestAddress(DRAM_BASE + offset as u64);
        if mem.read(addr, &mut buf[..take]).is_err() {
            break;
        }
        // Find runs of printable ASCII >= 16 chars and emit any that
        // contain one of the markers.
        let mut i = 0;
        while i < take {
            let mut j = i;
            while j < take {
                let b = buf[j];
                if b == b'\n' || (0x20..0x7f).contains(&b) {
                    j += 1;
                } else {
                    break;
                }
            }
            if j - i >= 16 {
                let s = std::str::from_utf8(&buf[i..j]).unwrap_or("");
                for m in MARKERS {
                    if s.contains(m) {
                        hits += 1;
                        let phys = DRAM_BASE + offset as u64 + i as u64;
                        eprintln!("DRAM@{:#x}: {}", phys, s.replace('\n', "\\n"));
                        break;
                    }
                }
            }
            i = j.max(i + 1);
        }
        offset += take;
    }
    if hits == 0 {
        eprintln!(
            "DRAM scan: no kernel-printk markers found in {} MiB.",
            bytes / (1 << 20)
        );
    }
    // Also dump a 32KiB window around the kernel printk ringbuffer —
    // recent runs put it at ~0x8121b000. Render printable bytes inline
    // so init's stderr (script set -e error, etc.) shows up.
    const RB_BASE: u64 = 0x8121_6000;
    const RB_SIZE: usize = 64 * 1024;
    let mut rb = vec![0u8; RB_SIZE];
    if mem.read(GuestAddress(RB_BASE), &mut rb).is_ok() {
        eprintln!(
            "---- ringbuffer dump @ {:#x} ({} KiB) ----",
            RB_BASE,
            RB_SIZE / 1024
        );
        let mut line = String::new();
        for &b in &rb {
            if b == b'\n' || (0x20..0x7f).contains(&b) {
                if b == b'\n' {
                    if !line.is_empty() {
                        eprintln!("RB: {line}");
                        line.clear();
                    }
                } else {
                    line.push(b as char);
                }
            } else if !line.is_empty() {
                if line.len() >= 8 {
                    eprintln!("RB: {line}");
                }
                line.clear();
            }
        }
        if !line.is_empty() && line.len() >= 8 {
            eprintln!("RB: {line}");
        }
        eprintln!("---- end ringbuffer dump ----");
    }
}

/// Helper test that documents the artifact path layout — no boot.
#[test]
fn test_artifacts_root_resolves_under_examples() {
    let root = artifacts_root();
    let trailing: &Path = root.as_ref();
    assert!(
        trailing.ends_with("examples/reference-vm/build"),
        "unexpected artifacts root: {}",
        root.display()
    );
}
