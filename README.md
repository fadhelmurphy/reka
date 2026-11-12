# reka

Perekam layar dalam Rust (perekam + editor gaya Screen Studio). Dua bagian dalam satu workspace:

| Crate | Isi |
|---|---|
| `server/` | Server berbagi video (Axum + SQLite): upload, halaman share `/v/:id`, library web, perekam browser |
| `studio/` | Mesin render gaya Screen Studio (auto-zoom, kursor halus, background) + CLI `reka-studio` |
| `desktop/` | Aplikasi desktop Tauri v2 untuk macOS, Windows, Linux. Server di atas ikut tertanam di dalamnya |

## Installer

| OS | File | ffmpeg |
|---|---|---|
| Windows | `Reka_x.y.z_x64-setup.exe` / `.msi` | sudah di dalam installer |
| macOS | `Reka_x.y.z_aarch64.dmg` (Apple Silicon) / `_x64.dmg` (Intel) | sudah di dalam installer |
| Linux | `.deb` / `.rpm` | dipasang otomatis dari repo distro |
| Linux | `.AppImage` | butuh `ffmpeg` di sistem |

**Membuat installer**: push repo ke GitHub, lalu `git tag v0.1.0 && git push --tags` (atau jalankan workflow *Release installers* secara manual). GitHub Actions membangun keempat target dan mengunggahnya ke **draft Release**.

Installer belum ditandatangani (butuh Apple Developer ID / sertifikat Windows berbayar):
- macOS: klik kanan → Open, atau `xattr -cr /Applications/Reka.app`
- Windows: SmartScreen → *More info* → *Run anyway*

## Aplikasi desktop

**Fitur**
- Rekam layar native lewat ffmpeg, pilih monitor dan mikrofon. Di Linux bisa juga audio sistem (`.monitor`)
- Bubble kamera bulat yang selalu di atas: seret untuk memindah, klik dua kali untuk ganti ukuran. Karena tampil di layar, bubble ikut terekam
- Bar kontrol mengambang: timer, jeda/lanjut, batal, selesai. Di macOS/Windows bar ini **tidak ikut terekam** (content protection)
- Hitung mundur 3 detik, shortcut global **Alt+Shift+L**, ikon tray, dan aplikasi tetap hidup di tray saat jendela ditutup
- Setelah rekam: beri judul → upload → link tersalin ke clipboard dan terbuka di browser
- Server lokal tertanam di `127.0.0.1:47800`. Untuk berbagi ke orang lain, isi **Server** dengan URL server `reka` publik

**Prasyarat**
- Rust stable + [prasyarat Tauri](https://v2.tauri.app/start/prerequisites/) untuk OS kamu
- **ffmpeg** dengan libx264:
  - macOS: `brew install ffmpeg`
  - Windows: `winget install Gyan.FFmpeg`
  - Linux: `sudo apt install ffmpeg`

  Aplikasi mencari ffmpeg di `REKA_FFMPEG`, di samping file aplikasi, di `/opt/homebrew/bin` / `/usr/local/bin`, lalu di PATH.

**Jalankan / build**
```bash
cargo run -p reka-desktop               # dev
cargo install tauri-cli --version "^2"  # sekali saja
cd desktop && cargo tauri build         # .dmg/.app, .msi/.exe, .deb/.AppImage
```

**Catatan per OS**
- **macOS**: saat pertama merekam, izinkan *Screen Recording* (dan Camera/Microphone) di System Settings → Privacy & Security, lalu buka ulang aplikasi. Pilihan "mikrofon" adalah perangkat avfoundation. Untuk audio sistem pakai BlackHole.
- **Windows**: perekaman layar memakai `gdigrab` dan mikrofon memakai `dshow`. Kalau display scaling ≠ 100% lalu hasil rekaman terpotong, coba set ffmpeg ke "Override high DPI scaling".
- **Linux**: memakai `x11grab` + PulseAudio/PipeWire. Di **Wayland**, x11grab hanya menangkap aplikasi XWayland, jadi pakai sesi X11. Bar kontrol disembunyikan saat merekam (X11 tidak bisa mengecualikan jendela dari tangkapan); hentikan dengan Alt+Shift+L.

**Bundling ffmpeg (opsional)**: tambahkan `"externalBin": ["binaries/ffmpeg"]` di `bundle` (`tauri.conf.json`), lalu taruh `binaries/ffmpeg-<target-triple>` (mis. `ffmpeg-aarch64-apple-darwin`). Tauri menyalinnya ke samping file aplikasi, dan aplikasi otomatis menemukannya.

## Mode Studio (gaya Screen Studio)

Centang **Mode Studio** sebelum merekam. Layar direkam **tanpa kursor**, dan posisi mouse + klik dicatat terpisah (ketikan keyboard *tidak* pernah dicatat). Setelah selesai, editor terbuka:

- **Zoom otomatis** ke tiap klik: kamera mulai bergerak 0,5 dtk sebelum klik, bertahan 1,6 dtk, klik berdekatan digabung, kamera mengikuti kursor dengan dead-zone supaya tidak goyang
- **Kursor digambar ulang** (halus, bisa dibesarkan) + efek riak saat klik
- **Tampilan**: background gradien (6 preset), padding, sudut membulat, bayangan
- Preview langsung + slider waktu, lalu **Export → upload** seperti biasa. Audio ikut dibawa

Catatan:
- macOS: butuh izin *Accessibility* dan *Input Monitoring* untuk membaca mouse
- Linux: butuh sesi X11 (di Wayland murni mouse tidak terbaca). Bar kontrol disembunyikan saat merekam karena X11 tidak bisa mengecualikan jendela dari tangkapan; **hentikan dengan Alt+Shift+L**, jeda/batal dari menu tray
- Belum ada: timeline untuk mengedit segmen zoom satu per satu, trim, dan pilihan rasio output. Zoom sepenuhnya otomatis
- CLI untuk menyetel tampilan tanpa aplikasi: `reka-studio export in.mp4 events.json out.mp4 [settings.json]`

## Server saja (untuk berbagi)
```bash
cargo run --release -p reka-server      # http://localhost:3000
```
Env: `REKA_ADDR` (default `0.0.0.0:3000`), `REKA_DATA` (default `./data`). Taruh di belakang HTTPS (Caddy/nginx) supaya perekam browser jalan dan link bisa dibagikan.

| Method | Path | Ket |
|---|---|---|
| GET | `/api/videos` | list |
| POST | `/api/videos` | multipart: `file`, `title?`, `duration_ms?` |
| GET/PATCH/DELETE | `/api/videos/:id` | detail / `{"title"}` / hapus |
| POST | `/api/videos/:id/view` | +1 view |
| GET | `/media/:id.{mp4,webm}` | file video (mendukung Range) |

⚠️ Belum ada autentikasi. Tambahkan sebelum server dibuka ke publik.

## Tes
```bash
cargo test -p reka-studio --release                          # kamera, render, parser ffmpeg
cargo test -p reka-desktop                                   # parser perangkat + pemetaan event mouse
xvfb-run cargo test -p reka-desktop -- --ignored --nocapture # rekam, jeda, gabung, tangkap mouse nyata, cek sinkron (Linux)
```
