//! Generate a real snapshot pair on disk for end-to-end CLI smoke-testing.
//!
//! Usage:
//! ```sh
//! cargo run --example produce_demo_pair --package squib-snapshot -- /tmp/demo
//! cargo run -p squib-cli --bin squib -- --describe-snapshot /tmp/demo.snap
//! ```
//!
//! Produces `<base>.snap` + `<base>.mem` against the live `squib_snapshot::save`
//! pipeline so the resulting files exercise the full Phase 5 path: bitcode
//! envelope, CRC64 trailer, atomic temp-file + fsync + rename, dense memory
//! dump.

#![allow(clippy::disallowed_methods, clippy::cast_possible_truncation)]

use std::path::PathBuf;

use squib_snapshot::{
    DeviceState, DeviceStates, GicState, MicrovmState, MmdsState, PsciVcpuState, SaveRequest,
    SnapshotKind, VcpuState, VecPageReader, VmInfo, save,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let base = args
        .next()
        .map_or_else(|| PathBuf::from("/tmp/demo"), PathBuf::from);
    let snap = base.with_extension("snap");
    let mem = base.with_extension("mem");

    let state = MicrovmState {
        vm_info: VmInfo {
            mem_size_mib: 256,
            smt: false,
            cpu_template: "V1N1".into(),
            kernel_image_path: "/var/lib/squib/vmlinux".into(),
            initrd_path: Some("/var/lib/squib/initrd.cpio".into()),
            boot_args: "console=ttyAMA0 panic=1 reboot=k root=/dev/vda".into(),
            track_dirty_pages: false,
        },
        vcpu_states: vec![
            {
                let mut v = VcpuState::new(0);
                v.psci_state = PsciVcpuState::On;
                v
            },
            VcpuState::new(0x100),
        ],
        device_states: DeviceStates::from_devices(vec![
            DeviceState {
                kind: "virtio-block".into(),
                id: "rootfs".into(),
                mmio_slot: 0,
                blob: vec![0xA1, 0xA2, 0xA3, 0xA4],
            },
            DeviceState {
                kind: "virtio-net".into(),
                id: "eth0".into(),
                mmio_slot: 1,
                blob: vec![0xB1, 0xB2],
            },
        ]),
        gic_state: GicState::from_bytes((0..128).collect()),
        mmds_state: Some(
            MmdsState::with_data(
                &serde_json::json!({"latest": {"meta-data": {"instance-id": "demo"}}}),
                Some(3600),
            )
            .expect("MMDS encode"),
        ),
    };

    let ram_size: u64 = 64 * 1024;
    let mut bytes = vec![0u8; ram_size as usize];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i % 256) as u8;
    }
    let reader = VecPageReader::new(bytes);

    let report = save(SaveRequest {
        state_path: &snap,
        memory_path: &mem,
        kind: SnapshotKind::Full,
        state,
        memory: &reader,
        ram_size,
        memory_page_size: 16 * 1024,
        dirty: None,
    })
    .expect("snapshot save failed");

    println!("wrote {} ({} bytes)", snap.display(), file_size(&snap));
    println!("wrote {} ({} bytes)", mem.display(), file_size(&mem));
    println!(
        "kind={:?} pages_written={}",
        report.kind, report.pages_written
    );
}

fn file_size(p: &std::path::Path) -> u64 {
    std::fs::metadata(p).map_or(0, |m| m.len())
}
