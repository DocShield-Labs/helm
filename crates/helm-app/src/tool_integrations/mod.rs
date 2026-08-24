//! Generalized tool-integration framework.
//!
//! Premise: bell (BEL, 0x07) is the canonical "pay attention" signal —
//! every tool can emit it, helmd detects it per session, and the inbox
//! routes it. What's missing is *making the tools bell at semantically
//! meaningful moments*:
//!
//!   - Claude Code is a TUI that runs continuously; OSC 133 never fires
//!     during its lifetime, so the inbox stays silent unless Claude
//!     itself rings. The fix is two `~/.claude/settings.json` hooks
//!     that emit BEL on "needs input" and "turn finished".
//!
//! Detection is driven by block metadata: when a shell accepts a
//! command (`OSC 133;B`) helmd reports the command line, and we match
//! its first token against each integration. No process polling.
//!
//! ## Sharp edges that any new integration should think about
//!
//! - **Idempotency.** `install` must be safe to call repeatedly and
//!   must NOT clobber unrelated user state in shared config files.
//! - **Atomic writes.** Write to a sibling tmp file then rename.
//! - **Format preservation.** Round-trip unknown keys intact.
//! - **Activation cost.** Most integrations only take effect for *new*
//!   processes; the success toast must say so.

use async_trait::async_trait;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

use helm_domain::{Host, HostEvent, HostId};
use helm_proto::BlockMeta;
use helm_ssh::SshSession;

pub mod claude_code;

/// One installable tool integration. Stable id is the persistence key
/// and the wire identifier between frontend and backend.
#[async_trait]
pub trait ToolIntegration: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;

    /// Program names (argv[0] basename) that signal this tool.
    fn process_names(&self) -> &'static [&'static str];

    /// True if a command line's program indicates this tool.
    fn command_matches(&self, program: &str) -> bool {
        self.process_names().iter().any(|name| *name == program)
    }

    /// `ssh` is the host's SSH session — `None` for localhost.
    async fn is_installed(
        &self,
        host: &Host,
        ssh: Option<&Arc<SshSession>>,
    ) -> Result<bool, String>;
    async fn install(&self, host: &Host, ssh: Option<&Arc<SshSession>>) -> Result<(), String>;
    async fn uninstall(&self, host: &Host, ssh: Option<&Arc<SshSession>>) -> Result<(), String>;

    fn post_install_note(&self) -> &'static str;
}

pub fn registry() -> Vec<Box<dyn ToolIntegration>> {
    vec![Box::new(claude_code::ClaudeCodeIntegration)]
}

pub fn find(id: &str) -> Option<Box<dyn ToolIntegration>> {
    registry().into_iter().find(|i| i.id() == id)
}

/// True iff at least one integration hasn't been suggested yet for
/// this host — lets the detector skip work entirely once every known
/// tool has been offered.
pub fn any_pending(suggested: &Arc<DashMap<(HostId, String), ()>>, host_id: HostId) -> bool {
    registry()
        .iter()
        .any(|i| !suggested.contains_key(&(host_id, i.id().to_string())))
}

/// Program name (argv[0] basename) of a command line: first
/// whitespace-separated token, leading `VAR=value` assignments and
/// `sudo`/`env` wrappers skipped, path stripped.
pub fn program_of(cmdline: &str) -> Option<String> {
    let mut tokens = cmdline.split_whitespace().peekable();
    while let Some(tok) = tokens.peek() {
        if tok.contains('=') && !tok.starts_with('-') {
            tokens.next();
            continue;
        }
        if *tok == "sudo" || *tok == "env" || *tok == "exec" || *tok == "nohup" {
            tokens.next();
            continue;
        }
        break;
    }
    let first = tokens.next()?;
    let name = first.rsplit('/').next().unwrap_or(first);
    (!name.is_empty()).then(|| name.to_string())
}

/// Called by the connection pump for every block event. When a block
/// gains its command line, see whether the program has an integration
/// we haven't offered yet; if it isn't already installed, emit a
/// `ToolIntegrationSuggested`. The `(host, integration)` pair is marked
/// seen after the first check either way, so the per-host cost is
/// bounded to one `is_installed` per integration per app launch.
pub fn detect_from_block(
    seen: &Arc<DashMap<(HostId, String), ()>>,
    event_tx: &Option<UnboundedSender<HostEvent>>,
    host: &Host,
    ssh: Option<Arc<SshSession>>,
    host_id: HostId,
    block: &BlockMeta,
) {
    let Some(cmdline) = &block.cmdline else {
        return;
    };
    if !any_pending(seen, host_id) {
        return;
    }
    let Some(program) = program_of(cmdline) else {
        return;
    };

    for integration in registry() {
        let key = (host_id, integration.id().to_string());
        if seen.contains_key(&key) || !integration.command_matches(&program) {
            continue;
        }
        // Reserve before the async check so concurrent blocks can't
        // double-suggest.
        seen.insert(key, ());
        let host = host.clone();
        let ssh = ssh.clone();
        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            let installed = integration
                .is_installed(&host, ssh.as_ref())
                .await
                .unwrap_or(false);
            if installed {
                return;
            }
            if let Some(tx) = event_tx {
                let _ = tx.send(HostEvent::ToolIntegrationSuggested {
                    host_id,
                    integration_id: integration.id().to_string(),
                    name: integration.name().to_string(),
                    description: integration.description().to_string(),
                    post_install_note: integration.post_install_note().to_string(),
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_of_strips_wrappers_and_paths() {
        assert_eq!(program_of("claude --model x").as_deref(), Some("claude"));
        assert_eq!(program_of("/opt/bin/claude").as_deref(), Some("claude"));
        assert_eq!(
            program_of("FOO=1 BAR=2 sudo env claude").as_deref(),
            Some("claude")
        );
        assert_eq!(program_of("   "), None);
    }
}
