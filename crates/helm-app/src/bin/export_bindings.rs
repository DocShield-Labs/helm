//! Standalone binary that regenerates `src/types/bindings.ts` without
//! launching the Tauri runtime. Run via `cargo run --bin export-bindings`.

fn main() {
    helm_app_lib::export_bindings();
    println!("ok · src/types/bindings.ts regenerated");
}
