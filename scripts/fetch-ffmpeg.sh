#!/usr/bin/env bash
# Download a static ffmpeg build and place it where Tauri's `externalBin` expects it:
#   desktop/binaries/ffmpeg-<rust-target-triple>[.exe]
# Usage: scripts/fetch-ffmpeg.sh <asset> <target-triple>
#   asset: darwin-arm64 | darwin-x64 | win32-x64   (from eugeneware/ffmpeg-static, GPL builds with libx264)
set -euo pipefail
ASSET="$1"; TRIPLE="$2"; TAG="${FFMPEG_STATIC_TAG:-b6.1.1}"
EXT=""; [[ "$TRIPLE" == *windows* ]] && EXT=".exe"
OUT="desktop/binaries/ffmpeg-${TRIPLE}${EXT}"
mkdir -p desktop/binaries
curl -fsSL "https://github.com/eugeneware/ffmpeg-static/releases/download/${TAG}/ffmpeg-${ASSET}.gz" | gunzip > "$OUT"
chmod +x "$OUT"
echo "ffmpeg -> $OUT ($(wc -c < "$OUT") bytes)"
