//! Generalized tool-integration framework.
//!
//! Premise: bell (BEL, 0x07) is the canonical "pay attention" signal —
//! every tool can emit it, our pipeline already detects it surgically
//! (per-pane, with active-window suppression and inbox routing). What's
//! missing is *making the tools bell at semantically meaningful moments*:
//!
//!   - Claude Code is a TUI that runs continuously; OSC 133 never fires
//!     during its lifetime, so the inbox stays silent unless Claude
//!     itself rings. By default it doesn't on "needs approval" or "task
//!     done" events. The community fix is two `~/.claude/settings.json`
//!     hooks that emit BEL on those events.
//!
//!   - Future: pgcli on slow query, mosh on drop, anything else a user
//!     wants to wire up. Same shape: detect the tool is in use, propose
//!     the integration with a one-time toast, install with consent.
//!
//! This module defines the trait + registry. Each integration owns its
//! own detection identifiers, install/uninstall semantics, and
//! idempotency story. The host_id-aware lifecycle (per-host install,
//! per-host dismissal) lives in commands.rs on top of this trait.
//!
//! ## Sharp edges that any new integration should think about
//!
//! - **Idempotency.** The user may have run `install` already, or have
//!   their own hook configured that does the same thing. `install`
//!   must be safe to call multiple times and must NOT clobber unrelated
//!   user state in shared config files.
//! - **Atomic writes.** Settings files are read by long-running
//!   processes; partial writes corrupt them. Always write to a
//!   sibling tmp file then rename.
//! - **Format preservation.** Use `serde_json::Value` (or a similar
//!   format-preserving parser per language) so unknown keys round-trip
//!   intact. Don't drop fields the integration doesn't recognize.
//! - **Activation cost.** Most integrations only take effect for *new*
//!   processes started after the file changed. The success toast must
//!   tell the user they need to restart the tool.

use async_trait::async_trait;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

use helm_domain::{Host, HostEvent, HostId};
use helm_ssh::SshSession;
use helm_tmux::TmuxClient;

pub mod claude_code;

/// One installable tool integration. Stable id is the persistence key
/// (used for "user dismissed this") and the wire identifier between
/// frontend and backend, so it must not change across releases.
#[async_trait]
pub trait ToolIntegration: Send + Sync {
    /// Stable id, e.g. `"claude-code"`. Persistence + wire key.
    fn id(&self) -> &'static str;

    /// Display name for the suggestion toast. e.g. `"Claude Code"`.
    fn name(&self) -> &'static str;

    /// Short description shown alongside the install button. Should
    /// state plainly *what* gets installed and *why*.
    fn description(&self) -> &'static str;

    /// Process names that signal this tool is active in a pane.
    /// Compared against tmux's `pane_current_command` — that field
    /// returns the basename of argv[0], not the full command line, so
    /// `["claude"]` matches `claude --foo bar` and any wrapper script
    /// also named `claude`. Conservative matching is on purpose; an
    /// integration's install touches user files, so false positives
    /// are worse than false negatives.
    ///
    /// For tools that mutate their process title (e.g. Node CLIs that
    /// set `process.title = version`), override `pane_matches` instead
    /// of relying on `process_names` alone.
    fn process_names(&self) -> &'static [&'static str];

    /// True if `current_command` (tmux's `pane_current_command` for a
    /// pane) indicates this tool is running. Default impl just checks
    /// against `process_names`. Override for tools whose process title
    /// doesn't match the binary name — Claude Code, for instance,
    /// mutates argv[0] to its version string.
    fn pane_matches(&self, current_command: &str) -> bool {
        self.process_names()
            .iter()
            .any(|name| *name == current_command)
    }

    /// Whether the integration is already installed for `host`.
    /// Implementations read whatever config file they manage and look
    /// for the canonical hook content (deep-equal on the snippet).
    ///
    /// `primary` is the host's primary tmux client, available for
    /// integrations that want to query state through tmux. `ssh` is
    /// the host's underlying SSH session — `None` for localhost,
    /// `Some` for remote — used by integrations that need to
    /// read/write remote files via `SshSession::run_oneshot`.
    /// Localhost-only integrations ignore both and use
    /// `dirs::home_dir()` + `std::fs`.
    async fn is_installed(
        &self,
        host: &Host,
        primary: &Arc<TmuxClient>,
        ssh: Option<&Arc<SshSession>>,
    ) -> Result<bool, String>;

    /// Install the integration. Must be idempotent — if already
    /// installed, `Ok(())` without changing the file. Existing user
    /// state in the same file (other hooks, unrelated settings) must
    /// be preserved.
    async fn install(
        &self,
        host: &Host,
        primary: &Arc<TmuxClient>,
        ssh: Option<&Arc<SshSession>>,
    ) -> Result<(), String>;

    /// Remove the integration. The reverse of `install`: drop only the
    /// hook entries the integration owns; leave other user state alone.
    /// Idempotent: calling `uninstall` when not installed is `Ok(())`.
    async fn uninstall(
        &self,
        host: &Host,
        primary: &Arc<TmuxClient>,
        ssh: Option<&Arc<SshSession>>,
    ) -> Result<(), String>;

    /// One-line message to show after a successful install. Usually
    /// "Restart $tool to activate the hooks." since most config files
    /// only take effect for new processes.
    fn post_install_note(&self) -> &'static str;
}

