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
