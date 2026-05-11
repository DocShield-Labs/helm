//! Helm — Tauri entry crate.
//!
//! Wires helm-pty / helm-tmux / helm-ssh into the Tauri runtime, owns the
//! global app state, and exposes commands + channels to the frontend.

mod anchor;
mod commands;
mod connection;
mod db;
mod integration;
mod keychain;
mod notifications;
mod reachability;
mod scheduler;
mod state;
mod subscriber;
mod tool_integrations;

use specta_typescript::{BigIntExportBehavior, Typescript};
use tauri::Manager;
use tauri_specta::{collect_commands, Builder};

/// Build the tauri-specta `Builder` with every command registered.
/// Both `run()` and `export_bindings()` start from this so the bindings can't
/// drift out of sync with what the runtime actually exposes.
fn specta_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new().commands(collect_commands![
        commands::host::ping,
        commands::host::host_list,
        commands::host::host_local_id,
        commands::host::host_save,
        commands::host::host_delete,
        commands::host::host_set_anchor,
        commands::host::host_save_password,
        commands::host::ssh_config_aliases,
        commands::host::host_subscribe,
        commands::host::host_connect,
        commands::host::host_disconnect,
        commands::host::host_key_prompt_response,
        commands::tmux::tmux_send_keys,
        commands::tmux::tmux_resize_pane,
        commands::tmux::tmux_new_window,
        commands::tmux::tmux_split_pane,
        commands::tmux::tmux_kill_window,
        commands::tmux::tmux_select_window,
        commands::tmux::tmux_select_pane,
        commands::tmux::tmux_rename_window,
        commands::tmux::tmux_list_windows,
        commands::tmux::tmux_list_panes,
        commands::tmux::tmux_list_sessions,
        commands::tmux::tmux_new_session,
        commands::tmux::tmux_kill_session,
        commands::tmux::tmux_rename_session,
        commands::tmux::tmux_switch_client,
        commands::tmux::tmux_capture_pane,
        commands::tmux::tmux_resize_client,
        commands::notifications::notifications_list,
        commands::notifications::notification_dismiss,
        commands::notifications::notification_dismiss_for_window,
        commands::notifications::set_focus,
        commands::tools::tool_integrations_list,
        commands::tools::tool_integration_install,
        commands::tools::tool_integration_uninstall,
        commands::tools::tool_integration_dismiss,
        commands::system::reveal_in_finder,
        commands::system::open_url,
        commands::fs::fs_list_dir,
        commands::schedule::schedule_list,
        commands::schedule::schedule_save,
        commands::schedule::schedule_delete,
        commands::schedule::schedule_set_enabled,
        commands::schedule::schedule_run_now,
        commands::schedule::schedule_runs,
        commands::anchor::anchor_probe,
    ])
}

/// Entry point for `helm anchor-rpc`. Stdio↔unix-socket proxy that
/// subscribers run via SSH to reach the anchor's RPC server. Returns
/// the process exit code.
pub fn anchor_rpc_main() -> i32 {
    anchor::run_stdio_proxy()
}

