//! Live FFI smoke test for `vmnet.framework`.
//!
//! Cuts through the binding stack end-to-end: builds an XPC parameter
//! dictionary, allocates a serial dispatch queue, calls `vmnet_start_interface`,
//! and waits on the global block trampoline's `dispatch_semaphore`. The
//! callback either reports `VmnetReturn::Success` (the test process is signed
//! with `com.apple.security.hypervisor` and vmnet was happy) or
//! `VmnetReturn::Failure`/`InvalidAccess` (unsigned process). Either outcome
//! is evidence that the FFI surface is shaped correctly — **the test fails
//! only if the call segfaults or hangs**.
//!
//! `#[ignore]` keeps it out of the default `cargo test` run because the live
//! call requires macOS at build time. To exercise it locally:
//!
//! ```sh
//! cargo test -p squib-net --test vmnet_ffi_smoke -- --ignored
//! ```

#![cfg(target_os = "macos")]

use std::time::Duration;

use squib_net::{IfaceError, InterfaceParams, VmnetIface, VmnetMode};

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("squib_net=debug")
        .with_test_writer()
        .try_init();
}

#[test]
#[ignore = "Calls into vmnet.framework. Run with `cargo test -- --ignored` or `make vmnet-test`."]
fn test_should_round_trip_vmnet_start_and_stop_via_real_framework() {
    init_tracing();
    // Use a generous start timeout — vmnet's first-time allocation can
    // take a couple of hundred milliseconds while the kernel sets up the
    // sharing service.
    let params = InterfaceParams {
        iface_id: "squib-vmnet-smoke".into(),
        mode: VmnetMode::Shared,
        bridged_iface_name: None,
        mtu: None,
        start_timeout: Duration::from_secs(10),
        enable_isolation: true,
    };
    match VmnetIface::start(params) {
        Ok(mut iface) => {
            // Signed-binary path: the start callback returned Success and
            // we have a live interface. Cross-check the negotiated values
            // and then stop cleanly. This is the path `make demo` exercises.
            assert!(iface.mtu() >= 1500, "vmnet reported MTU below 1500");
            assert_ne!(iface.host_mac(), [0u8; 6], "vmnet reported zero MAC");
            iface.stop().expect("stop succeeds on a started iface");
        }
        Err(IfaceError::Inner(squib_net::sys::iface_impl::InnerError::StartFailed { code })) => {
            // Unsigned-binary path: vmnet rejected the request. The
            // important thing is that the block trampoline fired and we
            // reached this branch with a recognisable status, rather than
            // hanging on the `dispatch_semaphore_wait` or segfaulting on
            // the descriptor layout. Print what we got for diagnostics.
            eprintln!("vmnet_start_interface declined (expected on unsigned binaries): {code:?}");
        }
        Err(IfaceError::Inner(squib_net::sys::iface_impl::InnerError::StartTimeout {
            timeout_ms,
        })) => {
            panic!(
                "vmnet_start_interface callback never fired within {timeout_ms} ms — the block \
                 trampoline / dispatch queue / static block layout is broken."
            );
        }
        Err(other) => {
            panic!("unexpected error shape from VmnetIface::start: {other}");
        }
    }
}
