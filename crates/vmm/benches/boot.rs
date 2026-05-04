//! Boot-path benchmark harness — Phase 1.7 skeleton.
//!
//! Per [71-performance-budgets.md § 7](../../../specs/71-performance-budgets.md#7-bench-harness)
//! the harness drives the boot-timer device (which lands in Phase 3.6 / 14 § 4.8) and
//! reports the boot-to-`/sbin/init` time. Phase 1.7 ships the skeleton: criterion
//! configuration + a placeholder benchmark that exercises the build planning step.
//! The real `boot_to_init` measurement is bolted on once the boot-timer device is
//! available.
//!
//! Run with:
//!   `cargo bench --features bench -p squib-vmm --bench boot`

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use squib_vmm::{KernelSource, VmResources, build_microvm_for_boot};

/// Synthesize a minimal aarch64 kernel image.
fn synth_kernel() -> Vec<u8> {
    let mut img = vec![0u8; 256];
    img[8..16].copy_from_slice(&0x80_0000u64.to_le_bytes()); // text_offset
    img[16..24].copy_from_slice(&0x100_0000u64.to_le_bytes()); // image_size
    img[0x38..0x3C].copy_from_slice(b"ARM\x64");
    img
}

fn write_tmp() -> PathBuf {
    let path = std::env::temp_dir().join("squib-vmm-bench-kernel.bin");
    // Synchronous write of a 256-byte fixture during bench setup; tokio::fs is
    // async-only and would require a runtime spin-up the bench doesn't otherwise
    // need. Per-bench setup is single-threaded.
    #[allow(clippy::disallowed_methods)]
    std::fs::write(&path, synth_kernel()).expect("write tmp kernel");
    path
}

fn bench_planning_step(c: &mut Criterion) {
    let path = write_tmp();
    c.bench_function("build_microvm_for_boot/planning", |b| {
        b.iter(|| {
            let res = VmResources {
                vcpu_count: 1,
                mem_size_mib: 128,
                kernel: KernelSource::Path(path.clone()),
                initrd: None,
                boot_args: String::new(),
                root_partuuid: None,
                virtio_devices: Vec::new(),
            };
            // On non-macOS targets, build_microvm_for_boot validates and produces the
            // planning artifacts; on macOS it additionally initialises HVF (which is
            // single-shot per process and therefore not benched here).
            let _ = build_microvm_for_boot(&res);
        });
    });
}

criterion_group!(benches, bench_planning_step);
criterion_main!(benches);