/// Regenerate `src/types/bindings.ts`. Run via `cargo run --bin export-bindings`.
pub fn export_bindings() {
    specta_builder()
        .export(
            Typescript::default()
                .header("// @ts-nocheck\n")
                // u64 timestamps (unix ms) sit comfortably under JS's
                // Number.MAX_SAFE_INTEGER (2^53). Emitting as `number`
                // avoids wrapping every timestamp in a BigInt at the
                // call site.
                .bigint(BigIntExportBehavior::Number),
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../src/types/bindings.ts"),
        )
        .expect("failed to export specta bindings");
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,helm=debug".into()),
        )
        .init();

    // Refresh the on-disk integration scripts to whatever this build
    // shipped. Idempotent overwrite — cheap, and keeps the user's
    // ~/.helm/integration tree in lockstep with the binary they just
    // launched. Soft failure: if HOME is missing or the directory
    // can't be created, we just log and continue without integration
    // (bell detection still works).
    if let Err(e) = integration::install_local() {
        tracing::warn!("shell integration install failed: {e}");
    }

    // Set the integration env vars in helm's own process env so the
    // *very first* tmux server we spawn inherits them at server start.
    // Without this, the bootstrap session's first pane (which gets
    // launched as part of tmux server startup, before we can
    // `set-environment` anything) wouldn't have ZDOTDIR set and would
    // miss zsh integration. configure_tmux_env still runs on every
    // connect to keep tmux's server-global env in sync for later
    // panes, but this is the only way to reach the bootstrap pane.
    //
    // Safety: std::env::set_var mutates only this process's env block
    // — not the user's shell. tmux + spawn_local inherit our env, so
    // the value flows through.
    if let Some(home) = dirs::home_dir() {
        let zsh_dir = home.join(".helm").join("integration").join("zsh");
        let user_zdotdir = std::env::var("ZDOTDIR")
            .unwrap_or_else(|_| home.to_string_lossy().into_owned());
        // SAFETY: single-threaded boot, no env iteration in flight.
        unsafe {
            std::env::set_var("HELM_INTEGRATION", "1");
            std::env::set_var("HELM_USER_ZDOTDIR", &user_zdotdir);
            std::env::set_var("ZDOTDIR", &zsh_dir);
        }
    }

    let specta = specta_builder();

    // In debug, regenerate the TS bindings on every cold start so they
    // never drift while iterating.
    #[cfg(debug_assertions)]
    specta
        .export(
            Typescript::default()
                .header("// @ts-nocheck\n")
                // u64 timestamps (unix ms) sit comfortably under JS's
                // Number.MAX_SAFE_INTEGER (2^53). Emitting as `number`
                // avoids wrapping every timestamp in a BigInt at the
                // call site.
                .bigint(BigIntExportBehavior::Number),
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../src/types/bindings.ts"),
        )
        .expect("failed to export specta bindings");

    let app = tauri::Builder::default()
        .invoke_handler(specta.invoke_handler())
        .setup(move |app| {
            specta.mount_events(app);
            // Spawn the schedule supervisor. It's safe to start before
            // host_subscribe wires up the frontend channel — emitted
            // events will silently drop until the channel exists, and
            // schedules whose first fire is more than a second away
            // give the frontend plenty of time to subscribe in practice.
            scheduler::spawn_supervisor(app.handle());
            // If localhost is the anchor on this machine, start the
            // RPC server so subscribers (and our future
            // SSH-piped transport in 1c) can talk to us. Skipped
            // when localhost isn't the anchor; the
            // `host_set_anchor` command starts/stops on flip.
            let state = app.state::<state::AppState>();
            if let Some(local) = state.hosts.get(&state.local_host_id) {
                if local.try_lock().map(|g| g.host.is_anchor).unwrap_or(false) {
                    match anchor::spawn(app.handle()) {
                        Ok(handle) => {
                            *state.anchor_server.lock() = Some(handle);
                        }
                        Err(e) => tracing::warn!("anchor RPC start failed: {e}"),
                    }
                }
            }
            Ok(())
        })
        .manage(state::AppState::open().expect("open helm.db"))
        .build(tauri::generate_context!())
        .expect("error while building Helm");

    app.run(|app_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            // Drop every host's tmux client so each `Drop` impl SIGKILLs its
            // `-CC` process. The tmux *server* (and SSH session) keeps
            // running with state intact for the next launch. Also abort
            // any reconnect supervisors so they don't try to revive
            // connections during shutdown.
            let state: tauri::State<state::AppState> = app_handle.state();
            for entry in state.hosts.iter() {
                if let Ok(mut guard) = entry.value().try_lock() {
                    guard.voluntary_disconnect = true;
                    if let Some(handle) = guard.supervisor.take() {
                        handle.abort();
                    }
                    guard.shutdown_clients();
                }
            }
        }
    });
}
