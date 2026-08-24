//! helmd — the helm persistence daemon.
//!
//! Owns PTYs, the terminal model per pane (grid + line history), OSC 133
//! block segmentation, and notifications. See PLAN.md at the repo root
//! (M8) for the architecture; the protocol lives in `helm-proto`.
//!
//! Module map:
//!   markers  — stateful streaming parser (OSC 133 / bell / alt-screen)
//!   screen   — VT model (alacritty_terminal) + row history + encoding
//!   pane     — PTY + reader thread: bytes → parser → model → events
//!   daemon   — workspace/window/pane tree, client fan-out, blocks,
//!              notifications, search, flush scheduling
//!   server   — unix-socket accept loop + `helmd stdio` bridge

pub mod env;
pub mod completion;
pub mod daemon;
pub mod markers;
pub mod pane;
pub mod screen;
pub mod server;
