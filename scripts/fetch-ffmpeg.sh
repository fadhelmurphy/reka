#!/usr/bin/env bash
# Unduh ffmpeg statis ke desktop/binaries/ffmpeg[.exe] supaya ikut dibundel oleh cargo-packager.
# Pakai: scripts/fetch-ffmpeg.sh <asset>   asset: darwin-arm64 | darwin-x64 | win32-x64 | linux-x64
# (build GPL dari eugeneware/ffmpeg-static, ada libx264)
set -euo pipefail
ASSET="$1"; TAG="${FFMPEG_STATIC_TAG:-b6.1.1}"
EXT=""; [[ "$ASSET" == win32* ]] && EXT=".exe"
OUT="desktop/binaries/ffmpeg${EXT}"
mkdir -p desktop/binaries
curl -fsSL "https://github.com/eugeneware/ffmpeg-static/releases/download/${TAG}/ffmpeg-${ASSET}.gz" | gunzip > "$OUT"
chmod +x "$OUT"
echo "ffmpeg -> $OUT ($(wc -c < "$OUT") bytes)"
