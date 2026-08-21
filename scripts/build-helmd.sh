#!/usr/bin/env bash
# Build the helmd daemon in release mode and stage it where Tauri's
# `bundle.externalBin` expects sidecars: `binaries/helmd-<target-triple>`
# next to tauri.conf.json. Tauri copies it into the .app as `helmd`,
# side by side with the main binary — exactly where helm-app looks for
# it (`helmd_bin_path` → sibling of current_exe).
set -euo pipefail
cd "$(dirname "$0")/.."
TRIPLE="${HELM_TARGET_TRIPLE:-$(rustc -vV | sed -n 's/^host: //p')}"
cargo build -p helmd --release
mkdir -p crates/helm-app/binaries
cp "target/release/helmd" "crates/helm-app/binaries/helmd-${TRIPLE}"
echo "staged crates/helm-app/binaries/helmd-${TRIPLE}"
