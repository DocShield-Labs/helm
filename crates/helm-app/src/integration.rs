//! Shell integration scripts (OSC 133 emitters).
//!
//! Embeds three scripts via `include_str!` and provides install helpers
//! for both local (file-write) and remote (base64-into-SSH-command)
//! delivery. Once installed and sourced, each shell emits the OSC 133
//! prompt-integration markers that helmd segments into blocks and
//! turns into notifications.
//!
//! Auto-injection model:
//!   - **zsh** auto-injects via `ZDOTDIR`: helmd sets ZDOTDIR on every
//!     shell it spawns to our wrapper directory. zsh reads *every*
//!     startup file from there — `.zshenv`, `.zprofile`, `.zshrc` — so
//!     the directory holds a forwarder for each: it points ZDOTDIR back
//!     at the user's real directory (`HELM_USER_ZDOTDIR`), sources their
//!     file, then points it at us again for the next one. `.zshrc` is
//!     the last hop: it leaves ZDOTDIR restored and installs the hooks.
//!     Zero user action, and nothing in the user's config is skipped —
//!     see the tests at the bottom, which run a real zsh to prove it.
//!   - **bash / fish** have no equivalent of ZDOTDIR. The script is
//!     written to disk; phase 4D will surface a one-time toast asking
//!     the user to add a single `source` line to their rc file. Bell
//!     detection still works without integration.

use std::path::{Path, PathBuf};

/// `~/.helm/integration/zsh/.zshenv` — the first file zsh reads from
/// ZDOTDIR. Forwards to the user's real `.zshenv` (where things like
/// `~/.cargo/env` live) and learns whether it relocated ZDOTDIR.
pub const ZSH_ENV: &str = include_str!("integration/zsh.zshenv");

/// `~/.helm/integration/zsh/.zprofile` — forwards to the user's real
/// `.zprofile` (`brew shellenv`, pyenv, PATH additions) on login shells.
pub const ZSH_PROFILE: &str = include_str!("integration/zsh.zprofile");

/// `~/.helm/integration/zsh/.zshrc` — forwards to the user's real
/// `.zshrc`, leaves ZDOTDIR restored to their directory, then registers
/// the OSC 133 hooks.
pub const ZSH_RC: &str = include_str!("integration/zsh.zshrc");

/// `~/.helm/integration/bash` — manual source target. Phase 4D will
/// detect missing integration and surface a setup toast asking the user
/// to add `[ -n "$HELM_INTEGRATION" ] && source ~/.helm/integration/bash`
/// to their `.bashrc`.
pub const BASH: &str = include_str!("integration/bash.sh");

/// `~/.helm/integration/fish` — manual source target. Same toast model
/// as bash; user adds `if test -n "$HELM_INTEGRATION"; source ~/.helm/integration/fish; end`
/// to their `config.fish`.
pub const FISH: &str = include_str!("integration/fish.fish");

/// Every file in the zsh wrapper directory. One forwarder per startup
/// file zsh reads from ZDOTDIR — leave one out and that file of the
/// user's silently stops running inside Helm.
pub const ZSH_FILES: [(&str, &str); 3] =
    [(".zshenv", ZSH_ENV), (".zprofile", ZSH_PROFILE), (".zshrc", ZSH_RC)];

/// Directory under the user's home where we install the integration
/// scripts. Stable across releases — users may reasonably want to
/// inspect or modify these.
pub fn integration_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".helm").join("integration"))
}

/// Idempotent install of the three integration scripts under
/// `~/.helm/integration/`. Always overwrites — the bytes are embedded in
/// the binary, so the on-disk copy is always what *this build* shipped
/// (never older). Cheap, no hash check needed.
///
/// Failures are bubbled up so the caller can log them; we don't panic
/// because a missing integration is a soft failure (bell detection still
/// works without it).
pub fn install_local() -> std::io::Result<()> {
    let Some(dir) = integration_dir() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no $HOME — can't install integration",
        ));
    };
    write_files(&dir)
}

/// Write all three integration files into `base` (which gets created
/// along with the `zsh` subdir). Shared between local install and the
/// per-host `pre-install on connect` path that may want to write into
/// a temp dir before scp'ing.
fn write_files(base: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(base.join("zsh"))?;
    for (name, body) in ZSH_FILES {
        std::fs::write(base.join("zsh").join(name), body)?;
    }
    std::fs::write(base.join("bash"), BASH)?;
    std::fs::write(base.join("fish"), FISH)?;
    Ok(())
}

