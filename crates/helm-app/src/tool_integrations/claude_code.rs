//! Claude Code integration.
//!
//! Adds two hooks to `~/.claude/settings.json` that emit BEL on the
//! events helm cares about:
//!
//!   - `Notification` — Claude needs the user's input (tool approval,
//!     idle-waiting). The headline use case for the inbox: "Claude is
//!     waiting on me in workspace 3."
//!   - `Stop` — Claude finished a turn. Surfaces "task done" without
//!     the user having to keep that pane focused.
//!
//! Both hooks are `printf '\a' > /dev/tty 2>/dev/null || true` — the
//! universal "ring my terminal" line, with the `|| true` so a hook
//! failure (e.g. headless run, no tty) doesn't break Claude's flow.
//!
//! ## Idempotency strategy
//!
//! The settings file is JSON; users may have their own hooks for the
//! same event types. We:
//!   1. Parse with `serde_json::Value` so unknown keys round-trip.
//!   2. For each event type we own, look at the array. If our exact
//!      hook command is already present (deep equal on the inner
//!      `command` string), we don't re-add. Other hooks the user
//!      configured stay intact.
//!   3. Write atomically (sibling tmp file + rename) so concurrent
//!      readers never see a partial file.
//!
//! Uninstall is the mirror — drop entries whose `command` matches
//! ours; leave other entries.
//!
//! ## Remote scope
//!
//! Both local and remote hosts are supported. Local uses
//! `dirs::home_dir()` + `std::fs`. Remote uses
//! `SshSession::run_oneshot` over the existing SSH session — `cat` to
//! read, `base64 -d` heredoc to write atomically. The same
//! idempotency + JSON-preservation guarantees apply: parse,
//! `ensure_hook` / `remove_hook` mutations, write back. base64
//! transport sidesteps every shell-quoting concern (any byte the JSON
//! contains survives the round trip).

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use helm_domain::Host;
use helm_ssh::SshSession;
use helm_tmux::TmuxClient;

use super::ToolIntegration;

/// Stable ids for our two hooks. Matches what we write into the
/// settings file. If we ever change the BEL command, bump these so
/// is_installed/uninstall match the right thing.
const HOOK_CMD: &str = r#"printf '\a' > /dev/tty 2>/dev/null || true"#;

pub struct ClaudeCodeIntegration;

#[async_trait]
impl ToolIntegration for ClaudeCodeIntegration {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn name(&self) -> &'static str {
        "Claude Code"
    }

    fn description(&self) -> &'static str {
        "Add Notification + Stop hooks to ~/.claude/settings.json so \
         Claude rings the bell when it needs input or finishes a turn. \
         Helm's inbox picks the bell up and surfaces the workspace."
    }

    fn process_names(&self) -> &'static [&'static str] {
        &["claude", "claude-code"]
    }

    /// Claude Code mutates `process.title` to its version string
    /// (e.g. `"2.1.126"`), so tmux's `pane_current_command` for an
    /// active Claude pane returns the version rather than `"claude"`.
    /// Match the binary name *or* a semver-shaped string. Vanishingly
    /// rare for an unrelated process to carry a name like `1.2.3`,
    /// and the install is gated on a user click anyway.
    fn pane_matches(&self, current_command: &str) -> bool {
        if self
            .process_names()
            .iter()
            .any(|name| *name == current_command)
        {
            return true;
        }
        is_semver_like(current_command)
    }

    async fn is_installed(
        &self,
        host: &Host,
        _primary: &Arc<TmuxClient>,
        ssh: Option<&Arc<SshSession>>,
    ) -> Result<bool, String> {
        let value = read_settings(host, ssh).await?;
        Ok(has_hook(&value, "Notification") && has_hook(&value, "Stop"))
    }

    async fn install(
        &self,
        host: &Host,
        _primary: &Arc<TmuxClient>,
        ssh: Option<&Arc<SshSession>>,
    ) -> Result<(), String> {
        let mut value = read_settings(host, ssh).await?;
        ensure_hook(&mut value, "Notification");
        ensure_hook(&mut value, "Stop");
        write_settings(host, ssh, &value).await
    }

    async fn uninstall(
        &self,
        host: &Host,
        _primary: &Arc<TmuxClient>,
        ssh: Option<&Arc<SshSession>>,
    ) -> Result<(), String> {
        let mut value = read_settings(host, ssh).await?;
        remove_hook(&mut value, "Notification");
        remove_hook(&mut value, "Stop");
        write_settings(host, ssh, &value).await
    }

    fn post_install_note(&self) -> &'static str {
        "Restart Claude Code (close and reopen the session) to activate the hooks."
    }
}

