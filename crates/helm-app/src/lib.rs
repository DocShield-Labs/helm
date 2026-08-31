//! Helm — Tauri entry crate.
//!
//! Wires helmd (via helm-proto) and helm-ssh into the Tauri runtime, owns
//! the global app state, and exposes commands + channels to the frontend.

mod commands;
mod connection;
mod integration;
mod keychain;
mod notifications;
mod persistence;
mod power;
mod reachability;
mod state;
mod titlebar;
mod tool_integrations;

use specta_typescript::{BigIntExportBehavior, Typescript};
use tauri::Manager;
use tauri_specta::{collect_commands, Builder};

/// Build the tauri-specta `Builder` with every command registered.
/// Both `run()` and `export_bindings()` start from this so the bindings can't
/// drift out of sync with what the runtime actually exposes.
fn specta_builder() -> Builder<tauri::Wry> {
    use helm_proto::{attrs, modes};
    use std::collections::BTreeMap;
    Builder::<tauri::Wry>::new()
        // Wire bit layouts the frontend must agree on, generated into
        // bindings.ts so they can't drift from helm-proto.
        .constant(
            "ATTRS",
            BTreeMap::from([
                ("BOLD", attrs::BOLD),
                ("DIM", attrs::DIM),
                ("ITALIC", attrs::ITALIC),
                ("UNDERLINE", attrs::UNDERLINE),
                ("INVERSE", attrs::INVERSE),
                ("STRIKE", attrs::STRIKE),
                ("HIDDEN", attrs::HIDDEN),
                ("DOUBLE_UNDERLINE", attrs::DOUBLE_UNDERLINE),
                ("UNDERCURL", attrs::UNDERCURL),
            ]),
        )
        .constant(
            "MODES",
            BTreeMap::from([
                ("APP_CURSOR", modes::APP_CURSOR),
                ("APP_KEYPAD", modes::APP_KEYPAD),
                ("BRACKETED_PASTE", modes::BRACKETED_PASTE),
                ("FOCUS_IN_OUT", modes::FOCUS_IN_OUT),
                ("MOUSE_CLICK", modes::MOUSE_CLICK),
                ("MOUSE_DRAG", modes::MOUSE_DRAG),
                ("MOUSE_MOTION", modes::MOUSE_MOTION),
                ("SGR_MOUSE", modes::SGR_MOUSE),
                ("UTF8_MOUSE", modes::UTF8_MOUSE),
                ("ALT_SCREEN", modes::ALT_SCREEN),
                ("ALTERNATE_SCROLL", modes::ALTERNATE_SCROLL),
            ]),
        )
        .constant("TRUECOLOR_FLAG", connection::TRUECOLOR_FLAG)
        // The top bar's height is also where macOS's traffic lights get
        // centred, so both sides read it from here.
        .constant("TITLE_BAR_HEIGHT", titlebar::TITLE_BAR_HEIGHT)
        .constant(
            "TITLE_BAR_CONTENT_INSET",
            titlebar::TITLE_BAR_CONTENT_INSET,
        )
        .constant("MAX_HISTORY_PAGE", helm_proto::MAX_HISTORY_PAGE)
        .commands(collect_commands![
            commands::host::ping,
            commands::host::host_list,
            commands::host::host_local_id,
            commands::host::host_save,
            commands::host::host_delete,
            commands::host::host_save_password,
            commands::host::ssh_config_aliases,
            commands::host::host_subscribe,
            commands::host::host_connect,
            commands::host::host_disconnect,
            commands::host::host_key_prompt_response,
            commands::session::session_tree,
            commands::session::session_input,
            commands::session::session_resize,
            commands::session::session_screen,
            commands::session::session_history,
            commands::session::session_new,
            commands::session::session_kill,
            commands::session::session_rename,
            commands::session::session_search,
            commands::session::session_blocks,
            commands::session::session_path_complete,
            commands::session::session_agent_commands,
            commands::session::session_file_search,
            commands::session::session_ping,
            commands::notifications::notifications_list,
            commands::notifications::notification_dismiss,
            commands::notifications::notification_dismiss_for_session,
            commands::notifications::set_focus,
            commands::tools::tool_integrations_list,
            commands::tools::tool_integration_install,
            commands::tools::tool_integration_uninstall,
            commands::tools::tool_integration_dismiss,
            commands::system::reveal_in_finder,
            commands::system::open_url,
            commands::system::perf_report,
            commands::system::set_daemon_auto_upgrade,
        ])
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
    // shipped. Idempotent overwrite — keeps ~/.helm/integration in
    // lockstep with the binary. helmd points every shell it spawns at
    // these via ZDOTDIR. Soft failure: log and continue.
    if let Err(e) = integration::install_local() {
        tracing::warn!("shell integration install failed: {e}");
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
        // Self-update: updater checks the release manifest and swaps the
        // .app bundle; process provides the relaunch after install.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(specta.invoke_handler())
        .setup(move |app| {
            specta.mount_events(app);
            // macOS only: take over the traffic lights from tauri.conf.json,
            // which cannot hold their alignment across a screen change.
            if let Some(window) = app.get_webview_window("main") {
                titlebar::install(&window);
            } else {
                tracing::warn!("no `main` window at setup; traffic lights left to macOS");
            }
            Ok(())
        })
        .manage(state::AppState::default())
        .build(tauri::generate_context!())
        .expect("error while building Helm");

    app.run(|app_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            // Drop every host's helmd session (the daemon and its
            // processes keep running for the next launch) and abort
            // reconnect supervisors so nothing revives during shutdown.
            let state: tauri::State<state::AppState> = app_handle.state();
            for entry in state.hosts.iter() {
                if let Ok(mut guard) = entry.value().try_lock() {
                    guard.voluntary_disconnect = true;
                    if let Some(handle) = guard.supervisor.take() {
                        handle.abort();
                    }
                    guard.shutdown_session();
                }
            }
        }
    });
}