/// Build the shell snippet that recreates the integration files at the
/// far end of an SSH session. Run as a oneshot before the helmd bridge
/// is opened. Idempotent overwrite — same rationale as the local install.
///
/// Uses base64 + a bash decoding step so script content can contain
/// arbitrary bytes (single quotes, dollar signs, the OSC 133 escape
/// sequences themselves) without quoting headaches. The remote needs
/// `base64` available, which is in coreutils on every platform we care
/// about.
pub fn remote_install_command() -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let mut cmd = String::from(r#"mkdir -p "$HOME/.helm/integration/zsh""#);
    let mut add = |rel: &str, body: &str| {
        let b64 = STANDARD.encode(body);
        cmd.push_str(&format!(
            " && \\\necho \"{b64}\" | base64 -d > \"$HOME/.helm/integration/{rel}\""
        ));
    };
    for (name, body) in ZSH_FILES {
        add(&format!("zsh/{name}"), body);
    }
    add("bash", BASH);
    add("fish", FISH);
    cmd
}

#[cfg(test)]
mod tests {
    //! These run a real `zsh` the way helmd spawns one — ZDOTDIR pointed
    //! at the installed shim, HELM_INTEGRATION set — against a throwaway
    //! $HOME whose dotfiles each leave a fingerprint. The system files
    //! (/etc/zprofile, /etc/zshrc) run too, exactly as in production.
    use super::*;
    use std::process::Command;

    /// The marker each user dotfile appends to `HELM_T_ORDER`, so a test
    /// can assert which files ran and in what order.
    fn dotfile(tag: &str, extra: &str) -> String {
        format!("export HELM_T_ORDER=\"${{HELM_T_ORDER}}{tag},\"\n{extra}\n")
    }

    struct FakeHome {
        dir: PathBuf,
    }

