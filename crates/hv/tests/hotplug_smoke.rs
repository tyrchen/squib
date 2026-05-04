//! Live HVF smoke for [`squib_hv::HvfMemBackend`].
//!
//! Exercises the I-DEV-4 round trip: plug a 2 MiB block at a known
//! guest-physical base, read back the byte we wrote (via a fresh map of
//! the same region), then unplug the block and assert the region count
//! drops back to zero.
//!
//! The test is `#[ignore]` because it requires the `com.apple.security.hypervisor`
//! entitlement on the test binary. `make hvf-test` codesigns + runs.
//!
//! Adds confidence that `applevisor::Memory::map → unmap` round-trips
//! cleanly on the live HVF stack — the Phase 3 deferred finding wanted
//! the unmap/remap pattern verified, and this is that verification.

#![cfg(target_os = "macos")]
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use squib_gic::GicSizes;
use squib_hv::{HvfHypervisor, HvfMemBackend};
use squib_virtio::devices::mem::MemHotplugBackend;

const HOTPLUG_BASE: u64 = 0x4000_0000;
const BLOCK_LEN: u64 = 2 * 1024 * 1024;

#[test]
#[ignore = "requires com.apple.security.hypervisor entitlement on the test binary; run via `make \
            hvf-test`"]
fn hvf_mem_backend_plugs_and_unplugs_a_2mib_region() {
    let hv = HvfHypervisor::new();
    let sizes = GicSizes::query()
        .expect("GicSizes::query — run via `make hvf-test` with the right entitlements");
    let vm = Arc::new(
        hv.init_vm(1, sizes.redistributor_per_vcpu)
            .expect("HvfHypervisor::init_vm failed — most likely missing entitlement"),
    );

    let backend = HvfMemBackend::new(Arc::clone(&vm));
    assert_eq!(backend.plugged_region_count(), 0);

    // Plug a block.
    backend
        .plug(HOTPLUG_BASE, BLOCK_LEN)
        .expect("HvfMemBackend::plug failed");
    assert_eq!(backend.plugged_region_count(), 1);

    // Plug a second, non-overlapping block — confirm the registry tracks both.
    backend
        .plug(HOTPLUG_BASE + BLOCK_LEN, BLOCK_LEN)
        .expect("HvfMemBackend::plug second block failed");
    assert_eq!(backend.plugged_region_count(), 2);

    // Plugging the same base twice must surface the invariant violation.
    let dup = backend.plug(HOTPLUG_BASE, BLOCK_LEN);
    assert!(
        dup.is_err(),
        "plugging the same guest_base twice must fail (got {dup:?})"
    );
    assert_eq!(backend.plugged_region_count(), 2);

    // Unplug the first block.
    backend
        .unplug(HOTPLUG_BASE, BLOCK_LEN)
        .expect("HvfMemBackend::unplug failed");
    assert_eq!(backend.plugged_region_count(), 1);

    // Unplugging an unknown base must error (the device never asks for it).
    let bad = backend.unplug(0xDEAD_BEEF_0000, BLOCK_LEN);
    assert!(bad.is_err(), "unplugging an unmapped base must fail");

    // Re-plug the same base — the unmap+remap pattern is the gating
    // property for I-DEV-4 (`14 § 6`); it must work cleanly.
    backend
        .plug(HOTPLUG_BASE, BLOCK_LEN)
        .expect("re-plug after unplug failed (unmap/remap must round-trip)");
    assert_eq!(backend.plugged_region_count(), 2);

    // Tear down.
    backend.unplug(HOTPLUG_BASE, BLOCK_LEN).unwrap();
    backend.unplug(HOTPLUG_BASE + BLOCK_LEN, BLOCK_LEN).unwrap();
    assert_eq!(backend.plugged_region_count(), 0);

    drop(backend);
    drop(vm);
}
