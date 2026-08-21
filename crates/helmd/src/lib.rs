//! helmd — the helm persistence daemon.
//!
//! Owns PTYs, ring-buffered scrollback, OSC 133 block segmentation, and
//! seq-numbered replay. See PLAN.md at the repo root for the full
//! architecture; the protocol lives in the `helm-proto` crate.
//!
//! Module map:
//!   ring     — per-pane byte ring addressed by absolute seq
//!   markers  — stateful streaming parser (OSC 133 / bell / alt-screen)
//!   pane     — PTY + reader thread: bytes → ring + parser → events
//!   daemon   — workspace/window/pane tree, client fan-out, blocks,
//!              notifications, search
//!   server   — unix-socket accept loop + `helmd stdio` bridge

pub mod daemon;
pub mod markers;
pub mod pane;
pub mod ring;
pub mod server;
