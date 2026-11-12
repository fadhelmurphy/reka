// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod devices;
mod input;
mod recorder;

use recorder::{RecOpts, Screen, Session};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

const LOCAL_PORT: u16 = 47800;

#[derive(Default, Serialize, Deserialize, Clone)]
struct Config {
    /// Remote reka server for sharing with other people. `None` = embedded local server.
    server_url: Option<String>,
}

struct AppState {
    session: Mutex<Option<Session>>,
    busy: Mutex<bool>, // countdown / stopping in progress
    config: Mutex<Config>,
    config_path: PathBuf,
    rec_dir: PathBuf,
    local_url: Mutex<String>,
    /// Set while a Studio-mode recording is running: the screen its mouse events refer to.
    studio: Mutex<Option<Screen>>,
}

impl AppState {
    fn server_url(&self) -> String {
        self.config
            .lock()
            .unwrap()
            .server_url
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.local_url.lock().unwrap().clone())
            .trim_end_matches('/')
            .to_string()
    }
}

#[derive(Clone, Serialize)]
struct RecState {
    state: &'static str, // recording | paused | processing | idle
    elapsed_ms: u64,
}

fn emit_state(app: &AppHandle, state: &'static str, elapsed: Duration) {
    let _ = app.emit("rec-state", RecState { state, elapsed_ms: elapsed.as_millis() as u64 });
}

// ---------------------------------------------------------------- commands

#[tauri::command]
async fn get_devices(app: AppHandle) -> devices::Devices {
    let a = app.clone();
    tauri::async_runtime::spawn_blocking(move || devices::list(&a)).await.unwrap()
}

#[derive(Serialize)]
struct ConfigView {
    server_url: String,
    custom_url: Option<String>,
    local_url: String,
}

#[tauri::command]
fn get_config(state: tauri::State<AppState>) -> ConfigView {
    ConfigView {
        server_url: state.server_url(),
        custom_url: state.config.lock().unwrap().server_url.clone(),
        local_url: state.local_url.lock().unwrap().clone(),
    }
}

#[tauri::command]
fn set_config(state: tauri::State<AppState>, server_url: Option<String>) -> Result<(), String> {
    let mut c = state.config.lock().unwrap();
    c.server_url = server_url.map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty());
    std::fs::write(&state.config_path, serde_json::to_vec_pretty(&*c).unwrap()).map_err(|e| e.to_string())
}

#[tauri::command]
async fn start_recording(
    app: AppHandle,
    screen: Screen,
    mic: Option<String>,
    camera: Option<String>,
    studio: Option<bool>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    {
        let mut busy = state.busy.lock().unwrap();
        if *busy || state.session.lock().unwrap().is_some() {
            return Err("Sedang merekam".into());
        }
        *busy = true;
    }
    let res = begin(&app, screen, mic, camera, studio.unwrap_or(false)).await;
    *state.busy.lock().unwrap() = false;
    if res.is_err() {
        hide_overlays(&app);
        show_main(&app);
    }
    res
}

