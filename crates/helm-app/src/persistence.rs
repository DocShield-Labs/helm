//! On-disk persistence for the host registry.
//!
//! Lives at `~/Library/Application Support/Helm/hosts.json` on macOS
//! (or whatever `dirs::config_dir()` resolves to elsewhere). Writes are
//! atomic — temp file + rename — so a crash mid-save can't leave a torn
//! file behind.
//!
//! Localhost is *not* serialized. Its identity is intentionally
//! per-process (a fresh UUID each boot) so the localhost entry never
//! drifts out of sync with the runtime; `AppState::default()` always
//! recreates it. Persisting it would just create a stale id mismatch.

use helm_domain::Host;
use std::path::PathBuf;
use tracing::warn;

const APP_DIR: &str = "Helm";
const HOSTS_FILE: &str = "hosts.json";

/// Resolve the on-disk path for `hosts.json`, creating the parent
/// directory if needed.
pub fn hosts_path() -> Result<PathBuf, String> {
    let base = dirs::config_dir().ok_or_else(|| "could not locate config dir".to_string())?;
    let dir = base.join(APP_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create_dir({:?}): {e}", dir))?;
    Ok(dir.join(HOSTS_FILE))
}

/// Load the persisted host list. Missing file is treated as "no hosts
/// yet" rather than an error — a fresh install just has no hosts.json.
/// Parse failures *do* error so we don't silently lose user data.
pub fn load_hosts() -> Result<Vec<Host>, String> {
    let path = hosts_path()?;
    if !path.exists() {
        return Ok(vec![]);
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("read({:?}): {e}", path))?;
    if bytes.is_empty() {
        return Ok(vec![]);
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("parse({:?}): {e}", path))
}

/// Atomic write: serialize to a sibling `.tmp` file, then rename over
/// the canonical path. Rename is atomic on the same filesystem, so a
/// reader will always see either the old version or the new — never
/// half-written JSON.
pub fn save_hosts(hosts: &[Host]) -> Result<(), String> {
    let path = hosts_path()?;
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(hosts).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&tmp, bytes).map_err(|e| format!("write({:?}): {e}", tmp))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename({:?} → {:?}): {e}", tmp, path))?;
    Ok(())
}

/// Best-effort wrapper used by `AppState::default()` — log on failure
/// instead of crashing app startup.
pub fn try_load_hosts() -> Vec<Host> {
    match load_hosts() {
        Ok(hosts) => hosts,
        Err(e) => {
            warn!("hosts.json load failed: {e} — starting with empty registry");
            Vec::new()
        }
    }
}
