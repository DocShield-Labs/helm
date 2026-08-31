//! System-shell adjacencies: thin shims that hand off to the OS so the
//! frontend doesn't have to spawn its own processes.

use std::process::Command;

/// Reveal `path` in the OS file manager (Finder on macOS, the default
/// handler elsewhere). Fire-and-forget — we wait for the spawn to
/// succeed but not for the GUI process to exit.
#[tauri::command]
#[specta::specta]
pub fn reveal_in_finder(path: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let cmd = Command::new("open").arg(&path).spawn();
    #[cfg(target_os = "linux")]
    let cmd = Command::new("xdg-open").arg(&path).spawn();
    #[cfg(target_os = "windows")]
    let cmd = Command::new("explorer").arg(&path).spawn();

    cmd.map(|_| ()).map_err(|e| e.to_string())
}

/// Open `url` in the user's default browser. The Tauri webview blocks
/// `window.open` for security, so terminal link clicks have to round-trip
/// through here. Validates scheme to prevent shell injection via crafted
/// `file://` or arbitrary schemes that could resolve to local paths.
#[tauri::command]
#[specta::specta]
pub fn open_url(url: String) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")) {
        return Err(format!("refused to open non-web URL: {url}"));
    }

    #[cfg(target_os = "macos")]
    let cmd = Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "linux")]
    let cmd = Command::new("xdg-open").arg(&url).spawn();
    #[cfg(target_os = "windows")]
    let cmd = Command::new("cmd").args(["/C", "start", "", &url]).spawn();

    cmd.map(|_| ()).map_err(|e| e.to_string())
}

/// Dev-only frontend performance telemetry: the webview aggregates its
/// main-thread timings (src/lib/perf.ts) and ships them here so they
/// land in the dev process stdout, where tooling can read them without
/// attaching an inspector to the WKWebView. Cheap and inert in release
/// builds — the frontend only sends in dev.
#[tauri::command]
#[specta::specta]
pub fn perf_report(report: String) -> Result<(), String> {
    tracing::info!(target: "helm_perf", "{report}");
    Ok(())
}

/// Consent from the update dialog: restart daemons (with session
/// resurrection) on hosts whose daemon is older than this app.
#[tauri::command]
#[specta::specta]
pub fn set_daemon_auto_upgrade(enabled: bool) -> Result<(), String> {
    crate::connection::set_daemon_auto_upgrade(enabled);
    Ok(())
}
