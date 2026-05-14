//! Anchor-RPC subscriber probe commands. Lets the frontend (or
//! devtools) verify that the subscriber side of the RPC plane works
//! end-to-end — both the local-subprocess transport and the SSH
//! transport — without needing to wire the full subscriber UI yet.
//!
//! Removable once 1d ships the actual subscriber wiring; for 1c
//! verification it's the easiest way to prove the protocol speaks on
//! a real transport.

use helm_domain::{HostId, RpcOp, RpcResult};
use std::sync::Arc;
use tauri::State;

use crate::state::AppState;

/// Probe an anchor by opening a subscriber client, sending Hello,
/// and returning the version string the anchor reports. Picks the
/// transport based on the host: localhost uses a local subprocess
/// (talks to this same helm's anchor socket); a remote host opens an
/// SSH exec channel running `helm anchor-rpc` on the far end.
///
/// Remote hosts must already be connected (so a live `SshSession`
/// exists in the host entry) — call `host_connect` first if needed.
#[tauri::command]
#[specta::specta]
pub async fn anchor_probe(
    state: State<'_, AppState>,
    host_id: HostId,
) -> Result<String, String> {
    let client = if host_id == state.local_host_id {
        let helm_bin = std::env::current_exe()
            .map_err(|e| format!("locate helm binary: {e}"))?;
        crate::subscriber::open_local_subprocess(&helm_bin)?
    } else {
        let entry = state.entry(host_id).ok_or_else(|| "unknown host".to_string())?;
        let session: Arc<helm_ssh::SshSession> = {
            let guard = entry.lock().await;
            guard
                .ssh
                .as_ref()
                .ok_or_else(|| {
                    "host is not connected — call host_connect first".to_string()
                })?
                .clone()
        };
        crate::subscriber::open_ssh(session)?
    };

    match client
        .request(RpcOp::Hello {
            hostname: String::new(),
        })
        .await?
    {
        RpcResult::Hello { version, .. } => Ok(version),
        other => Err(format!("unexpected hello reply: {other:?}")),
    }
}
