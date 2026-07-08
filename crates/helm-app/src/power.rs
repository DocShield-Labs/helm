//! System power (sleep/wake) monitor.
//!
//! Emits a signal when the machine resumes from sleep so the reconnect
//! supervisor can immediately probe each live SSH session instead of
//! waiting out the SSH keepalive window (~45s of "connected but frozen"
//! after every wake). Sleep is the one network-killing event the OS
//! announces explicitly — everything else (WiFi drop, VPN flap) is
//! covered by the keepalive itself.
//!
//! macOS implementation mirrors `reachability.rs`: a dedicated CFRunLoop
//! thread owns an `IORegisterForSystemPower` observer; the callback
//! bumps a `u64` generation counter in a tokio `watch` channel that
//! consumers subscribe to via `.changed()`. A counter (not a bool)
//! because wake is an event, not a state — `watch` coalesces rapid
//! wakes, which is exactly what we want.
//!
//! Non-macOS builds get a stub watch that never fires — supervisors
//! simply never take the wake-probe branch, and the SSH keepalive
//! remains the (slower) detection path.

use tokio::sync::watch;

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use core_foundation::base::TCFType;
    use core_foundation::runloop::{
        kCFRunLoopCommonModes, CFRunLoop, CFRunLoopSource, CFRunLoopSourceRef,
    };
    use std::ffi::c_void;
    use tracing::{debug, warn};

    // ---- IOKit FFI (the subset IORegisterForSystemPower needs) ----
    // IOKit has no maintained Rust bindings crate we don't already
    // transitively avoid; the surface here is four stable C symbols
    // documented in IOKit/pwr_mgt/IOPMLib.h since macOS 10.0.

    #[allow(non_camel_case_types)]
    type io_connect_t = u32; // mach_port_t
    #[allow(non_camel_case_types)]
    type io_object_t = u32;
    #[allow(non_camel_case_types)]
    type io_service_t = u32;
    type IONotificationPortRef = *mut c_void;

    type IOServiceInterestCallback = unsafe extern "C" fn(
        refcon: *mut c_void,
        service: io_service_t,
        message_type: u32,
        message_argument: *mut c_void,
    );

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IORegisterForSystemPower(
            refcon: *mut c_void,
            the_port_ref: *mut IONotificationPortRef,
            callback: IOServiceInterestCallback,
            notifier: *mut io_object_t,
        ) -> io_connect_t;
        fn IONotificationPortGetRunLoopSource(notify: IONotificationPortRef)
            -> CFRunLoopSourceRef;
        fn IOAllowPowerChange(kernel_port: io_connect_t, notification_id: isize) -> i32;
    }

    // `iokit_common_msg(...)` values from IOKit/IOMessage.h.
    const K_IO_MESSAGE_CAN_SYSTEM_SLEEP: u32 = 0xE000_0270;
    const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: u32 = 0xE000_0280;
    const K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = 0xE000_0300;

    /// Callback context. Leaked once at registration (lives for the
    /// process); `root_port` is written after `IORegisterForSystemPower`
    /// returns but before the run loop starts, so the callback — which
    /// only ever fires from `CFRunLoopRun` on the same thread — always
    /// sees it initialized.
    struct Ctx {
        tx: watch::Sender<u64>,
        root_port: io_connect_t,
    }

    unsafe extern "C" fn power_callback(
        refcon: *mut c_void,
        _service: io_service_t,
        message_type: u32,
        message_argument: *mut c_void,
    ) {
        let ctx = &*(refcon as *const Ctx);
        match message_type {
            // We never veto or delay sleep — acknowledge immediately.
            // Skipping this makes the kernel wait up to 30s for us on
            // every lid close.
            K_IO_MESSAGE_CAN_SYSTEM_SLEEP | K_IO_MESSAGE_SYSTEM_WILL_SLEEP => {
                let _ = IOAllowPowerChange(ctx.root_port, message_argument as isize);
            }
            K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => {
                debug!("power: system woke from sleep");
                ctx.tx.send_modify(|n| *n += 1);
            }
            _ => {}
        }
    }

    pub fn spawn() -> watch::Receiver<u64> {
        let (tx, rx) = watch::channel(0u64);

        std::thread::Builder::new()
            .name("helm-power".into())
            .spawn(move || {
                // Leaked deliberately — the observer lives for the whole
                // process, and keeping `tx` alive inside it means the
                // watch never closes (receivers pend rather than error).
                let ctx = Box::into_raw(Box::new(Ctx { tx, root_port: 0 }));

                let mut port: IONotificationPortRef = std::ptr::null_mut();
                let mut notifier: io_object_t = 0;
                // SAFETY: ctx outlives the process; out-pointers are
                // valid locals; callback matches the declared signature.
                let root_port = unsafe {
                    IORegisterForSystemPower(
                        ctx as *mut c_void,
                        &mut port,
                        power_callback,
                        &mut notifier,
                    )
                };
                if root_port == 0 {
                    // MACH_PORT_NULL — registration failed. Wake probing
                    // is disabled; SSH keepalive still detects dead
                    // connections, just slower.
                    warn!("power: IORegisterForSystemPower failed; wake probing disabled");
                    return;
                }
                // SAFETY: callback can't have fired yet (run loop below
                // hasn't started), so no concurrent access.
                unsafe { (*ctx).root_port = root_port };

                // SAFETY: port is valid (registration succeeded); the
                // returned source is owned by the port (get rule);
                // kCFRunLoopCommonModes is a static Apple mode string.
                unsafe {
                    let source = CFRunLoopSource::wrap_under_get_rule(
                        IONotificationPortGetRunLoopSource(port),
                    );
                    CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes);
                }
                CFRunLoop::run_current();
            })
            .expect("failed to spawn power monitor thread");

        rx
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub fn spawn() -> watch::Receiver<u64> {
        // Non-mac stub: never fires. Leak the sender so the channel
        // never closes and `.changed()` pends forever instead of
        // erroring in a loop.
        let (tx, rx) = watch::channel(0u64);
        std::mem::forget(tx);
        rx
    }
}

/// Spawn the power monitor. Call once during app startup; hand the
/// returned receiver to anyone who needs to react to system wake (in
/// our case: the reconnect supervisor's post-wake liveness probe).
pub fn spawn() -> watch::Receiver<u64> {
    platform::spawn()
}