/// All integrations the binary ships. v1: just Claude Code. New
/// integrations get appended here; the order is the order the
/// suggestion toasts may appear in (insertion order, not user
/// preference).
pub fn registry() -> Vec<Box<dyn ToolIntegration>> {
    vec![Box::new(claude_code::ClaudeCodeIntegration)]
}

/// Cheap lookup helper for callers that have an id. Returns None if
/// the id isn't a registered integration.
pub fn find(id: &str) -> Option<Box<dyn ToolIntegration>> {
    registry().into_iter().find(|i| i.id() == id)
}

/// True iff at least one integration hasn't been suggested yet for
/// this host. Lets the periodic detector skip its work entirely once
/// every known tool has been offered to the user.
pub fn any_pending(suggested: &Arc<DashMap<(HostId, String), ()>>, host_id: HostId) -> bool {
    registry()
        .iter()
        .any(|i| !suggested.contains_key(&(host_id, i.id().to_string())))
}

/// Sweep the host's panes for tool processes that have a known
/// integration. For each match where the integration isn't already
/// installed AND we haven't already suggested it this session, emit
/// `HostEvent::ToolIntegrationSuggested`.
///
/// Idempotency: the `(host_id, integration_id)` pair is recorded in
/// `suggested` whether we emit a suggestion, find the integration
/// already installed, or hit a non-fatal error during is_installed.
/// That way the work is bounded — we ask each integration "are you
/// installed?" at most once per host per app launch.
///
/// Called from the supervisor after each `refresh_pane_index`, so
/// detection follows the same cadence as pane-index refreshes (on
/// connect, on `%window-added` etc.). Cheap: one `list-panes` call
/// + one is_installed per never-checked integration.
pub async fn detect_and_suggest(
    seen: &Arc<DashMap<(HostId, String), ()>>,
    event_tx: &Option<UnboundedSender<HostEvent>>,
    primary: &Arc<TmuxClient>,
    ssh: Option<&Arc<SshSession>>,
    host: &Host,
    host_id: HostId,
) {
    let raw = match primary.list_panes("#{pane_current_command}").await {
        Ok(s) => s,
        Err(_) => return,
    };
    let commands: std::collections::HashSet<String> = raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|s| s.trim().to_string())
        .collect();
    if commands.is_empty() {
        return;
    }

    for integration in registry() {
        let id = integration.id().to_string();
        let key = (host_id, id.clone());
        if seen.contains_key(&key) {
            continue;
        }

        // Delegate matching to the integration so tools with mutated
        // process titles (Claude Code → semver string) can recognize
        // themselves beyond the static process_names list.
        let matched = commands
            .iter()
            .any(|cmd| integration.pane_matches(cmd));
        if !matched {
            continue;
        }

        // Pre-check: don't suggest if already installed. is_installed
        // for local Claude is a single fs::read; for remote it's a
        // single `cat` over the existing SSH session — both cheap.
        let already_installed = integration
            .is_installed(host, primary, ssh)
            .await
            .unwrap_or(false);
        seen.insert(key.clone(), ());
        if already_installed {
            continue;
        }

        if let Some(tx) = event_tx {
            let _ = tx.send(HostEvent::ToolIntegrationSuggested {
                host_id,
                integration_id: id,
                name: integration.name().to_string(),
                description: integration.description().to_string(),
                post_install_note: integration.post_install_note().to_string(),
            });
        }
    }
}