/// True iff `s` looks like a semantic version (`<digits>.<digits>...`).
/// Catches Claude Code's process title pattern (`"2.1.126"`) without
/// matching genuine binary names — they'd need to contain a dot AND
/// be entirely numeric to false-positive.
fn is_semver_like(s: &str) -> bool {
    if s.is_empty() || !s.contains('.') {
        return false;
    }
    s.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Read `~/.claude/settings.json` from the right side of the wire and
/// parse to a `serde_json::Value`. Returns an empty object for a
/// missing file (the install path treats missing the same as empty —
/// we'll create it).
async fn read_settings(host: &Host, ssh: Option<&Arc<SshSession>>) -> Result<Value, String> {
    if host.port == 0 {
        let path = local_settings_path()?;
        match fs::read_to_string(&path) {
            Ok(s) => parse_or_empty(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Value::Object(Default::default()))
            }
            Err(e) => Err(format!("read {}: {e}", path.display())),
        }
    } else {
        let ssh = ssh.ok_or_else(|| "remote host missing SSH session".to_string())?;
        // `cat` to stdout; if missing, `|| echo '{}'` substitutes an
        // empty object so the parser can proceed without a special case.
        let cmd = "cat \"$HOME/.claude/settings.json\" 2>/dev/null || echo '{}'".to_string();
        let out = tokio::task::spawn_blocking({
            let ssh = ssh.clone();
            move || ssh.run_oneshot(cmd)
        })
        .await
        .map_err(|e| format!("ssh task: {e}"))?
        .map_err(|e| e.to_string())?;
        parse_or_empty(&out.stdout)
    }
}

/// Write `value` back to `~/.claude/settings.json` atomically. Local
/// uses a sibling `.tmp` rename. Remote sends the JSON over the
/// existing SSH session base64-encoded so any byte the JSON contains
/// (quotes, newlines, dollar signs, heredoc markers) survives intact.
async fn write_settings(
    host: &Host,
    ssh: Option<&Arc<SshSession>>,
    value: &Value,
) -> Result<(), String> {
    let serialized =
        serde_json::to_string_pretty(value).map_err(|e| format!("serialize: {e}"))?;
    if host.port == 0 {
        let path = local_settings_path()?;
        return write_atomic_local(&path, &serialized);
    }
    let ssh = ssh.ok_or_else(|| "remote host missing SSH session".to_string())?;
    let b64 = STANDARD.encode(serialized.as_bytes());
    // Equivalent of write_atomic_local: stage into `.helm.tmp`, then
    // rename over the canonical path. Atomic against concurrent
    // readers (Claude Code itself) — they never see a partial file.
    let cmd = format!(
        "mkdir -p \"$HOME/.claude\" && \
         echo '{b64}' | base64 -d > \"$HOME/.claude/settings.json.helm.tmp\" && \
         mv \"$HOME/.claude/settings.json.helm.tmp\" \"$HOME/.claude/settings.json\""
    );
    let out = tokio::task::spawn_blocking({
        let ssh = ssh.clone();
        move || ssh.run_oneshot(cmd)
    })
    .await
    .map_err(|e| format!("ssh task: {e}"))?
    .map_err(|e| e.to_string())?;
    if !matches!(out.exit_code, Some(0) | None) {
        return Err(format!(
            "remote write failed (exit {:?}): {}",
            out.exit_code,
            out.stderr.trim()
        ));
    }
    Ok(())
}

fn local_settings_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|h| h.join(".claude").join("settings.json"))
        .ok_or_else(|| "no $HOME — can't locate ~/.claude/settings.json".into())
}

/// Parse a JSON string, returning an empty object for empty input
/// (so a freshly-touched settings file doesn't blow up).
fn parse_or_empty(s: &str) -> Result<Value, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(trimmed).map_err(|e| format!("parse settings.json: {e}"))
}

/// True iff the value has a hooks.<event>[*].hooks[*] entry whose
/// `command` field equals our HOOK_CMD. Ignores other hook entries
/// (the user may have their own).
fn has_hook(value: &Value, event: &str) -> bool {
    let Some(events) = value.pointer(&format!("/hooks/{event}")) else {
        return false;
    };
    let Some(arr) = events.as_array() else {
        return false;
    };
    for matcher in arr {
        let Some(hooks) = matcher.get("hooks").and_then(|h| h.as_array()) else {
            continue;
        };
        for hook in hooks {
            if hook.get("command").and_then(|c| c.as_str()) == Some(HOOK_CMD) {
                return true;
            }
        }
    }
    false
}

