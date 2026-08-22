//! The environment a pane starts with.
//!
//! helmd inherits whatever launched it: launchd's near-empty set when
//! the app came from the Dock, an entire iTerm-inside-tmux session when
//! it came from `open` in a terminal, a previous Helm's when it's a dev
//! build started from a Helm pane. Panes used to inherit that verbatim,
//! so the same dotfiles produced a different environment depending on
//! how Helm had been opened — every PATH entry twice, a stale `TMUX` or
//! `ITERM_SESSION_ID`, a `ZDOTDIR` from the previous Helm.
//!
//! Terminal.app, iTerm and Warp don't do that. A login shell starts from
//! a small fixed base and the user's dotfiles build everything else, so
//! a pane looks the same however the app was launched. This module is
//! that base: a short allowlist of per-login-session handles that only
//! the launcher can supply (the ssh-agent socket, the per-user temp
//! dir, the locale), a system PATH for the dotfiles to extend, and our
//! own identity. Nothing else crosses over.

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Inherited variables a pane keeps, by exact name. Each is a handle to
/// the login session that dotfiles can't reconstruct on their own.
const KEEP: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "SSH_AUTH_SOCK",
    "TZ",
    // macOS: what Terminal.app passes a fresh tab.
    "__CF_USER_TEXT_ENCODING",
    "XPC_FLAGS",
    "SECURITYSESSIONID",
    "COMMAND_MODE",
    // Linux desktop/session handles.
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Inherited variables a pane keeps, by prefix: locale (`LANG`, `LC_*`,
/// `LANGUAGE`), the sshd-provided `SSH_*` when helmd itself runs at the
/// far end of an SSH bridge, and XDG session dirs.
const KEEP_PREFIX: &[&str] = &["LANG", "LC_", "SSH_", "XDG_"];

/// What PATH looks like before any dotfile touches it — the same value
/// a login shell from Terminal.app or a getty starts with. On macOS
/// `/etc/zprofile` then runs `path_helper`, which rebuilds it from
/// `/etc/paths` and `/etc/paths.d/*`, exactly as in any other terminal.
#[cfg(target_os = "macos")]
pub const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
#[cfg(not(target_os = "macos"))]
pub const SYSTEM_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Build a pane's base environment from the daemon's own (`inherited`).
/// Pure: the daemon layers `HELM_TTY` and the integration variables on
/// top, and the PTY layer sets `SHELL` from the password database when
/// it isn't inherited.
pub fn pane_env(inherited: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = inherited
        .into_iter()
        .filter(|(k, _)| KEEP.contains(&k.as_str()) || KEEP_PREFIX.iter().any(|p| k.starts_with(p)))
        .collect();

    // Fill what the launcher didn't supply. launchd gives a Dock-launched
    // app HOME/USER/TMPDIR but no LANG; a bare `exec` (the SSH bridge's
    // `helmd stdio`) may give almost nothing.
    if !env.contains_key("HOME") {
        if let Some(home) = dirs::home_dir() {
            env.insert("HOME".into(), home.to_string_lossy().into_owned());
        }
    }
    if let Some(user) = passwd_name() {
        env.entry("USER".into()).or_insert_with(|| user.clone());
        env.entry("LOGNAME".into()).or_insert(user);
    }
    let has_locale = env.keys().any(|k| k == "LANG" || k == "LC_ALL" || k == "LC_CTYPE");
    if !has_locale {
        env.insert("LANG".into(), default_lang().to_string());
    }

    env.insert("PATH".into(), SYSTEM_PATH.into());
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    env.insert("TERM_PROGRAM".into(), "Helm".into());
    env.insert("TERM_PROGRAM_VERSION".into(), env!("CARGO_PKG_VERSION").into());
    env.into_iter().collect()
}

/// The current user's name from the password database.
fn passwd_name() -> Option<String> {
    // SAFETY: getpwuid returns a pointer to static storage (or null);
    // we copy the name out immediately and never hold the pointer.
    unsafe {
        let ent = libc::getpwuid(libc::getuid());
        if ent.is_null() || (*ent).pw_name.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*ent).pw_name).to_str().ok().map(str::to_owned)
    }
}

