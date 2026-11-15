# Reka

Perekam layar native (Rust + egui, tanpa web/Tauri) dengan mode **Studio** (zoom otomatis ke klik, kursor halus, latar gradien) dan **subtitle otomatis** (Whisper) dengan font kustom. Rekaman diunggah ke server bawaan dan langsung dapat link.

Windows, macOS, Linux (X11/Wayland).

## Jalankan

Butuh Rust stable dan `ffmpeg` (atau taruh `ffmpeg` di samping binary / set `REKA_FFMPEG`).

```bash
cargo run --release -p reka-desktop                       # tanpa auto-subtitle
cargo run --release -p reka-desktop --features whisper    # dengan Whisper (butuh cmake)
```

Dengan `whisper`, set `WHISPER_DONT_GENERATE_BINDINGS=1` supaya tidak perlu libclang. Model (tiny/base/small) diunduh dari HuggingFace saat pertama dipakai; model `.bin` sendiri juga bisa dipilih.

Linux butuh paket: `libx11-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libwayland-dev libxdo-dev libxtst-dev libxi-dev`.

## Pakai

- Hotkey global: `Alt+Shift+L` mulai/stop, `Alt+Shift+P` jeda, `Alt+Shift+C` batal.
- Setelah stop, editor terbuka: atur tampilan Studio, buat/impor/ekspor `.srt`, ubah gaya subtitle (font .ttf/.otf, warna, outline, kotak, posisi), lalu **Ekspor & unggah**.
- Data di `%LOCALAPPDATA%\Reka` / `~/Library/Application Support/Reka` / `~/.local/share/Reka` (ubah dengan `REKA_DATA`).
- Di Linux, bar kontrol tidak disembunyikan dari rekaman layar (tidak ada API-nya); pakai hotkey.

## Installer

```bash
cargo install cargo-packager --locked
bash scripts/fetch-ffmpeg.sh win32-x64   # atau darwin-arm64 | darwin-x64 | linux-x64
cargo build --release -p reka-desktop --features whisper
cd desktop && cargo packager --release   # nsis / dmg / deb / appimage
```

Windows (Git Bash): butuh cmake dan Visual Studio Build Tools; jangan build dari path `\\wsl.localhost` (set `CARGO_INCREMENTAL=0` bila terpaksa).
GitHub Actions (`.github/workflows/release.yml`) membangun semua installer saat tag `v*` di-push.

## CLI Studio

```bash
reka-studio export in.mp4 events.json out.mp4 [settings.json] [subs.srt]
reka-studio frame  in.mp4 events.json 1500 out.png [settings.json] [subs.srt]
```