/// Append our hook for `event` if not already present. Creates the
/// nested structure as needed; never touches sibling keys.
fn ensure_hook(value: &mut Value, event: &str) {
    if has_hook(value, event) {
        return;
    }
    // Coerce root to object.
    if !value.is_object() {
        *value = Value::Object(Default::default());
    }
    let root = value.as_object_mut().unwrap();
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Default::default()));
    if !hooks.is_object() {
        *hooks = Value::Object(Default::default());
    }
    let hooks_obj = hooks.as_object_mut().unwrap();
    let arr = hooks_obj
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !arr.is_array() {
        *arr = Value::Array(Vec::new());
    }
    arr.as_array_mut().unwrap().push(json!({
        "hooks": [{
            "type": "command",
            "command": HOOK_CMD,
        }]
    }));
}

/// Drop any matcher whose `hooks` array references our HOOK_CMD —
/// drop the entire matcher (not just our hook entry within it),
/// because we always create a dedicated matcher when installing.
/// This means uninstall preserves user-authored matchers that include
/// other hooks but not ours.
fn remove_hook(value: &mut Value, event: &str) {
    let Some(arr) = value
        .pointer_mut(&format!("/hooks/{event}"))
        .and_then(|v| v.as_array_mut())
    else {
        return;
    };
    arr.retain(|matcher| {
        let Some(hooks) = matcher.get("hooks").and_then(|h| h.as_array()) else {
            return true;
        };
        // Keep matcher unless ALL of its hooks are ours (we own the
        // matcher) or it's a single matcher containing only our hook.
        // For simplicity in v1: drop any matcher containing our hook
        // command. Since install always creates a dedicated matcher
        // with just our hook, this matches what we wrote.
        !hooks
            .iter()
            .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(HOOK_CMD))
    });
}

/// Write `serialized` to `path` atomically: stage into a sibling
/// `.tmp` file then rename over the canonical path. Same pattern the
/// host persistence layer uses.
fn write_atomic_local(path: &std::path::Path, serialized: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.helm.tmp");
    fs::write(&tmp, serialized).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_hook_creates_structure_from_empty() {
        let mut v = Value::Object(Default::default());
        ensure_hook(&mut v, "Notification");
        assert!(has_hook(&v, "Notification"));
        assert!(!has_hook(&v, "Stop"));
    }

    #[test]
    fn ensure_hook_is_idempotent() {
        let mut v = Value::Object(Default::default());
        ensure_hook(&mut v, "Notification");
        ensure_hook(&mut v, "Notification");
        let arr = v
            .pointer("/hooks/Notification")
            .and_then(|n| n.as_array())
            .unwrap();
        assert_eq!(arr.len(), 1, "second install should not duplicate");
    }

    #[test]
    fn ensure_hook_preserves_user_hooks() {
        let mut v: Value = serde_json::from_str(
            r#"{
                "hooks": {
                    "Notification": [{
                        "hooks": [{
                            "type": "command",
                            "command": "say 'attention please'"
                        }]
                    }]
                },
                "other_setting": 42
            }"#,
        )
        .unwrap();
        ensure_hook(&mut v, "Notification");
        let arr = v
            .pointer("/hooks/Notification")
            .and_then(|n| n.as_array())
            .unwrap();
        assert_eq!(arr.len(), 2, "should append, not replace");
        assert_eq!(v["other_setting"], json!(42), "sibling keys preserved");
    }

    #[test]
    fn remove_hook_drops_only_ours() {
        let mut v: Value = serde_json::from_str(
            r#"{
                "hooks": {
                    "Notification": [
                        {
                            "hooks": [{
                                "type": "command",
                                "command": "say 'attention'"
                            }]
                        },
                        {
                            "hooks": [{
                                "type": "command",
                                "command": "printf '\\a' > /dev/tty 2>/dev/null || true"
                            }]
                        }
                    ]
                }
            }"#,
        )
        .unwrap();
        remove_hook(&mut v, "Notification");
        let arr = v
            .pointer("/hooks/Notification")
            .and_then(|n| n.as_array())
            .unwrap();
        assert_eq!(arr.len(), 1);
        let cmd = arr[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("attention"));
    }

    #[test]
    fn parse_or_empty_handles_empty_file() {
        assert!(parse_or_empty("").unwrap().as_object().unwrap().is_empty());
        assert!(parse_or_empty("   \n").unwrap().as_object().unwrap().is_empty());
    }

    #[test]
    fn semver_pattern_matches_claude_titles() {
        assert!(is_semver_like("2.1.126"));
        assert!(is_semver_like("1.0.0"));
        assert!(is_semver_like("0.0.1"));
        assert!(is_semver_like("12.34.56"));
    }

    #[test]
    fn semver_pattern_rejects_real_binaries() {
        assert!(!is_semver_like("claude"));
        assert!(!is_semver_like("node"));
        assert!(!is_semver_like("zsh"));
        assert!(!is_semver_like("vim"));
        assert!(!is_semver_like("npm"));
        assert!(!is_semver_like(""));
        assert!(!is_semver_like("123")); // no dot — could be a real name
        assert!(!is_semver_like("foo.bar")); // letters present
    }
}