    impl FakeHome {
        /// A home with the full set of zsh dotfiles under `rel` (`""` for
        /// `$HOME` itself, `.config/zsh` for the relocated layout) and the
        /// shim installed under `.helm/integration`.
        fn new(test: &str, rel: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("helm-zsh-integration-{test}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(rel)).unwrap();
            write_files(&dir.join(".helm/integration")).unwrap();
            let home = Self { dir };
            let d = home.dir.join(rel);
            std::fs::write(
                d.join(".zshenv"),
                dotfile("env", r#"export PATH="$HOME/from-zshenv:$PATH""#),
            )
            .unwrap();
            std::fs::write(
                d.join(".zprofile"),
                dotfile("profile", r#"export PATH="$HOME/from-zprofile:$PATH""#),
            )
            .unwrap();
            std::fs::write(d.join(".zshrc"), dotfile("rc", r#"HELM_T_RC_ZDOTDIR="$ZDOTDIR""#))
                .unwrap();
            std::fs::write(d.join(".zlogin"), dotfile("login", "")).unwrap();
            home
        }

        fn shim(&self) -> PathBuf {
            self.dir.join(".helm/integration/zsh")
        }

        /// Spawn zsh with `flags` (e.g. `-lic`) and helmd's environment;
        /// returns the fingerprint lines the probe prints.
        fn run(&self, flags: &str, integration: bool) -> Probe {
            let probe = r#"printf '%s
' "$HELM_T_ORDER" "$PATH" "$ZDOTDIR" "$HELM_T_RC_ZDOTDIR" "$HISTFILE" "${precmd_functions[*]}" "$HELM_USER_ZDOTDIR""#;
            let mut cmd = Command::new("zsh");
            cmd.arg(flags)
                .arg(probe)
                .env_clear()
                .env("HOME", &self.dir)
                .env("PATH", "/usr/bin:/bin")
                .env("TERM", "xterm-256color")
                .env("ZDOTDIR", self.shim())
                .env("HELM_USER_ZDOTDIR", &self.dir);
            if integration {
                cmd.env("HELM_INTEGRATION", "1");
            }
            let out = cmd.output().expect("spawn zsh");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let lines: Vec<String> = stdout.lines().map(str::to_string).collect();
            assert!(
                out.status.success() && lines.len() == 7,
                "zsh {flags} failed: {:?}\nstdout: {stdout}\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            Probe {
                order: lines[0].clone(),
                path: lines[1].clone(),
                zdotdir: lines[2].clone(),
                rc_saw_zdotdir: lines[3].clone(),
                histfile: lines[4].clone(),
                precmd: lines[5].clone(),
                user_zdotdir: lines[6].clone(),
            }
        }
    }

    impl Drop for FakeHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    struct Probe {
        order: String,
        path: String,
        zdotdir: String,
        rc_saw_zdotdir: String,
        histfile: String,
        precmd: String,
        user_zdotdir: String,
    }

    impl Probe {
        /// Is `$HOME/<dir>` on PATH? Position-agnostic: macOS's
        /// /etc/zprofile runs path_helper, which moves system paths to
        /// the front and user entries to the back — in every terminal.
        fn has_path_entry(&self, dir: &str) -> bool {
            self.path.split(':').any(|p| p.ends_with(&format!("/{dir}")))
        }
    }

    fn have_zsh() -> bool {
        let ok = Command::new("zsh").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
        if !ok {
            eprintln!("zsh not installed; skipping integration test");
        }
        ok
    }

    #[test]
    fn interactive_login_shell_runs_every_user_dotfile_in_order() {
        if !have_zsh() {
            return;
        }
        let home = FakeHome::new("login", "");
        let p = home.run("-lic", true);
        assert_eq!(p.order, "env,profile,rc,login,");
        // The whole point: what .zshenv/.zprofile put on PATH survives.
        assert!(p.has_path_entry("from-zshenv"), "PATH lost .zshenv: {}", p.path);
        assert!(p.has_path_entry("from-zprofile"), "PATH lost .zprofile: {}", p.path);
        // The user's .zshrc saw its own directory, not our shim.
        assert_eq!(p.rc_saw_zdotdir, home.dir.to_string_lossy());
        // ...and ZDOTDIR is left restored for child shells.
        assert_eq!(p.zdotdir, home.dir.to_string_lossy());
        // History belongs to the user, not the shim (macOS /etc/zshrc
        // sets HISTFILE from ZDOTDIR before we run).
        assert!(
            !p.histfile.starts_with(&*home.shim().to_string_lossy()),
            "HISTFILE points into the shim: {}",
            p.histfile
        );
        assert!(p.precmd.contains("__helm_precmd"), "hooks not installed: {:?}", p.precmd);
    }

    #[test]
    fn non_interactive_login_shell_gets_user_files_and_no_hooks() {
        if !have_zsh() {
            return;
        }
        let home = FakeHome::new("script", "");
        let p = home.run("-lc", true);
        assert_eq!(p.order, "env,profile,login,");
        assert!(p.has_path_entry("from-zshenv"), "PATH lost .zshenv: {}", p.path);
        assert!(p.has_path_entry("from-zprofile"), "PATH lost .zprofile: {}", p.path);
        assert_eq!(p.zdotdir, home.dir.to_string_lossy());
        assert!(p.precmd.is_empty(), "hooks in a non-interactive shell: {}", p.precmd);
    }

    #[test]
    fn zshenv_that_relocates_zdotdir_is_honoured() {
        if !have_zsh() {
            return;
        }
        // The ~/.config/zsh layout: ~/.zshenv only points at the real dir.
        let home = FakeHome::new("relocated", ".config/zsh");
        let real = home.dir.join(".config/zsh");
        std::fs::write(
            home.dir.join(".zshenv"),
            format!("export ZDOTDIR=\"{}\"\nsource \"$ZDOTDIR/.zshenv\"\n", real.display()),
        )
        .unwrap();
        let p = home.run("-lic", true);
        assert_eq!(p.order, "env,profile,rc,login,");
        assert_eq!(p.zdotdir, real.to_string_lossy());
        assert_eq!(p.user_zdotdir, real.to_string_lossy());
        assert_eq!(p.rc_saw_zdotdir, real.to_string_lossy());
        assert!(p.precmd.contains("__helm_precmd"));
    }

    #[test]
    fn without_helm_integration_the_shim_is_transparent() {
        if !have_zsh() {
            return;
        }
        let home = FakeHome::new("plain", "");
        let p = home.run("-lic", false);
        assert_eq!(p.order, "env,profile,rc,login,");
        assert_eq!(p.zdotdir, home.dir.to_string_lossy());
        assert!(p.precmd.is_empty());
    }

    #[test]
    fn remote_install_writes_every_zsh_file() {
        let cmd = remote_install_command();
        for (name, _) in ZSH_FILES {
            assert!(cmd.contains(&format!("/.helm/integration/zsh/{name}\"")), "missing {name}");
        }
        assert!(cmd.contains("/.helm/integration/bash\""));
        assert!(cmd.contains("/.helm/integration/fish\""));
    }
}
