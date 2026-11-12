#!/usr/bin/env bash
# Cross-build the Windows NSIS installer from Linux (experimental; GitHub Actions on
# windows-latest is the recommended route). Needs: mingw-w64, nsis, Node.
# Normal case: `rustup target add x86_64-pc-windows-gnu` works and the build-std lines are unnecessary.
set -euo pipefail
cd "$(dirname "$0")/.."
[ -f desktop/binaries/ffmpeg-x86_64-pc-windows-gnu.exe ] || bash scripts/fetch-ffmpeg.sh win32-x64 x86_64-pc-windows-gnu
if ! rustup target list --installed | grep -q x86_64-pc-windows-gnu; then
  rustup target add x86_64-pc-windows-gnu || {
    echo "rustup target add failed; building std from source (-Zbuild-std, needs rust-src)"
    export RUSTC_BOOTSTRAP=1 CARGO_UNSTABLE_BUILD_STD=std,panic_abort
  }
fi
cd desktop && npx -y @tauri-apps/cli@2 build --target x86_64-pc-windows-gnu --bundles nsis
