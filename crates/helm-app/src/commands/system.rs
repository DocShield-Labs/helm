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

/// One-shot diagnostic snapshot, pretty JSON, for pasting into a bug
/// report: app/protocol versions, and per host the connected daemon's
/// version, negotiated extensions, and session geometry. Built for the
/// classic failure shape — an updated app quietly attached to an older
/// daemon, so extension-backed features (slash commands, fuzzy file
/// search, in-place retirement) come back empty.
#[tauri::command]
#[specta::specta]
pub async fn diagnostics(
    state: tauri::State<'_, crate::state::AppState>,
) -> Result<String, String> {
    let entries: Vec<_> = state.hosts.iter().map(|e| e.value().clone()).collect();
    let mut hosts = Vec::new();
    for entry in entries {
        let guard = entry.lock().await;
        let host = &guard.host;
        let mut report = serde_json::json!({
            "name": host.name,
            "kind": if host.port == 0 { "local" } else { "remote" },
            "status": format!("{:?}", guard.status),
            "retired_generation_socket": host.retired.as_ref().map(|r| r.socket.clone()),
        });
        if let Some(session) = &guard.session {
            report["daemon_version"] = session.daemon_version.clone().into();
            {
                let caps = session.capabilities.read();
                report["daemon_compat_baseline"] = caps.compatibility_baseline.into();
                let mut extensions: Vec<String> = caps.extensions.iter().cloned().collect();
                extensions.sort();
                report["daemon_extensions"] = extensions.into();
            }
            let tree = session.tree.lock();
            report["sessions"] = tree
                .sessions
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "id": s.id.to_string(),
                        "name": s.name,
                        "cols": s.cols,
                        "rows": s.rows,
                        "alt_screen": s.alt_screen,
                        "command": s.command,
                    })
                })
                .collect::<Vec<_>>()
                .into();
        }
        hosts.push(report);
    }
    let report = serde_json::json!({
        "app_version": env!("CARGO_PKG_VERSION"),
        "protocol_version": helm_proto::PROTOCOL_VERSION,
        "compatibility_baseline": helm_proto::COMPATIBILITY_BASELINE,
        "socket_override": std::env::var("HELM_SOCKET").ok(),
        "retired_sockets_on_disk": crate::connection::local_retired_sockets(),
        "hosts": hosts,
    });
    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())
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
