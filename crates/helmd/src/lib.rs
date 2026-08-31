//! helmd — the helm persistence daemon.
//!
//! Owns PTYs, the terminal model per session (grid + line history), OSC 133
//! block segmentation, and notifications. See PLAN.md at the repo root
//! (M8) for the architecture; the protocol lives in `helm-proto`.
//!
//! Module map:
//!   markers  — stateful streaming parser (OSC 133 / bell / alt-screen)
//!   screen   — VT model (alacritty_terminal) + row history + encoding
//!   session  — PTY + reader thread: bytes → parser → model → events
//!   daemon   — session ownership, client fan-out, blocks,
//!              notifications, search, flush scheduling
//!   server   — unix-socket accept loop + `helmd stdio` bridge

pub mod agent_commands;
pub mod completion;
pub mod file_search;
pub mod daemon;
pub mod env;
pub mod markers;
pub mod screen;
pub mod server;
pub mod session;
