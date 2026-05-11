//! Per-machine install steps that run at every helm boot. Today this
//! is just the launcher symlink — `~/.helm/bin/helm` → the running
//! binary — so the anchor RPC's SSH-piped transport can find `helm
//! anchor-rpc` when a subscriber execs against this machine.
//!
//! All operations are idempotent and soft-fail: a missing HOME or a
//! read-only `~/.helm` shouldn't prevent helm from booting. We just
//! log and continue.

use std::path::PathBuf;
use tracing::{info, warn};

const HELM_DIR: &str = ".helm";
const BIN_DIR: &str = "bin";
const LAUNCHER_NAME: &str = "helm";

/// Resolve `~/.helm/bin/helm`. Creates the parent directory on demand.
fn launcher_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "no home dir".to_string())?;
    let dir = home.join(HELM_DIR).join(BIN_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {:?}: {e}", dir))?;
    Ok(dir.join(LAUNCHER_NAME))
}

/// Make sure `~/.helm/bin/helm` points at the current binary. Refreshed
/// on every boot so dev rebuilds (where `current_exe()` moves around)
/// stay current, while prod installs (stable `/Applications/Helm.app/.../helm`)
/// settle into a steady symlink.
///
/// Specifically:
///   - If the path is a symlink pointing at the current exe → no-op.
///   - If it's a symlink pointing elsewhere → replace it.
///   - If it's a regular file we didn't put there → leave it alone (user
///     may have manually installed a different shim).
///   - Otherwise → create the symlink.
#[cfg(unix)]
fn ensure_launcher_symlink() -> Result<PathBuf, String> {
    let link_path = launcher_path()?;
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    match std::fs::read_link(&link_path) {
        Ok(existing) if existing == exe => return Ok(link_path),
        Ok(_) => {
            // Symlink, but to the wrong place. Replace it.
            std::fs::remove_file(&link_path)
                .map_err(|e| format!("remove stale link: {e}"))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No file there; create the link below.
        }
        Err(_) => {
            // Either it's a regular file (read_link errors) or some
            // other unreadable thing. Check existence; if the path
            // does exist but isn't a symlink, leave it alone.
            if link_path.exists() {
                warn!(
                    "{:?} exists but isn't our symlink — leaving it untouched. \
                     If subscribers can't find `helm`, replace or remove it.",
                    link_path
                );
                return Ok(link_path);
            }
        }
    }
    std::os::unix::fs::symlink(&exe, &link_path)
        .map_err(|e| format!("symlink {:?} → {:?}: {e}", link_path, exe))?;
    Ok(link_path)
}

#[cfg(not(unix))]
fn ensure_launcher_symlink() -> Result<PathBuf, String> {
    // Anchor RPC's SSH transport is Unix-only for v1 (unix sockets).
    // The launcher install is a no-op on Windows — the rest of helm
    // works fine, just the anchor-side server doesn't run.
    Err("launcher symlink is Unix-only".into())
}

/// Check whether `~/.helm/bin` is on the current process's PATH. If
/// not, log a one-time hint with the exact line to add to the user's
/// login shell rc file. We don't auto-edit rc files — that crosses an
/// invasiveness line — but we make the fix one copy-paste away.
fn warn_if_not_on_path(launcher_dir: &std::path::Path) {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = std::env::split_paths(&path).any(|p| p == launcher_dir);
    if on_path {
        return;
    }
    warn!(
        "anchor RPC: `helm` launcher installed at {:?}, but this directory \
         isn't on your PATH. Subscribers reach this machine over SSH by \
         running `helm anchor-rpc`, which needs the binary discoverable in \
         the remote login shell. Add this line to ~/.zprofile (or \
         ~/.bash_profile / ~/.profile):\n  \
         export PATH=\"$HOME/.helm/bin:$PATH\"",
        launcher_dir,
    );
}

/// Top-level entry — call once at boot. Soft-fails: a broken install
/// path logs and otherwise lets the app continue.
pub fn ensure_launcher() {
    match ensure_launcher_symlink() {
        Ok(link_path) => {
            info!("helm launcher: {:?}", link_path);
            if let Some(parent) = link_path.parent() {
                warn_if_not_on_path(parent);
            }
        }
        Err(e) => {
            warn!("helm launcher install failed: {e}");
        }
    }
}
