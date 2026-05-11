// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Subcommand dispatch. Today there's exactly one — `anchor-rpc`,
    // the stdio↔unix-socket proxy a subscriber's SSH connection runs
    // on the anchor host. Anything else falls through to the GUI
    // entry point. Kept hand-rolled (no clap) so the cold start is
    // identical for the GUI path.
    let mut args = std::env::args().skip(1);
    if let Some(sub) = args.next() {
        if sub == "anchor-rpc" {
            std::process::exit(helm_app_lib::anchor_rpc_main());
        }
    }
    helm_app_lib::run()
}
