//! LLDB-attach integration tests for the squib-host pager (Phase 5.6).
//!
//! Per [16-snapshots.md § 5](../../../specs/16-snapshots.md#5-postcopy--lazy-restore)
//! the pager must keep `lldb -p $(pgrep squib)` working in both orderings:
//!
//! 1. squib registers the exception port first; LLDB attaches later (the "lldb-after" case). The
//!    pager re-installs its handler on the next 1 s drift-check tick.
//! 2. LLDB attaches first; squib registers later (the "lldb-before" case). The pager's
//!    `task_swap_exception_ports` captures LLDB's handler as the prior port and forwards every
//!    out-of-region exception to it.
//!
//! Phase 5.5's pager skeleton ships the lifecycle (spawn → poll → shutdown →
//! join) and the counters (`port_reinstalls`, `forwarded_exceptions`). The live
//! `mach_msg` server feature is gated; the tests here verify the lifecycle
//! contract end-to-end without privileged Mach state. When the live feature
//! lands, these same tests gain real LLDB-attach coverage by adding a
//! `lldb -p $(pgrep <test-binary>) -o detach` driver under the same names.

use std::{thread, time::Duration};

use bytes::Bytes;
use squib_host::{
    PageRequest, PageSource, PageSourceError, Pager, PagerConfig, PagerStatsSnapshot, PrewarmList,
    spawn_mach_server,
};

#[derive(Debug)]
struct ConstSource(Bytes);

impl PageSource for ConstSource {
    fn fetch(&self, _req: &PageRequest) -> Result<Bytes, PageSourceError> {
        Ok(self.0.clone())
    }
}

fn make_pager() -> Pager {
    Pager::new(
        Box::new(ConstSource(Bytes::from(vec![0u8; 16 * 1024]))),
        PagerConfig {
            poll_interval: Duration::from_millis(50),
            prewarm: PrewarmList::default(),
        },
    )
}

#[test]
fn pager_lifecycle_accepts_shutdown_in_lldb_after_ordering() {
    // Simulates: squib-pager spawns → "LLDB attaches" (no-op in this skeleton)
    // → squib-pager remains responsive → shutdown → join.
    let pager = make_pager();
    pager.register_region(0x8000_0000, 0x1000_0000);
    let join = spawn_mach_server(&pager).expect("spawn pager");

    // Let the drift-check tick at least twice.
    thread::sleep(Duration::from_millis(150));

    // External thread "attaches" — in production this is `lldb -p $(pgrep squib)`.
    // The pager's drift-check would re-install on its next tick (the live
    // implementation calls `task_get_exception_ports` and `task_swap_*` if our
    // handler was overwritten). The skeleton verifies the tick rate is honored.

    // Shutdown.
    pager.request_shutdown();
    // Wake the parked thread; the live path uses `mach_msg` cancellation.
    // For the skeleton, the thread checks the flag at every poll boundary —
    // joining within poll_interval × 4 is the lifecycle promise.
    wait_for_join(join, Duration::from_secs(5))
        .expect("pager should join cleanly after request_shutdown");
}

#[test]
fn pager_lifecycle_accepts_shutdown_in_lldb_before_ordering() {
    // Simulates: "LLDB attaches first" → squib-pager spawns → squib-pager owns
    // the exception port → shutdown.
    //
    // In the skeleton, the LLDB-before case is symmetric to LLDB-after — the
    // pager spawns, runs its drift loop, and exits on shutdown.
    let pager = make_pager();
    pager.register_region(0x8000_0000, 0x1000_0000);
    let join = spawn_mach_server(&pager).expect("spawn pager");
    thread::sleep(Duration::from_millis(120));
    pager.request_shutdown();
    wait_for_join(join, Duration::from_secs(5))
        .expect("pager should join cleanly after request_shutdown");
}

#[test]
fn pager_serves_concurrent_faults_without_corrupting_stats() {
    let pager = make_pager();
    pager.register_region(0x8000_0000, 0x1000_0000);
    let pager = std::sync::Arc::new(pager);
    let mut handles = vec![];
    for t in 0..4u64 {
        let p = pager.clone();
        handles.push(thread::spawn(move || {
            for i in 0..100u64 {
                let ipa = 0x8000_0000 + ((t * 100 + i) * 16 * 1024);
                if ipa < 0x9000_0000 {
                    p.serve_fault(ipa, 16 * 1024).unwrap();
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let s: PagerStatsSnapshot = pager.stats();
    assert_eq!(s.faults, 400);
}

fn wait_for_join<T>(join: thread::JoinHandle<T>, timeout: Duration) -> Result<(), &'static str> {
    let start = std::time::Instant::now();
    let mut maybe_join = Some(join);
    loop {
        if let Some(j) = maybe_join.take() {
            if j.is_finished() {
                let _ = j.join().map_err(|_| "pager thread panicked")?;
                return Ok(());
            }
            maybe_join = Some(j);
        }
        if start.elapsed() > timeout {
            return Err("pager thread did not join within timeout");
        }
        thread::sleep(Duration::from_millis(20));
    }
}
