//! Apple Block ABI shim for the `vmnet_*_interface` callbacks.
//!
//! Earlier this module rolled the Block ABI by hand (global static literal +
//! signature string + descriptor). On macOS 15.x the layout had a subtle
//! incompatibility — `vmnet_start_interface` accepted the call (sync return
//! non-null) but the callback never fired, suggesting libdispatch refused to
//! invoke our block. Rather than keep guessing at undocumented ABI details,
//! we now use the [`block2`] crate which is the de-facto Rust binding for
//! Apple Blocks and is maintained by the objc2 ecosystem.
//!
//! Each [`super::iface_impl::Inner::start`] / `stop` call constructs a
//! one-shot [`block2::RcBlock`] that captures a `dispatch_semaphore_t` and
//! a `Mutex<Option<StartCallbackOutcome>>`. When the callback fires, the
//! captured state is written and the semaphore is signalled. The block is
//! kept alive for the duration of the call by the calling Rust frame.

use std::sync::Arc;

use block2::{Block, RcBlock};
use parking_lot::Mutex;

use super::{VmnetReturn, xpc::XpcObject};

/// Outcome of a `vmnet_start_interface` callback.
#[derive(Debug)]
pub struct StartCallbackOutcome {
    /// The status the callback received.
    pub status: VmnetReturn,
    /// The interface parameter dictionary, retained by the callback.
    pub param: Option<XpcObject>,
}

/// Shared state between the C callback and the Rust caller waiting on the
/// `vmnet_start_interface` result.
#[derive(Debug)]
pub struct StartContext {
    /// Semaphore the callback signals once it has populated `outcome`.
    pub semaphore: super::dispatch::Semaphore,
    /// The outcome — populated by the block, read by the caller after wait.
    pub outcome: Mutex<Option<StartCallbackOutcome>>,
}

impl StartContext {
    /// Build a fresh context to be cloned into the start/stop block.
    pub fn new(semaphore: super::dispatch::Semaphore) -> Arc<Self> {
        Arc::new(Self {
            semaphore,
            outcome: Mutex::new(None),
        })
    }
}

/// Build the start-interface callback block. The returned [`RcBlock`] must
/// be kept alive for the duration of the `vmnet_start_interface` call and
/// the subsequent `dispatch_semaphore_wait` — `block2` ensures the captured
/// `Arc<StartContext>` is reachable until the block is dropped.
///
/// Block signature: `void (^)(vmnet_return_t status, xpc_object_t param)`.
pub fn start_block(
    ctx: Arc<StartContext>,
) -> RcBlock<dyn Fn(u32, *mut std::os::raw::c_void) + 'static> {
    RcBlock::new(move |status: u32, param: *mut std::os::raw::c_void| {
        let outcome = StartCallbackOutcome {
            status: VmnetReturn::from_raw(status),
            // SAFETY: vmnet retains the XPC object before invoking the
            // callback; wrapping it in `XpcObject` takes ownership and the
            // eventual Drop calls `xpc_release`. NULL (failure case) is
            // preserved as `None`.
            param: unsafe { XpcObject::from_raw_retained_or_null(param) },
        };
        *ctx.outcome.lock() = Some(outcome);
        ctx.semaphore.signal();
    })
}

/// Build the stop-interface callback block. Same shape as the start block
/// but with no param pointer — `void (^)(vmnet_return_t status)`.
pub fn stop_block(ctx: Arc<StartContext>) -> RcBlock<dyn Fn(u32) + 'static> {
    RcBlock::new(move |status: u32| {
        let outcome = StartCallbackOutcome {
            status: VmnetReturn::from_raw(status),
            param: None,
        };
        *ctx.outcome.lock() = Some(outcome);
        ctx.semaphore.signal();
    })
}

/// Borrow a `RcBlock` as the raw `&Block<...>` type the FFI signatures expect.
/// Defined here so call sites don't need to depend on `block2` directly.
#[must_use]
pub fn as_raw_start(
    b: &RcBlock<dyn Fn(u32, *mut std::os::raw::c_void) + 'static>,
) -> *const Block<dyn Fn(u32, *mut std::os::raw::c_void) + 'static> {
    &raw const **b
}

/// As [`as_raw_start`] but for the stop callback.
#[must_use]
pub fn as_raw_stop(b: &RcBlock<dyn Fn(u32) + 'static>) -> *const Block<dyn Fn(u32) + 'static> {
    &raw const **b
}