async fn begin(app: &AppHandle, screen: Screen, mic: Option<String>, camera: Option<String>, studio: bool) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    if let Some(cam) = camera {
        if let Some(b) = app.get_webview_window("bubble") {
            let _ = b.show();
            let _ = b.emit("bubble-camera", Some(cam));
        }
    }
    if let Some(bar) = app.get_webview_window("bar") {
        let _ = bar.show();
    }
    for n in (1..=3).rev() {
        let _ = app.emit("rec-countdown", n);
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    let _ = app.emit("rec-countdown", 0);

    let dir = app.state::<AppState>().rec_dir.join(format!("session-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    let studio_screen = studio.then(|| screen.clone());
    if studio {
        // Start the mouse log just before ffmpeg so no early movement is missed.
        input::begin(initial_cursor(app)).map_err(|e| format!("Mode Studio tidak bisa membaca mouse: {e}"))?;
    }
    let opts = RecOpts { screen, mic, hide_cursor: studio };
    let started = tauri::async_runtime::spawn_blocking(move || Session::start(opts, dir)).await;
    let session = match started {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            input::end();
            return Err(e);
        }
        Err(e) => {
            input::end();
            return Err(e.to_string());
        }
    };
    *app.state::<AppState>().studio.lock().unwrap() = studio_screen;
    *app.state::<AppState>().session.lock().unwrap() = Some(session);
    // X11 cannot exclude a window from a screen grab, so on Linux the bar would end up in the
    // video. Hide it; stop with Alt+Shift+L, pause/cancel from the tray menu.
    #[cfg(target_os = "linux")]
    if let Some(bar) = app.get_webview_window("bar") {
        let _ = bar.hide();
    }
    emit_state(app, "recording", Duration::ZERO);
    Ok(())
}

#[tauri::command]
async fn pause_recording(app: AppHandle) -> Result<(), String> {
    let a = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let st = a.state::<AppState>();
        let mut g = st.session.lock().unwrap();
        let s = g.as_mut().ok_or("Tidak sedang merekam")?;
        if s.is_paused() {
            s.resume()?;
            emit_state(&a, "recording", s.elapsed());
        } else {
            s.pause();
            emit_state(&a, "paused", s.elapsed());
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Clone, Serialize)]
struct Finished {
    path: String,
    duration_ms: u64,
    /// Present for Studio recordings: the mouse log to feed the editor.
    events_path: Option<String>,
}

#[tauri::command]
async fn stop_recording(app: AppHandle) -> Result<(), String> {
    let session = app.state::<AppState>().session.lock().unwrap().take();
    let Some(session) = session else { return Ok(()) };
    // Stop logging now; whatever happens while ffmpeg finalises is outside the video anyway.
    let studio = app.state::<AppState>().studio.lock().unwrap().take();
    let raw = studio.as_ref().map(|_| input::end());
    emit_state(&app, "processing", session.elapsed());
    let out = app.state::<AppState>().rec_dir.join(format!("rec-{}.mp4", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    let res = tauri::async_runtime::spawn_blocking(move || session.finish(&out)).await.map_err(|e| e.to_string())?;
    hide_overlays(&app);
    show_main(&app);
    emit_state(&app, "idle", Duration::ZERO);
    let (path, dur, spans) = res?;
    let events_path = match (studio, raw) {
        (Some(screen), Some(raw)) => {
            let events = input::map(&raw, &spans, &screen);
            let p = path.with_extension("events.json");
            std::fs::write(&p, serde_json::to_vec(&events).unwrap()).map_err(|e| e.to_string())?;
            Some(p.to_string_lossy().into_owned())
        }
        _ => None,
    };
    let _ = app.emit_to("main", "recording-finished", Finished {
        path: path.to_string_lossy().into_owned(),
        duration_ms: dur.as_millis() as u64,
        events_path,
    });
    Ok(())
}

#[tauri::command]
async fn cancel_recording(app: AppHandle) -> Result<(), String> {
    if app.state::<AppState>().studio.lock().unwrap().take().is_some() {
        input::end();
    }
    let session = app.state::<AppState>().session.lock().unwrap().take();
    if let Some(s) = session {
        tauri::async_runtime::spawn_blocking(move || s.cancel()).await.map_err(|e| e.to_string())?;
    }
    hide_overlays(&app);
    show_main(&app);
    emit_state(&app, "idle", Duration::ZERO);
    Ok(())
}

/// Upload with progress events, copy the share link and open it in the browser.
#[tauri::command]
async fn upload_recording(app: AppHandle, path: String, title: String, duration_ms: u64) -> Result<serde_json::Value, String> {
    use futures_util::StreamExt;
    let base = app.state::<AppState>().server_url();
    let file = tokio::fs::File::open(&path).await.map_err(|e| e.to_string())?;
    let total = file.metadata().await.map_err(|e| e.to_string())?.len();
    let mut sent = 0u64;
    let mut last_pct = 0u64;
    let a = app.clone();
    let stream = tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024).inspect(move |chunk| {
        if let Ok(c) = chunk {
            sent += c.len() as u64;
            let pct = sent * 100 / total.max(1);
            if pct != last_pct {
                last_pct = pct;
                let _ = a.emit_to("main", "upload-progress", pct);
            }
        }
    });
    let part = reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(stream), total)
        .file_name("recording.mp4")
        .mime_str("video/mp4")
        .unwrap();
    let form = reqwest::multipart::Form::new()
        .text("title", title)
        .text("duration_ms", duration_ms.to_string())
        .part("file", part);
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/videos"))
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("Upload gagal: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Server menolak upload ({}): {}", resp.status(), resp.text().await.unwrap_or_default()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let link = format!("{base}{}", v["share_url"].as_str().unwrap_or(""));
    let _ = app.clipboard().write_text(link.clone());
    let _ = tauri_plugin_opener::open_url(&link, None::<&str>);
    let _ = tokio::fs::remove_file(&path).await;
    Ok(v)
}

#[tauri::command]
async fn discard_recording(app: AppHandle, path: String) -> Result<(), String> {
    // Only ever delete inside our own recordings folder.
    let p = PathBuf::from(&path);
    if !p.starts_with(&app.state::<AppState>().rec_dir) {
        return Err("path di luar folder rekaman".into());
    }
    let _ = tokio::fs::remove_file(p.with_extension("events.json")).await;
    tokio::fs::remove_file(p).await.map_err(|e| e.to_string())
}

fn in_rec_dir(app: &AppHandle, p: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(p);
    if p.starts_with(&app.state::<AppState>().rec_dir) {
        Ok(p)
    } else {
        Err("path di luar folder rekaman".into())
    }
}

fn load_events(p: &std::path::Path) -> Result<Vec<reka_studio::InputEvent>, String> {
    serde_json::from_slice(&std::fs::read(p).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// One composed frame (PNG data URL) so the editor can show the current look at time `t_ms`.
#[tauri::command]
async fn studio_preview(
    app: AppHandle,
    path: String,
    events_path: String,
    settings: reka_studio::Settings,
    t_ms: u64,
) -> Result<String, String> {
    use base64::Engine;
    let (video, events) = (in_rec_dir(&app, &path)?, in_rec_dir(&app, &events_path)?);
    tauri::async_runtime::spawn_blocking(move || {
        let ev = load_events(&events)?;
        let png = reka_studio::preview(&recorder::ffmpeg_bin(), &video, &ev, &settings, t_ms, 1100)?;
        Ok::<_, String>(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Render the styled video next to the raw one, then drop the raw video and its mouse log.
#[tauri::command]
async fn studio_export(
    app: AppHandle,
    path: String,
    events_path: String,
    settings: reka_studio::Settings,
) -> Result<String, String> {
    let (video, events) = (in_rec_dir(&app, &path)?, in_rec_dir(&app, &events_path)?);
    let out = video.with_file_name(format!("{}-studio.mp4", video.file_stem().unwrap_or_default().to_string_lossy()));
    let (a, v, e, o) = (app.clone(), video.clone(), events.clone(), out.clone());
    tauri::async_runtime::spawn_blocking(move || {
        let ev = load_events(&e)?;
        reka_studio::export(&recorder::ffmpeg_bin(), &v, &ev, &settings, &o, |pct| {
            let _ = a.emit_to("main", "studio-progress", pct);
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = std::fs::remove_file(&video);
    let _ = std::fs::remove_file(&events);
    Ok(out.to_string_lossy().into_owned())
}

/// Pointer position in the coordinate space the mouse hook reports (points on macOS).
fn initial_cursor(app: &AppHandle) -> Option<(f64, f64)> {
    let p = app.cursor_position().ok()?;
    #[cfg(target_os = "macos")]
    {
        let sf = app.primary_monitor().ok().flatten()?.scale_factor();
        return Some((p.x / sf, p.y / sf));
    }
    #[cfg(not(target_os = "macos"))]
    Some((p.x, p.y))
}

#[tauri::command]
fn copy_text(app: AppHandle, text: String) -> Result<(), String> {
    app.clipboard().write_text(text).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("hanya http(s)".into());
    }
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- windows

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn hide_overlays(app: &AppHandle) {
    if let Some(b) = app.get_webview_window("bubble") {
        let _ = b.emit("bubble-camera", None::<String>); // release the camera
        let _ = b.hide();
    }
    if let Some(b) = app.get_webview_window("bar") {
        let _ = b.hide();
    }
}

fn overlay(app: &AppHandle, label: &str, page: &str, w: f64, h: f64) -> tauri::Result<WebviewWindow> {
    let b = WebviewWindowBuilder::new(app, label, WebviewUrl::App(page.into()))
        .title("Reka")
        .inner_size(w, h)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible_on_all_workspaces(true)
        .shadow(false)
        .focused(false)
        .visible(false)
        .transparent(true); // needs `macOSPrivateApi` on macOS
    b.build()
}

/// Place a window relative to the primary monitor's bottom edge.
/// Uses the requested logical size: a hidden window may report 0×0 on Linux.
fn place_bottom(win: &WebviewWindow, w: f64, h: f64, center: bool, margin_logical: f64) {
    let Ok(Some(m)) = win.primary_monitor() else { return };
    let sf = m.scale_factor();
    let (mp, ms) = (m.position(), m.size());
    let ws = PhysicalSize::new((w * sf) as u32, (h * sf) as u32);
    let margin = (margin_logical * sf) as i32;
    let x = if center { mp.x + (ms.width as i32 - ws.width as i32) / 2 } else { mp.x + margin };
    let y = mp.y + ms.height as i32 - ws.height as i32 - margin;
    let _ = win.set_position(PhysicalPosition::new(x, y));
}

/// WebKitGTK ships with getUserMedia off and denies permission requests by default.
fn enable_media(win: &WebviewWindow) {
    #[cfg(target_os = "linux")]
    let _ = win.with_webview(|wv| {
        use webkit2gtk::{PermissionRequestExt, SettingsExt, WebViewExt};
        let view = wv.inner();
        if let Some(s) = view.settings() {
            s.set_enable_media_stream(true);
            s.set_enable_mediasource(true);
        }
        view.connect_permission_request(|_, req| {
            req.allow();
            true
        });
    });
    #[cfg(not(target_os = "linux"))]
    let _ = win;
}

async fn toggle_from_shortcut(app: AppHandle) {
    let recording = app.state::<AppState>().session.lock().unwrap().is_some();
    if recording {
        let _ = stop_recording(app).await;
    } else if !*app.state::<AppState>().busy.lock().unwrap() {
        show_main(&app);
        let _ = app.emit_to("main", "shortcut-start", ());
    }
}

// ---------------------------------------------------------------- main

fn main() {
    let shortcut = Shortcut::new(Some(Modifiers::ALT | Modifiers::SHIFT), Code::KeyL);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, sc, ev| {
                    if sc == &shortcut && ev.state() == ShortcutState::Pressed {
                        tauri::async_runtime::spawn(toggle_from_shortcut(app.clone()));
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let h = app.handle().clone();
            let data_dir = app.path().app_data_dir()?;
            let rec_dir = app.path().app_cache_dir()?.join("recordings");
            std::fs::create_dir_all(&rec_dir)?;
            std::fs::create_dir_all(&data_dir)?;
            let config_path = data_dir.join("config.json");
            let config: Config = std::fs::read(&config_path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();

            // Embedded server: fixed port so share links stay valid across restarts.
            let listener = std::net::TcpListener::bind(("127.0.0.1", LOCAL_PORT))
                .or_else(|_| std::net::TcpListener::bind(("127.0.0.1", 0)))?;
            listener.set_nonblocking(true)?;
            let local_url = format!("http://{}", listener.local_addr()?);
            let server_dir = data_dir.join("server");
            tauri::async_runtime::spawn(async move {
                let l = tokio::net::TcpListener::from_std(listener).expect("listener");
                if let Err(e) = reka_server::serve(l, &server_dir).await {
                    eprintln!("embedded server stopped: {e}");
                }
            });

            app.manage(AppState {
                session: Mutex::new(None),
                busy: Mutex::new(false),
                config: Mutex::new(config),
                config_path,
                rec_dir,
                local_url: Mutex::new(local_url),
                studio: Mutex::new(None),
            });

            let bubble = overlay(&h, "bubble", "bubble.html", 200.0, 200.0)?;
            place_bottom(&bubble, 200.0, 200.0, false, 40.0);
            let bar = overlay(&h, "bar", "bar.html", 330.0, 64.0)?;
            // Keep the control bar out of the recording (macOS + Windows).
            let _ = bar.set_content_protected(true);
            place_bottom(&bar, 330.0, 64.0, true, 48.0);
            for w in [&bubble, &bar] {
                enable_media(w);
            }
            if let Some(main) = h.get_webview_window("main") {
                enable_media(&main);
            }

            // Tray
            let rec_item = MenuItem::with_id(app, "toggle", "Rekam / Berhenti  (Alt+Shift+L)", true, None::<&str>)?;
            let open_item = MenuItem::with_id(app, "open", "Buka Reka", true, None::<&str>)?;
            let pause_item = MenuItem::with_id(app, "pause", "Jeda / lanjut", true, None::<&str>)?;
            let cancel_item = MenuItem::with_id(app, "cancel", "Batalkan rekaman", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Keluar", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&rec_item, &pause_item, &cancel_item, &open_item, &quit_item])?;
            TrayIconBuilder::with_id("tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Reka")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "toggle" => {
                        tauri::async_runtime::spawn(toggle_from_shortcut(app.clone()));
                    }
                    "pause" => {
                        tauri::async_runtime::spawn(pause_recording(app.clone()));
                    }
                    "cancel" => {
                        tauri::async_runtime::spawn(cancel_recording(app.clone()));
                    }
                    "open" => show_main(app),
                    "quit" => {
                        if let Some(s) = app.state::<AppState>().session.lock().unwrap().take() {
                            s.cancel();
                        }
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            if let Err(e) = app.global_shortcut().register(shortcut) {
                eprintln!("shortcut Alt+Shift+L tidak bisa didaftarkan: {e}");
            }
            Ok(())
        })
        .on_window_event(|win, ev| {
            // Closing the panel keeps the app alive in the tray, like other screen recorders.
            if let WindowEvent::CloseRequested { api, .. } = ev {
                if win.label() == "main" {
                    api.prevent_close();
                    let _ = win.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_devices,
            get_config,
            set_config,
            start_recording,
            pause_recording,
            stop_recording,
            cancel_recording,
            upload_recording,
            discard_recording,
            studio_preview,
            studio_export,
            copy_text,
            open_url
        ])
        .run(tauri::generate_context!())
        .expect("error while running reka desktop");
}
