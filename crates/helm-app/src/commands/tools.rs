//! Tool-integration commands. Surface for the install/uninstall/dismiss
//! flow + the list endpoint a future settings panel will consume. The
//! suggestion toast that proactively prompts the user is pushed via
//! `HostEvent::ToolIntegrationSuggested` from `crate::tool_integrations`,
//! not through this module.

use helm_domain::HostId;
use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::state::AppState;

/// One row in the integration list returned to the frontend.
#[derive(Debug, Clone, Serialize, Type)]
pub struct ToolIntegrationStatus {
    pub id: String,
    pub name: String,
    pub description: String,
    pub post_install_note: String,
    pub installed: bool,
    /// True for hosts where this integration's install path supports
    /// the host's locality (port == 0 for local-only integrations).
    /// Frontend grays out unsupported rows in any future settings UI.
    pub supported: bool,
}

/// Snapshot the available integrations + their per-host install
/// status. Used by a future settings panel to render install/uninstall
/// toggles. The suggestion toast doesn't go through this path — it's
/// pushed via `HostEvent::ToolIntegrationSuggested`.
#[tauri::command]
#[specta::specta]
pub async fn tool_integrations_list(
    state: State<'_, AppState>,
    host_id: HostId,
) -> Result<Vec<ToolIntegrationStatus>, String> {
    let entry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let (host, primary, ssh) = {
        let g = entry.lock().await;
        (g.host.clone(), g.primary_client(), g.ssh.clone())
    };
    let primary = primary.ok_or_else(|| "host not connected".to_string())?;

    let mut out = Vec::new();
    for integration in crate::tool_integrations::registry() {
        let installed = integration
            .is_installed(&host, &primary, ssh.as_ref())
            .await
            .unwrap_or(false);
        out.push(ToolIntegrationStatus {
            id: integration.id().to_string(),
            name: integration.name().to_string(),
            description: integration.description().to_string(),
            post_install_note: integration.post_install_note().to_string(),
            installed,
            supported: true,
        });
    }
    Ok(out)
}

/// Install a tool integration on `host_id`. Idempotent — calling
/// install when already installed is fine.
#[tauri::command]
#[specta::specta]
pub async fn tool_integration_install(
    state: State<'_, AppState>,
    host_id: HostId,
    integration_id: String,
) -> Result<(), String> {
    let entry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let (host, primary, ssh) = {
        let g = entry.lock().await;
        (g.host.clone(), g.primary_client(), g.ssh.clone())
    };
    let primary = primary.ok_or_else(|| "host not connected".to_string())?;
    let integration = crate::tool_integrations::find(&integration_id)
        .ok_or_else(|| format!("unknown integration: {integration_id}"))?;
    integration.install(&host, &primary, ssh.as_ref()).await?;
    // Mark as seen so we don't re-prompt mid-session for the same
    // tool — install satisfies the "we've nagged the user" contract.
    state
        .tool_integration_seen
        .insert((host_id, integration_id), ());
    Ok(())
}

/// Uninstall a tool integration on `host_id`. Idempotent.
#[tauri::command]
#[specta::specta]
pub async fn tool_integration_uninstall(
    state: State<'_, AppState>,
    host_id: HostId,
    integration_id: String,
) -> Result<(), String> {
    let entry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let (host, primary, ssh) = {
        let g = entry.lock().await;
        (g.host.clone(), g.primary_client(), g.ssh.clone())
    };
    let primary = primary.ok_or_else(|| "host not connected".to_string())?;
    let integration = crate::tool_integrations::find(&integration_id)
        .ok_or_else(|| format!("unknown integration: {integration_id}"))?;
    integration.uninstall(&host, &primary, ssh.as_ref()).await
}

/// Suppress further suggestion toasts for `(host_id, integration_id)`
/// for the rest of this app session. Called when the user clicks
/// "Not now" on the toast. Cleared at app restart so the user gets
/// the prompt again next time — they may have changed their mind.
#[tauri::command]
#[specta::specta]
pub async fn tool_integration_dismiss(
    state: State<'_, AppState>,
    host_id: HostId,
    integration_id: String,
) -> Result<(), String> {
    state
        .tool_integration_seen
        .insert((host_id, integration_id), ());
    Ok(())
}
