//! tmux control mode (`tmux -CC`) client.
//!
//! Drives the line-based protocol and emits typed state deltas.
//!
//! Layers:
//!   - `parse` · pure line parser (no IO)
//!   - `client` · spawn + reader/writer + command/response routing

pub mod client;
pub mod parse;

pub use client::{Cleanup, TmuxClient, TmuxError};
pub use parse::{parse_line, Notification, TmuxLine};
