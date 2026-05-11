//! On-disk persistence for user-defined schedules.
//!
//! Lives at `~/Library/Application Support/Helm/schedules.json` on
//! macOS (or whatever `dirs::config_dir()` resolves to elsewhere).
//! Mirrors the atomic write-then-rename pattern used by `persistence`
//! for `hosts.json`.
//!
//! Local-only in v1: schedules are owned by the helm instance that
//! created them and only fire while that instance is running. The
//! cloud/synced story is deferred and explicitly tracked alongside the
//! same work for notifications.

use helm_domain::Schedule;
use std::path::PathBuf;
use tracing::warn;

const APP_DIR: &str = "Helm";
const SCHEDULES_FILE: &str = "schedules.json";

/// Resolve the on-disk path for `schedules.json`, creating the parent
/// directory if needed. Same parent dir as `hosts.json`.
pub fn schedules_path() -> Result<PathBuf, String> {
    let base = dirs::config_dir().ok_or_else(|| "could not locate config dir".to_string())?;
    let dir = base.join(APP_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create_dir({:?}): {e}", dir))?;
    Ok(dir.join(SCHEDULES_FILE))
}

/// Load the persisted schedule list. Missing file → empty list (fresh
/// install). Parse failures error so we don't silently lose user data.
pub fn load_schedules() -> Result<Vec<Schedule>, String> {
    let path = schedules_path()?;
    if !path.exists() {
        return Ok(vec![]);
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("read({:?}): {e}", path))?;
    if bytes.is_empty() {
        return Ok(vec![]);
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("parse({:?}): {e}", path))
}

/// Atomic write: serialize to a sibling `.tmp`, then rename. Same
/// guarantee as `save_hosts` — readers see either the old or new file,
/// never a torn write.
pub fn save_schedules(schedules: &[Schedule]) -> Result<(), String> {
    let path = schedules_path()?;
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(schedules).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&tmp, bytes).map_err(|e| format!("write({:?}): {e}", tmp))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename({:?} → {:?}): {e}", tmp, path))?;
    Ok(())
}

/// Best-effort wrapper used at boot — log + start empty rather than
/// crash if the file is unreadable.
pub fn try_load_schedules() -> Vec<Schedule> {
    match load_schedules() {
        Ok(s) => s,
        Err(e) => {
            warn!("schedules.json load failed: {e} — starting with empty registry");
            Vec::new()
        }
    }
}
