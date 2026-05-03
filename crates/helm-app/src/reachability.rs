//! Network reachability monitor.
//!
//! Watches for OS-level "is the network up" transitions so the reconnect
//! supervisor can break out of its backoff sleep early when WiFi/VPN/etc.
//! comes back. Without this, after a 30s sleep tick a connection that
//! could be re-established immediately on network return waits up to 30
//! more seconds before retrying.
//!
//! macOS implementation uses `SCNetworkReachability` against `0.0.0.0`,
//! the Apple idiom for "any usable network." A dedicated CFRunLoop
//! thread owns the C-side observer; the callback writes into a tokio
//! `watch` channel that consumers can subscribe to.
//!
//! Non-macOS builds get a stub watch that always reports `online = true`
//! — the supervisor still works, it just doesn't get the early-wake.

use tokio::sync::watch;

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
    use std::net::SocketAddr;
    use system_configuration::network_reachability::{
        ReachabilityFlags, SCNetworkReachability,
    };
    use tracing::{debug, warn};

    pub fn spawn() -> watch::Receiver<bool> {
        let (tx, rx) = watch::channel(true);

        std::thread::Builder::new()
            .name("helm-reachability".into())
            .spawn(move || {
                // 0.0.0.0:0 — Apple's "any address" idiom for general
                // internet reachability (matches what Reachability.h's
                // sample code uses).
                let addr: SocketAddr = "0.0.0.0:0".parse().expect("static addr parses");
                let mut reach = SCNetworkReachability::from(addr);

                // Seed the channel with the current state so subscribers
                // don't sit on the default until the first transition.
                if let Ok(flags) = reach.reachability() {
                    let _ = tx.send(is_online(flags));
                }

                let tx_for_cb = tx.clone();
                if let Err(e) = reach.set_callback(move |flags| {
                    let online = is_online(flags);
                    debug!("reachability: flags={flags:?}, online={online}");
                    let _ = tx_for_cb.send(online);
                }) {
                    warn!("reachability: failed to install callback: {e}");
                    return;
                }

                // SAFETY: kCFRunLoopCommonModes is a static Apple-provided
                // mode string; CFRunLoop::get_current() returns the run
                // loop attached to this thread.
                unsafe {
                    if let Err(e) =
                        reach.schedule_with_runloop(&CFRunLoop::get_current(), kCFRunLoopCommonModes)
                    {
                        warn!("reachability: failed to schedule on runloop: {e}");
                        return;
                    }
                }

                CFRunLoop::run_current();
                // Run loop returned (process shutdown). Drop the sender,
                // closing the watch — consumers get a final "still
                // online" signal from before shutdown started.
                drop(tx);
            })
            .expect("failed to spawn reachability thread");

        rx
    }

    /// True iff the OS thinks at least one network path is up *without*
    /// requiring on-demand dial-up. Mirrors what AFNetworkReachability /
    /// other Apple sample code treats as "actually usable network."
    fn is_online(flags: ReachabilityFlags) -> bool {
        flags.contains(ReachabilityFlags::REACHABLE)
            && !flags.contains(ReachabilityFlags::CONNECTION_REQUIRED)
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub fn spawn() -> watch::Receiver<bool> {
        // Non-mac stub: always report online. The supervisor's backoff
        // ladder still functions; it just doesn't get the early-wake on
        // network return.
        let (_tx, rx) = watch::channel(true);
        // Leak the sender so the channel never closes and the receiver
        // doesn't see `Err(_)` on changed().
        std::mem::forget(_tx);
        rx
    }
}

/// Spawn the reachability monitor. Call once during app startup; hand
/// the returned receiver to anyone who needs to know about network
/// transitions (in our case: the reconnect supervisor).
pub fn spawn() -> watch::Receiver<bool> {
    platform::spawn()
}