/// The locale a pane gets when the launcher supplied none — what
/// Terminal.app does from the system locale setting. Without it
/// LC_CTYPE is "C": `/etc/zshrc` skips COMBINING_CHARS and every TUI
/// (Claude Code included) draws box characters wrong.
fn default_lang() -> &'static str {
    static LANG: OnceLock<String> = OnceLock::new();
    LANG.get_or_init(|| {
        #[cfg(target_os = "macos")]
        {
            let out = std::process::Command::new("defaults")
                .args(["read", "-g", "AppleLocale"])
                .output();
            if let Ok(out) = out {
                let locale = String::from_utf8_lossy(&out.stdout).trim().to_string();
                // "en_US", "fr_FR", "de_DE@currency=EUR" → keep the
                // language_REGION part; anything odd falls through.
                let base: String = locale.chars().take_while(|c| c.is_ascii_alphabetic() || *c == '_').collect();
                if base.len() >= 2 && base.contains('_') {
                    return format!("{base}.UTF-8");
                }
            }
            "en_US.UTF-8".to_string()
        }
        #[cfg(not(target_os = "macos"))]
        {
            "C.UTF-8".to_string()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pane_env(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string()))).into_iter().collect()
    }

    #[test]
    fn launcher_identity_and_path_do_not_leak() {
        let env = env_of(&[
            ("HOME", "/Users/x"),
            ("PATH", "/Users/x/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"),
            ("TMUX", "/tmp/tmux-501/default,1,0"),
            ("TMUX_PANE", "%3"),
            ("ITERM_SESSION_ID", "w0t0p0"),
            ("TERM_SESSION_ID", "abc"),
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM", "tmux-256color"),
            ("ZDOTDIR", "/Users/x/.helm/integration/zsh"),
            ("HELM_INTEGRATION", "1"),
            ("HELM_USER_ZDOTDIR", "/Users/x"),
            ("NVM_DIR", "/Users/x/.nvm"),
            ("VIRTUAL_ENV", "/Users/x/venv"),
        ]);
        for gone in ["TMUX", "TMUX_PANE", "ITERM_SESSION_ID", "TERM_SESSION_ID", "ZDOTDIR",
                     "HELM_INTEGRATION", "HELM_USER_ZDOTDIR", "NVM_DIR", "VIRTUAL_ENV"] {
            assert!(!env.contains_key(gone), "{gone} leaked: {:?}", env.get(gone));
        }
        assert_eq!(env["PATH"], SYSTEM_PATH);
        assert_eq!(env["TERM"], "xterm-256color");
        assert_eq!(env["TERM_PROGRAM"], "Helm");
        assert_eq!(env["HOME"], "/Users/x");
    }

    #[test]
    fn login_session_handles_are_kept() {
        let env = env_of(&[
            ("HOME", "/Users/x"),
            ("USER", "x"),
            ("SHELL", "/opt/homebrew/bin/fish"),
            ("TMPDIR", "/var/folders/ab/T/"),
            ("SSH_AUTH_SOCK", "/private/tmp/com.apple.launchd.abc/Listeners"),
            ("LANG", "fr_FR.UTF-8"),
            ("LC_TIME", "en_GB.UTF-8"),
            ("SSH_CONNECTION", "10.0.0.2 5000 10.0.0.1 22"),
            ("__CF_USER_TEXT_ENCODING", "0x1F5:0x0:0x0"),
        ]);
        assert_eq!(env["USER"], "x");
        assert_eq!(env["SHELL"], "/opt/homebrew/bin/fish");
        assert_eq!(env["TMPDIR"], "/var/folders/ab/T/");
        assert_eq!(env["SSH_AUTH_SOCK"], "/private/tmp/com.apple.launchd.abc/Listeners");
        assert_eq!(env["LANG"], "fr_FR.UTF-8");
        assert_eq!(env["LC_TIME"], "en_GB.UTF-8");
        assert_eq!(env["SSH_CONNECTION"], "10.0.0.2 5000 10.0.0.1 22");
        assert_eq!(env["__CF_USER_TEXT_ENCODING"], "0x1F5:0x0:0x0");
    }

    #[test]
    fn gaps_from_a_bare_launcher_are_filled() {
        // What `helmd stdio` under sshd, or a Dock launch, can look like.
        let env = env_of(&[]);
        assert!(env.contains_key("HOME"));
        assert!(env.contains_key("USER"));
        assert_eq!(env["USER"], env["LOGNAME"]);
        assert!(env["LANG"].ends_with("UTF-8"), "LANG={}", env["LANG"]);
        assert_eq!(env["PATH"], SYSTEM_PATH);
    }

    #[test]
    fn a_supplied_locale_is_not_overridden() {
        let env = env_of(&[("LC_ALL", "C")]);
        assert!(!env.contains_key("LANG"));
        assert_eq!(env["LC_ALL"], "C");
    }
}
