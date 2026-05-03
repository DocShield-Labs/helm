//! Helm — Tauri entry crate.
//!
//! Wires helm-pty / helm-tmux / helm-ssh into the Tauri runtime, owns the
//! global app state, and exposes commands + channels to the frontend.

mod commands;
mod keychain;
mod persistence;
mod reachability;
mod state;

use specta_typescript::Typescript;
use tauri::Manager;
use tauri_specta::{collect_commands, Builder};

/// Build the tauri-specta `Builder` with every command registered.
/// Both `run()` and `export_bindings()` start from this so the bindings can't
/// drift out of sync with what the runtime actually exposes.
fn specta_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new().commands(collect_commands![
        commands::ping,
        commands::host_list,
        commands::host_local_id,
        commands::host_save,
        commands::host_delete,
        commands::host_save_password,
        commands::ssh_config_aliases,
        commands::host_subscribe,
        commands::host_connect,
        commands::host_disconnect,
        commands::host_key_prompt_response,
        commands::tmux_send_keys,
        commands::tmux_resize_pane,
        commands::tmux_new_window,
        commands::tmux_split_pane,
        commands::tmux_kill_window,
        commands::tmux_select_window,
        commands::tmux_select_pane,
        commands::tmux_rename_window,
        commands::tmux_list_windows,
        commands::tmux_list_panes,
        commands::tmux_list_sessions,
        commands::tmux_new_session,
        commands::tmux_kill_session,
        commands::tmux_rename_session,
        commands::tmux_switch_client,
        commands::tmux_capture_pane,
        commands::tmux_resize_client,
    ])
}

/// Regenerate `src/types/bindings.ts`. Run via `cargo run --bin export-bindings`.
pub fn export_bindings() {
    specta_builder()
        .export(
            Typescript::default().header("// @ts-nocheck\n"),
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

    let specta = specta_builder();

    // In debug, regenerate the TS bindings on every cold start so they
    // never drift while iterating.
    #[cfg(debug_assertions)]
    specta
        .export(
            Typescript::default().header("// @ts-nocheck\n"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../src/types/bindings.ts"),
        )
        .expect("failed to export specta bindings");

    let app = tauri::Builder::default()
        .invoke_handler(specta.invoke_handler())
        .setup(move |app| {
            specta.mount_events(app);
            Ok(())
        })
        .manage(state::AppState::default())
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
                    guard.tmux = None;
                    guard.ssh = None;
                }
            }
        }
    });
}
