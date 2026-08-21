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
//!     shell it spawns to our wrapper directory; our `.zshrc` restores
//!     the user's real ZDOTDIR (`HELM_USER_ZDOTDIR`), sources their real
//!     `.zshrc`, then installs the hooks. Zero user action.
//!   - **bash / fish** have no equivalent of ZDOTDIR. The script is
//!     written to disk; phase 4D will surface a one-time toast asking
//!     the user to add a single `source` line to their rc file. Bell
//!     detection still works without integration.

use std::path::{Path, PathBuf};

/// `~/.helm/integration/zsh/.zshrc` — sourced automatically because
/// helmd sets `ZDOTDIR=~/.helm/integration/zsh` on spawned shells. Restores the
/// user's real ZDOTDIR (captured into HELM_USER_ZDOTDIR before we
/// clobbered it) and sources their real `.zshrc`, then registers the
/// OSC 133 hooks.
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
    std::fs::write(base.join("zsh").join(".zshrc"), ZSH_RC)?;
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
    let zsh_b64 = STANDARD.encode(ZSH_RC);
    let bash_b64 = STANDARD.encode(BASH);
    let fish_b64 = STANDARD.encode(FISH);
    format!(
        r#"mkdir -p "$HOME/.helm/integration/zsh" && \
echo "{zsh_b64}" | base64 -d > "$HOME/.helm/integration/zsh/.zshrc" && \
echo "{bash_b64}" | base64 -d > "$HOME/.helm/integration/bash" && \
echo "{fish_b64}" | base64 -d > "$HOME/.helm/integration/fish""#,
    )
}
