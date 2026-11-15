//! The Reka window: home panel, recording bar, and the Studio + subtitle editor.

use crate::camera::{self, Camera};
use crate::config::{self, Config, Paths};
use crate::devices::Devices;
use crate::input;
use crate::jobs::{self, Msg, Notifier};
use crate::recorder::{RecOpts, Screen, Session};
use eframe::egui::{self, Color32, RichText, Vec2, ViewportCommand};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use reka_studio::{Caption, InputEvent, Position, Settings};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PANEL: (f32, f32) = (480.0, 760.0);
const EDITOR: (f32, f32) = (1180.0, 760.0);
const BAR: (f32, f32) = (360.0, 60.0);
const COUNTDOWN_SECS: f32 = 3.0;
const ACCENT: Color32 = Color32::from_rgb(0x6a, 0x5a, 0xf9);
const RED: Color32 = Color32::from_rgb(0xe5, 0x39, 0x35);

const PRESETS: &[(&str, &str)] = &[
    ("#6a5af9", "#d66efd"),
    ("#0f2027", "#2c5364"),
    ("#f7971e", "#ffd200"),
    ("#11998e", "#38ef7d"),
    ("#ee0979", "#ff6a00"),
    ("#232526", "#414345"),
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Home,
    Countdown(Instant),
    /// Countdown overlay is closing; capture starts once it is really gone from the screen.
    Starting(Instant),
    /// ffmpeg is being launched on a worker thread.
    Launching,
    Recording,
    Processing,
    Review,
}

#[derive(Clone, Copy, Debug)]
enum Hot {
    Toggle,
    Pause,
    Cancel,
}

enum After {
    Upload,
    Save(PathBuf),
}

enum Step {
    Idle,
    Exporting(u32),
    Uploading(u32),
    Done { link: Option<String>, saved: Option<PathBuf> },
}

struct Review {
    path: PathBuf,
    duration_ms: u64,
    events: Arc<Vec<InputEvent>>,
    studio: bool,
    /// Non-Studio recordings can still get the framed background look.
    styled: bool,
    look: Settings,
    captions: Vec<Caption>,
    t: f32,
    tex: Option<egui::TextureHandle>,
    seq: u64,
    busy: bool,
    dirty: bool,
    sig: String,
    title: String,
    transcribing: Option<(String, i32)>,
    step: Step,
    after: After,
    exported: Option<PathBuf>,
    /// Trim range in seconds.
    trim_a: f32,
    trim_b: f32,
    copied: bool,
}

impl Review {
    fn trimmed(&self) -> bool {
        self.trim_a > 0.05 || self.trim_b < self.duration_ms as f32 / 1000.0 - 0.05
    }

    fn trim_ms(&self) -> Option<(u64, u64)> {
        self.trimmed().then(|| ((self.trim_a * 1000.0) as u64, (self.trim_b * 1000.0) as u64))
    }

    fn out_duration_ms(&self) -> u64 {
        self.trim_ms().map(|(a, b)| b - a).unwrap_or(self.duration_ms)
    }

    /// The settings that actually get rendered.
    fn effective(&self) -> Settings {
        let mut s = self.look.clone();
        if !self.studio {
            s.zoom = false;
            s.cursor = false;
            s.click_fx = false;
            if !self.styled {
                s.padding = 0.0;
                s.radius = 0.0;
                s.shadow = 0.0;
            }
        }
        s
    }

    /// Whether the clip needs the compositor at all; otherwise the raw file is uploaded as is.
    fn needs_render(&self) -> bool {
        self.trimmed() || self.studio || self.styled || (self.look.captions.enabled && !self.captions.is_empty())
    }
}

pub struct App {
    ctx: egui::Context,
    rt: tokio::runtime::Runtime,
    paths: Paths,
    cfg: Config,
    n: Notifier,
    rx: Receiver<Msg>,
    hot_rx: Receiver<Hot>,
    _hotkeys: Option<GlobalHotKeyManager>,
    local_url: String,
    fonts: Vec<(String, PathBuf)>,

    devices: Devices,
    devices_loaded: bool,
    sel_screen: usize,
    sel_mic: usize,
    sel_cam: usize,

    phase: Phase,
    session: Option<Session>,
    studio_screen: Option<Screen>,
    camera: Option<Camera>,
    cam_tex: Option<egui::TextureHandle>,
    cam_ver: u64,
    review: Option<Review>,
    error: Option<String>,
    model_dl: Option<(u64, u64)>,
    show_settings: bool,
    cancel_start: bool,
}

fn fmt_time(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}", s / 60, s % 60)
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: Paths) -> Self {
        let ctx = cc.egui_ctx.clone();
        let mut visuals = egui::Visuals::dark();
        visuals.selection.bg_fill = ACCENT;
        visuals.widgets.hovered.bg_stroke.color = ACCENT;
        ctx.set_visuals(visuals);
        ctx.global_style_mut(|s| {
            s.spacing.item_spacing = Vec2::new(8.0, 8.0);
            s.spacing.button_padding = Vec2::new(12.0, 6.0);
        });

        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build().expect("tokio runtime");
        let cfg = paths.load();
        let (tx, rx) = channel();
        let n = Notifier { tx, ctx: ctx.clone() };

        // Embedded server: fixed port so share links stay valid across restarts.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 47800))
            .or_else(|_| std::net::TcpListener::bind(("127.0.0.1", 0)))
            .expect("bind local server");
        let _ = listener.set_nonblocking(true);
        let local_url = format!("http://{}", listener.local_addr().expect("addr"));
        let server_dir = paths.data.join("server");
        rt.spawn(async move {
            let l = tokio::net::TcpListener::from_std(listener).expect("listener");
            if let Err(e) = reka_server::serve(l, &server_dir).await {
                eprintln!("server lokal berhenti: {e}");
            }
        });

        // Global hotkeys work while another app is focused and the window is out of the way.
        let (hot_tx, hot_rx) = channel();
        let mods = Some(Modifiers::ALT | Modifiers::SHIFT);
        let keys = [(HotKey::new(mods, Code::KeyL), Hot::Toggle), (HotKey::new(mods, Code::KeyP), Hot::Pause), (HotKey::new(mods, Code::KeyC), Hot::Cancel)];
        let hotkeys = match GlobalHotKeyManager::new() {
            Ok(m) => {
                for (k, _) in &keys {
                    if let Err(e) = m.register(*k) {
                        eprintln!("hotkey tidak bisa didaftarkan: {e}");
                    }
                }
                let map: Vec<(u32, Hot)> = keys.iter().map(|(k, h)| (k.id(), *h)).collect();
                let c = ctx.clone();
                GlobalHotKeyEvent::set_event_handler(Some(move |ev: GlobalHotKeyEvent| {
                    if ev.state() == HotKeyState::Pressed {
                        if let Some((_, h)) = map.iter().find(|(id, _)| *id == ev.id()) {
                            let _ = hot_tx.send(*h);
                            c.request_repaint();
                        }
                    }
                }));
                Some(m)
            }
            Err(e) => {
                eprintln!("hotkey global tidak tersedia: {e}");
                None
            }
        };

        jobs::list_devices(n.clone());
        let fonts = config::system_fonts();
        App {
            ctx,
            rt,
            paths,
            cfg,
            n,
            rx,
            hot_rx,
            _hotkeys: hotkeys,
            local_url,
            fonts,
            devices: Devices::default(),
            devices_loaded: false,
            sel_screen: 0,
            sel_mic: 0,
            sel_cam: 0,
            phase: Phase::Home,
            session: None,
            studio_screen: None,
            camera: None,
            cam_tex: None,
            cam_ver: 0,
            review: None,
            error: None,
            model_dl: None,
            show_settings: false,
            cancel_start: false,
        }
    }

    // ------------------------------------------------------------ helpers

    fn server_url(&self) -> String {
        self.cfg
            .server_url
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.local_url.clone())
            .trim_end_matches('/')
            .to_string()
    }

    fn save_cfg(&mut self) {
        if let Some(r) = &self.review {
            self.cfg.look = r.look.clone();
        }
        self.paths.save(&self.cfg);
    }

    fn model_path(&self) -> Option<PathBuf> {
        let p = if self.cfg.whisper_model == "custom" {
            PathBuf::from(self.cfg.whisper_custom.clone()?)
        } else {
            self.paths.model_file(&self.cfg.whisper_model)
        };
        p.is_file().then_some(p)
    }

    fn monitor(&self) -> Vec2 {
        self.ctx.input(|i| i.viewport().monitor_size).unwrap_or(Vec2::new(1280.0, 800.0))
    }

    /// Normal window, centred on the screen.
    fn set_panel(&self, (w, h): (f32, f32)) {
        let m = self.monitor();
        let c = &self.ctx;
        c.send_viewport_cmd(ViewportCommand::ContentProtected(false));
        c.send_viewport_cmd(ViewportCommand::Decorations(true));
        c.send_viewport_cmd(ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
        c.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(w, h)));
        c.send_viewport_cmd(ViewportCommand::OuterPosition(egui::pos2(((m.x - w) / 2.0).max(0.0), ((m.y - h) / 2.0 - 20.0).max(0.0))));
    }

    /// While recording the window shrinks to a control bar. Windows and macOS can exclude it
    /// from the capture; X11 cannot, so on Linux it is parked off-screen and the hotkeys
    /// (Alt+Shift+L / P / C) are the controls.
    fn set_bar(&self) {
        let c = &self.ctx;
        c.send_viewport_cmd(ViewportCommand::Decorations(false));
        c.send_viewport_cmd(ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        if cfg!(target_os = "linux") {
            c.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(1.0, 1.0)));
            c.send_viewport_cmd(ViewportCommand::OuterPosition(egui::pos2(-4000.0, -4000.0)));
        } else {
            let m = self.monitor();
            c.send_viewport_cmd(ViewportCommand::ContentProtected(true));
            c.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(BAR.0, BAR.1)));
            c.send_viewport_cmd(ViewportCommand::OuterPosition(egui::pos2((m.x - BAR.0) / 2.0, m.y - BAR.1 - 56.0)));
        }
    }

    // ------------------------------------------------------------ recording

    fn begin_countdown(&mut self) {
        let Some(screen) = self.devices.screens.get(self.sel_screen).cloned() else {
            self.error = Some("Belum ada layar yang terdeteksi.".into());
            return;
        };
        let _ = screen;
        self.error = None;
        self.save_cfg();
        self.camera = None;
        if self.sel_cam > 0 {
            if let Some(cam) = self.devices.cameras.get(self.sel_cam - 1) {
                let c = self.ctx.clone();
                match Camera::start(&cam.id, move || c.request_repaint()) {
                    Ok(cam) => self.camera = Some(cam),
                    Err(e) => self.error = Some(e),
                }
            }
        }
        self.phase = Phase::Countdown(Instant::now());
        self.set_bar();
    }

    fn abort_to_home(&mut self) {
        self.cancel_start = self.phase == Phase::Launching;
        if let Some(s) = self.session.take() {
            std::thread::spawn(move || s.cancel());
        }
        if self.studio_screen.take().is_some() {
            input::end();
        }
        self.camera = None;
        self.phase = Phase::Home;
        self.set_panel(PANEL);
    }

    fn begin_capture(&mut self) {
        let Some(screen) = self.devices.screens.get(self.sel_screen).cloned() else { return };
        let mic = (self.sel_mic > 0).then(|| self.devices.mics.get(self.sel_mic - 1).map(|m| m.id.clone())).flatten();
        let studio = self.cfg.studio;
        let dir = self.paths.recordings.join(format!("session-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
        if studio {
            let initial = match mouse_position::mouse_position::Mouse::get_mouse_position() {
                mouse_position::mouse_position::Mouse::Position { x, y } => Some((x as f64, y as f64)),
                _ => None,
            };
            if let Err(e) = input::begin(initial) {
                self.error = Some(format!("Mode Studio tidak bisa membaca mouse: {e}"));
                return self.abort_to_home();
            }
        }
        self.studio_screen = studio.then(|| screen.clone());
        self.phase = Phase::Launching;
        self.cancel_start = false;
        // Starting ffmpeg takes about a second; keep the UI (and the camera bubble) alive meanwhile.
        let n = self.n.clone();
        std::thread::spawn(move || n.send(Msg::Started(Session::start(RecOpts { screen, mic, hide_cursor: studio }, dir))));
    }

    fn stop_recording(&mut self) {
        let Some(session) = self.session.take() else { return };
        let studio = self.studio_screen.take().map(|sc| (sc, input::end()));
        self.camera = None;
        self.phase = Phase::Processing;
        self.set_panel(PANEL);
        let out = self.paths.recordings.join(format!("rec-{}.mp4", chrono::Local::now().format("%Y%m%d-%H%M%S")));
        jobs::finish_recording(self.n.clone(), session, out, studio);
    }

    fn toggle_pause(&mut self) {
        if let Some(s) = self.session.as_mut() {
            if s.is_paused() {
                if let Err(e) = s.resume() {
                    self.error = Some(e);
                }
            } else {
                s.pause();
            }
        }
    }

    // ------------------------------------------------------------ messages

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Devices(d) => {
                self.devices = d;
                self.devices_loaded = true;
                self.sel_screen = self.sel_screen.min(self.devices.screens.len().saturating_sub(1));
            }
            Msg::Started(res) => {
                if self.cancel_start {
                    self.cancel_start = false;
                    if let Ok(sess) = res {
                        std::thread::spawn(move || sess.cancel());
                    }
                    return;
                }
                match res {
                    Ok(sess) => {
                        self.session = Some(sess);
                        self.phase = Phase::Recording;
                    }
                    Err(e) => {
                        self.error = Some(e);
                        self.abort_to_home();
                    }
                }
            }
            Msg::Stopped(Ok(st)) => {
                self.phase = Phase::Review;
                let mut look = self.cfg.look.clone();
                look.captions.enabled = self.cfg.auto_captions;
                let studio = st.events.is_some();
                self.review = Some(Review {
                    title: format!("Rekaman {}", chrono::Local::now().format("%d %b %H:%M")),
                    path: st.path,
                    duration_ms: st.duration_ms,
                    events: Arc::new(st.events.unwrap_or_default()),
                    studio,
                    styled: false,
                    look,
                    captions: vec![],
                    t: 0.0,
                    tex: None,
                    seq: 0,
                    busy: false,
                    dirty: true,
                    sig: String::new(),
                    transcribing: None,
                    step: Step::Idle,
                    after: After::Upload,
                    exported: None,
                    trim_a: 0.0,
                    trim_b: st.duration_ms as f32 / 1000.0,
                    copied: false,
                });
                self.set_panel(EDITOR);
                if self.cfg.auto_captions {
                    self.start_transcription();
                }
            }
            Msg::Stopped(Err(e)) => {
                self.error = Some(e);
                self.phase = Phase::Home;
                self.set_panel(PANEL);
            }
            Msg::Preview { seq, img } => {
                if let Some(r) = self.review.as_mut() {
                    if seq == r.seq {
                        r.busy = false;
                        match img {
                            Ok((w, h, px)) => {
                                let ci = egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], &px);
                                match r.tex.as_mut() {
                                    Some(t) => t.set(ci, egui::TextureOptions::LINEAR),
                                    None => r.tex = Some(self.ctx.load_texture("preview", ci, egui::TextureOptions::LINEAR)),
                                }
                            }
                            Err(e) => self.error = Some(format!("Preview gagal: {e}")),
                        }
                    }
                }
            }
            Msg::ExportProgress(p) => {
                if let Some(r) = self.review.as_mut() {
                    r.step = Step::Exporting(p);
                }
            }
            Msg::ExportDone(Ok(path)) => {
                let Some(r) = self.review.as_mut() else { return };
                r.exported = Some(path.clone());
                match std::mem::replace(&mut r.after, After::Upload) {
                    After::Upload => self.start_upload(path),
                    After::Save(dest) => self.finish_save(path, dest),
                }
            }
            Msg::ExportDone(Err(e)) => {
                self.error = Some(format!("Ekspor gagal: {e}"));
                if let Some(r) = self.review.as_mut() {
                    r.step = Step::Idle;
                }
            }
            Msg::Stage(s) => {
                if let Some(r) = self.review.as_mut() {
                    r.transcribing = Some((s, 0));
                }
            }
            Msg::TranscribeProgress(p) => {
                if let Some(r) = self.review.as_mut() {
                    if let Some(t) = r.transcribing.as_mut() {
                        t.1 = p;
                    }
                }
            }
            Msg::TranscribeDone(res) => {
                if let Some(r) = self.review.as_mut() {
                    r.transcribing = None;
                    match res {
                        Ok(c) if c.is_empty() => self.error = Some("Tidak ada ucapan yang terdeteksi.".into()),
                        Ok(c) => {
                            r.captions = c;
                            r.look.captions.enabled = true;
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
            }
            Msg::ModelProgress(d, t) => self.model_dl = Some((d, t)),
            Msg::ModelDone(res) => {
                self.model_dl = None;
                match res {
                    Ok(_) => {
                        // A model that arrives while an auto-subtitle recording is open gets used right away.
                        if self.review.as_ref().map(|r| r.look.captions.enabled && r.captions.is_empty()).unwrap_or(false) {
                            self.start_transcription();
                        }
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Msg::UploadProgress(p) => {
                if let Some(r) = self.review.as_mut() {
                    r.step = Step::Uploading(p);
                }
            }
            Msg::UploadDone(Ok(link)) => {
                self.ctx.copy_text(link.clone());
                self.ctx.open_url(egui::OpenUrl::new_tab(link.clone()));
                if let Some(r) = self.review.as_mut() {
                    let _ = std::fs::remove_file(&r.path);
                    if let Some(e) = &r.exported {
                        let _ = std::fs::remove_file(e);
                    }
                    r.step = Step::Done { link: Some(link), saved: None };
                }
            }
            Msg::UploadDone(Err(e)) => {
                self.error = Some(e);
                if let Some(r) = self.review.as_mut() {
                    r.step = Step::Idle;
                }
            }
        }
    }

    fn start_transcription(&mut self) {
        let Some(model) = self.model_path() else {
            self.error = Some("Unduh model Whisper dulu (Pengaturan subtitle).".into());
            return;
        };
        let lang = (self.cfg.language != "auto").then(|| self.cfg.language.clone());
        if let Some(r) = self.review.as_mut() {
            r.transcribing = Some(("Menyiapkan…".into(), 0));
            jobs::transcribe(self.n.clone(), r.path.clone(), model, lang);
        }
    }

    fn start_export(&mut self, after: After) {
        let Some(r) = self.review.as_mut() else { return };
        self.cfg.look = r.look.clone();
        self.paths.save(&self.cfg);
        if !r.needs_render() {
            let p = r.path.clone();
            return match after {
                After::Upload => self.start_upload(p),
                After::Save(dest) => self.finish_save(p, dest),
            };
        }
        r.after = after;
        r.step = Step::Exporting(0);
        let out = r.path.with_file_name(format!("{}-final.mp4", r.path.file_stem().unwrap_or_default().to_string_lossy()));
        jobs::export(self.n.clone(), r.path.clone(), r.events.clone(), r.captions.clone(), r.effective(), r.trim_ms(), out);
    }

    fn start_upload(&mut self, path: PathBuf) {
        let base = self.server_url();
        if let Some(r) = self.review.as_mut() {
            r.step = Step::Uploading(0);
            jobs::upload(self.rt.handle(), self.n.clone(), base, path, r.title.clone(), r.out_duration_ms());
        }
    }

    fn finish_save(&mut self, from: PathBuf, dest: PathBuf) {
        let res = std::fs::copy(&from, &dest);
        let Some(r) = self.review.as_mut() else { return };
        match res {
            Ok(_) => {
                r.step = Step::Done { link: None, saved: Some(dest) };
            }
            Err(e) => {
                r.step = Step::Idle;
                self.error = Some(format!("Tidak bisa menyimpan: {e}"));
            }
        }
    }

    fn discard(&mut self) {
        if let Some(r) = self.review.take() {
            let _ = std::fs::remove_file(&r.path);
            if let Some(e) = r.exported {
                let _ = std::fs::remove_file(e);
            }
        }
        self.phase = Phase::Home;
        self.set_panel(PANEL);
    }

    fn kick_preview(&mut self) {
        let Some(r) = self.review.as_mut() else { return };
        let eff = r.effective();
        let sig = format!("{}|{}|{}", serde_json::to_string(&eff).unwrap_or_default(), serde_json::to_string(&r.captions).unwrap_or_default(), (r.t * 10.0) as i64);
        if sig != r.sig {
            r.sig = sig;
            r.dirty = true;
        }
        if r.dirty && !r.busy {
            r.dirty = false;
            r.busy = true;
            r.seq += 1;
            jobs::preview(self.n.clone(), r.seq, r.path.clone(), r.events.clone(), r.captions.clone(), eff, (r.t * 1000.0) as u64);
        }
    }

    // ------------------------------------------------------------ screens

    fn home_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new("Reka").size(30.0).strong().color(ACCENT));
        ui.label(RichText::new("Rekam layar, percantik, bagikan.").weak());
        ui.add_space(6.0);
        if let Some(w) = &self.devices.warning {
            ui.colored_label(Color32::from_rgb(0xff, 0xb7, 0x4d), w);
        }
        if let Some(e) = self.error.clone() {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(Color32::from_rgb(0xff, 0x6b, 0x6b), e);
                if ui.small_button("tutup").clicked() {
                    self.error = None;
                }
            });
        }

        let screens: Vec<String> = self.devices.screens.iter().map(|s| s.label.clone()).collect();
        let mics: Vec<String> = std::iter::once("Tanpa audio".to_string()).chain(self.devices.mics.iter().map(|m| m.label.clone())).collect();
        let cams: Vec<String> = std::iter::once("Tanpa kamera".to_string()).chain(self.devices.cameras.iter().map(|m| m.label.clone())).collect();
        if !self.devices_loaded {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Mencari perangkat…");
            });
        }
        combo(ui, "Layar", "screen", &screens, &mut self.sel_screen);
        combo(ui, "Mikrofon", "mic", &mics, &mut self.sel_mic);
        combo(ui, "Kamera", "cam", &cams, &mut self.sel_cam);
        ui.add_space(4.0);
        ui.checkbox(&mut self.cfg.studio, "Mode Studio — zoom otomatis, kursor halus, background");
        ui.checkbox(&mut self.cfg.auto_captions, "Subtitle otomatis setelah merekam");
        ui.add_space(8.0);

        let rec = egui::Button::new(RichText::new("Mulai merekam   (Alt+Shift+L)").size(17.0).strong().color(Color32::WHITE))
            .fill(RED)
            .min_size(Vec2::new(ui.available_width(), 46.0));
        if ui.add_enabled(self.devices_loaded && !self.devices.screens.is_empty(), rec).clicked() {
            self.begin_countdown();
        }
        ui.label(RichText::new("Saat merekam: Alt+Shift+L berhenti · Alt+Shift+P jeda · Alt+Shift+C batal").small().weak());

        ui.add_space(10.0);
        ui.separator();
        let url = self.server_url();
        ui.horizontal(|ui| {
            if ui.button("Buka library").clicked() {
                self.ctx.open_url(egui::OpenUrl::new_tab(url));
            }
            if ui.button("Folder rekaman").clicked() {
                self.ctx.open_url(egui::OpenUrl::new_tab(format!("file://{}", self.paths.recordings.display())));
            }
        });
        ui.add_space(4.0);
        egui::CollapsingHeader::new("Pengaturan").default_open(self.show_settings).show(ui, |ui| {
            let mut custom = self.cfg.server_url.clone().unwrap_or_default();
            ui.label("Server untuk berbagi link (kosong = server lokal di komputer ini)");
            if ui.add(egui::TextEdit::singleline(&mut custom).hint_text("https://reka.contoh.com").desired_width(f32::INFINITY)).changed() {
                self.cfg.server_url = (!custom.trim().is_empty()).then(|| custom.trim().to_string());
            }
            ui.add_space(6.0);
            self.whisper_settings_ui(ui);
        });
        let _ = ui.input(|i| i.viewport().focused);
    }

    /// Model + language pickers, shared by the home panel and the editor.
    fn whisper_settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Subtitle otomatis (Whisper, jalan di komputer ini)").strong());
        if !reka_studio::transcribe::AVAILABLE {
            ui.colored_label(
                Color32::from_rgb(0xff, 0xb7, 0x4d),
                "Build ini tanpa Whisper. Bangun dengan --features whisper untuk mengaktifkan subtitle otomatis. Subtitle manual / impor .srt tetap bisa.",
            );
        }
        let cur = config::MODELS.iter().find(|(k, _)| *k == self.cfg.whisper_model).map(|(_, l)| l.to_string()).unwrap_or_else(|| "file sendiri…".into());
        egui::ComboBox::from_label("Model").selected_text(cur).width(330.0).show_ui(ui, |ui| {
            for (k, l) in config::MODELS {
                ui.selectable_value(&mut self.cfg.whisper_model, k.to_string(), *l);
            }
            ui.selectable_value(&mut self.cfg.whisper_model, "custom".into(), "file sendiri (.bin)…");
        });
        if self.cfg.whisper_model == "custom" {
            ui.horizontal(|ui| {
                ui.label(self.cfg.whisper_custom.clone().unwrap_or_else(|| "belum dipilih".into()));
                if ui.button("Pilih…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("Model Whisper", &["bin"]).pick_file() {
                        self.cfg.whisper_custom = Some(p.to_string_lossy().into_owned());
                    }
                }
            });
        } else if self.model_path().is_none() {
            if let Some((d, t)) = self.model_dl {
                let frac = if t > 0 { d as f32 / t as f32 } else { 0.0 };
                ui.add(egui::ProgressBar::new(frac).show_percentage().text(format!("{} / {} MB", d / 1_000_000, t / 1_000_000)));
            } else if ui.button("Unduh model").clicked() {
                let name = self.cfg.whisper_model.clone();
                jobs::download_model(self.n.clone(), config::model_url(&name), self.paths.model_file(&name));
                self.model_dl = Some((0, 0));
            }
        } else {
            ui.label(RichText::new("Model siap").color(Color32::from_rgb(0x66, 0xbb, 0x6a)));
        }
        let cur = config::LANGUAGES.iter().find(|(k, _)| *k == self.cfg.language).map(|(_, l)| *l).unwrap_or("?");
        egui::ComboBox::from_label("Bahasa ucapan").selected_text(cur).show_ui(ui, |ui| {
            for (k, l) in config::LANGUAGES {
                ui.selectable_value(&mut self.cfg.language, k.to_string(), *l);
            }
        });
    }

    fn bar_ui(&mut self, ui: &mut egui::Ui) {
        let bg = egui::Frame::NONE.fill(Color32::from_rgb(0x1b, 0x1b, 0x22)).inner_margin(egui::Margin::symmetric(12, 10)).corner_radius(14.0);
        egui::CentralPanel::default().frame(bg).show(ui, |ui| {
            ui.horizontal_centered(|ui| match self.phase {
                Phase::Countdown(t0) => {
                    let left = (COUNTDOWN_SECS - t0.elapsed().as_secs_f32()).ceil().max(1.0);
                    ui.label(RichText::new(format!("Mulai dalam {left}…")).strong().size(16.0));
                    if ui.button("Batal").clicked() {
                        self.abort_to_home();
                    }
                }
                Phase::Starting(_) | Phase::Launching => {
                    ui.spinner();
                    ui.label(RichText::new("Memulai…").strong().size(16.0));
                }
                _ => {
                    let (el, paused) = self.session.as_ref().map(|s| (s.elapsed(), s.is_paused())).unwrap_or_default();
                    let dot = if paused { Color32::GRAY } else { RED };
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 7.0, dot);
                    ui.label(RichText::new(fmt_time(el)).monospace().size(18.0).strong());
                    if ui.button(if paused { "Lanjut" } else { "Jeda" }).clicked() {
                        self.toggle_pause();
                    }
                    if ui.add(egui::Button::new(RichText::new("Berhenti").strong().color(Color32::WHITE)).fill(RED)).clicked() {
                        self.stop_recording();
                    }
                    if ui.button("Batal").clicked() {
                        self.abort_to_home();
                    }
                }
            });
        });
    }

    fn processing_ui(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 3.0);
            ui.spinner();
            ui.label("Menyimpan rekaman…");
        });
    }

    fn overlays(&mut self, ctx: &egui::Context) {
        let m = self.monitor();
        // Countdown number in the middle of the screen; gone before the first frame is recorded.
        if let Phase::Countdown(t0) = self.phase {
            let left = (COUNTDOWN_SECS - t0.elapsed().as_secs_f32()).ceil().max(1.0) as u32;
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("countdown"),
                egui::ViewportBuilder::default()
                    .with_title("Reka")
                    .with_inner_size([220.0, 220.0])
                    .with_position(egui::pos2((m.x - 220.0) / 2.0, (m.y - 220.0) / 2.0))
                    .with_decorations(false)
                    .with_transparent(true)
                    .with_always_on_top()
                    .with_taskbar(false)
                    .with_resizable(false)
                    .with_mouse_passthrough(true),
                |ui, _| {
                    let r = ui.max_rect();
                    ui.painter().circle_filled(r.center(), 100.0, Color32::from_black_alpha(190));
                    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, left.to_string(), egui::FontId::proportional(110.0), Color32::WHITE);
                },
            );
        }

        // Camera bubble: part of the screen, so it ends up in the recording.
        if let (Some(cam), true) = (&self.camera, matches!(self.phase, Phase::Countdown(_) | Phase::Starting(_) | Phase::Launching | Phase::Recording)) {
            {
                let g = cam.shared.lock().unwrap();
                if g.version != self.cam_ver && !g.rgba.is_empty() {
                    self.cam_ver = g.version;
                    let ci = egui::ColorImage::from_rgba_unmultiplied([camera::SIZE, camera::SIZE], &g.rgba);
                    match self.cam_tex.as_mut() {
                        Some(t) => t.set(ci, egui::TextureOptions::LINEAR),
                        None => self.cam_tex = Some(ctx.load_texture("camera", ci, egui::TextureOptions::LINEAR)),
                    }
                }
                if let Some(e) = g.error.clone() {
                    self.error = Some(e);
                }
            }
            let tex = self.cam_tex.clone();
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("bubble"),
                egui::ViewportBuilder::default()
                    .with_title("Reka kamera")
                    .with_inner_size([200.0, 200.0])
                    .with_position(egui::pos2(40.0, m.y - 200.0 - 70.0))
                    .with_decorations(false)
                    .with_transparent(true)
                    .with_always_on_top()
                    .with_taskbar(false)
                    .with_resizable(false),
                |ui, _| {
                    let r = ui.max_rect();
                    let resp = ui.interact(r, ui.scope_id().with("bubble-drag"), egui::Sense::drag());
                    if resp.drag_started() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
                    }
                    ui.painter().circle_filled(r.center(), 100.0, Color32::from_rgb(0x22, 0x22, 0x2b));
                    if let Some(t) = &tex {
                        let img = egui::Image::new((t.id(), Vec2::splat(196.0))).corner_radius(98.0);
                        img.paint_at(ui, egui::Rect::from_center_size(r.center(), Vec2::splat(196.0)));
                    }
                    ui.painter().circle_stroke(r.center(), 98.0, egui::Stroke::new(3.0, Color32::WHITE));
                },
            );
        }
    }

    // ------------------------------------------------------------ review

    fn review_ui(&mut self, ui: &mut egui::Ui) {
        let Some(mut rv) = self.review.take() else { return };
        let busy = !matches!(rv.step, Step::Idle);
        let mut act: Option<UiAct> = None;
        let fonts = self.fonts.clone();
        let have_model = self.model_path().is_some();

        egui::Panel::right("controls").exact_size(400.0).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().slider_width = 170.0;
                ui.add_enabled_ui(!busy, |ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new("Tampilan").strong().size(16.0));
                    if rv.studio {
                        ui.label(RichText::new("Mode Studio: zoom otomatis mengikuti klik.").small().weak());
                    } else {
                        ui.checkbox(&mut rv.styled, "Beri background dan sudut membulat");
                    }
                    let look = &mut rv.look;
                    if rv.studio || rv.styled {
                        ui.horizontal_wrapped(|ui| {
                            for (a, b) in PRESETS {
                                let (rect, resp) = ui.allocate_exact_size(Vec2::new(34.0, 22.0), egui::Sense::click());
                                let (ca, cb) = (hex_color(a), hex_color(b));
                                let half = Vec2::new(rect.width() / 2.0, rect.height());
                                ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, half), 4.0, ca);
                                ui.painter().rect_filled(egui::Rect::from_min_size(rect.min + Vec2::new(half.x, 0.0), half), 4.0, cb);
                                if look.bg_from == *a && look.bg_to == *b {
                                    ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(2.0, Color32::WHITE), egui::StrokeKind::Outside);
                                }
                                if resp.clicked() {
                                    look.bg_from = a.to_string();
                                    look.bg_to = b.to_string();
                                }
                            }
                        });
                        ui.add(egui::Slider::new(&mut look.padding, 0.0..=20.0).text("Padding %"));
                        ui.add(egui::Slider::new(&mut look.radius, 0.0..=48.0).text("Sudut"));
                        ui.add(egui::Slider::new(&mut look.shadow, 0.0..=1.0).text("Bayangan"));
                    }
                    if rv.studio {
                        ui.checkbox(&mut look.zoom, "Zoom otomatis ke klik");
                        if look.zoom {
                            ui.add(egui::Slider::new(&mut look.zoom_level, 1.2..=3.0).text("Level zoom"));
                        }
                        ui.checkbox(&mut look.cursor, "Kursor halus");
                        if look.cursor {
                            ui.add(egui::Slider::new(&mut look.cursor_scale, 0.8..=3.0).text("Ukuran kursor"));
                        }
                        ui.checkbox(&mut look.click_fx, "Efek klik");
                    }

                    ui.add_space(10.0);
                    ui.separator();
                    ui.label(RichText::new("Subtitle").strong().size(16.0));
                    ui.checkbox(&mut look.captions.enabled, "Tampilkan subtitle di video");

                    ui.horizontal_wrapped(|ui| {
                        let can = reka_studio::transcribe::AVAILABLE && have_model && rv.transcribing.is_none();
                        if ui.add_enabled(can, egui::Button::new("Buat subtitle otomatis")).clicked() {
                            act = Some(UiAct::Transcribe);
                        }
                        if ui.button("Impor .srt").clicked() {
                            act = Some(UiAct::ImportSrt);
                        }
                        if ui.add_enabled(!rv.captions.is_empty(), egui::Button::new("Ekspor .srt")).clicked() {
                            act = Some(UiAct::ExportSrt);
                        }
                    });
                    if !reka_studio::transcribe::AVAILABLE {
                        ui.label(RichText::new("Build ini tanpa Whisper: pakai Impor .srt atau tambah baris manual.").small().weak());
                    } else if !have_model {
                        ui.label(RichText::new("Model Whisper belum diunduh — atur di bawah.").small().color(Color32::from_rgb(0xff, 0xb7, 0x4d)));
                    }
                    if let Some((stage, p)) = &rv.transcribing {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("{stage} {}", if *p > 0 { format!("{p}%") } else { String::new() }));
                        });
                    }

                    // Caption list
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("Baris subtitle ({})", rv.captions.len())).strong());
                        if ui.button("+ di posisi ini").clicked() {
                            let t = (rv.t * 1000.0) as u32;
                            let at = rv.captions.iter().position(|c| c.start > t).unwrap_or(rv.captions.len());
                            rv.captions.insert(at, Caption { start: t, end: t + 2000, text: "Teks baru".into() });
                        }
                    });
                    let mut del = None;
                    egui::ScrollArea::vertical().id_salt("caps").max_height(240.0).show(ui, |ui| {
                        for (i, c) in rv.captions.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                let (mut s, mut e) = (c.start as f64 / 1000.0, c.end as f64 / 1000.0);
                                if ui.add(egui::DragValue::new(&mut s).speed(0.05).fixed_decimals(1).suffix("s")).changed() {
                                    c.start = (s.max(0.0) * 1000.0) as u32;
                                }
                                if ui.add(egui::DragValue::new(&mut e).speed(0.05).fixed_decimals(1).suffix("s")).changed() {
                                    c.end = (e.max(0.0) * 1000.0) as u32;
                                }
                                ui.add(egui::TextEdit::singleline(&mut c.text).desired_width(150.0));
                                if ui.small_button("Ke").on_hover_text("Lompat ke baris ini").clicked() {
                                    act = Some(UiAct::Seek(c.start as f32 / 1000.0 + 0.1));
                                }
                                if ui.small_button("Hapus").clicked() {
                                    del = Some(i);
                                }
                            });
                        }
                    });
                    if let Some(i) = del {
                        rv.captions.remove(i);
                    }
                    // Style
                    egui::CollapsingHeader::new("Gaya subtitle (font, warna, posisi)").show(ui, |ui| {
                        let cs = &mut look.captions;
                        ui.add_space(4.0);
                        ui.label("Font");
                        let cur = cs.font_path.as_ref().map(|p| std::path::Path::new(p).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()).unwrap_or_else(|| "Bawaan (Liberation Sans Bold)".into());
                        egui::ComboBox::from_id_salt("font").selected_text(cur).width(340.0).height(320.0).show_ui(ui, |ui| {
                            if ui.selectable_label(cs.font_path.is_none(), "Bawaan (Liberation Sans Bold)").clicked() {
                                cs.font_path = None;
                            }
                            for (label, path) in &fonts {
                                let p = path.to_string_lossy().into_owned();
                                if ui.selectable_label(cs.font_path.as_deref() == Some(p.as_str()), label).clicked() {
                                    cs.font_path = Some(p);
                                }
                            }
                        });
                        if ui.button("Pilih file font (.ttf / .otf)…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().add_filter("Font", &["ttf", "otf"]).pick_file() {
                                cs.font_path = Some(p.to_string_lossy().into_owned());
                            }
                        }
                        ui.add(egui::Slider::new(&mut cs.size, 2.0..=10.0).text("Ukuran"));
                        ui.horizontal(|ui| {
                            hex_edit(ui, &mut cs.color, "Warna");
                            hex_edit(ui, &mut cs.outline_color, "Garis");
                        });
                        ui.add(egui::Slider::new(&mut cs.outline, 0.0..=8.0).text("Tebal garis"));
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut cs.box_on, "Kotak latar");
                            if cs.box_on {
                                hex_edit(ui, &mut cs.box_color, "");
                            }
                            ui.checkbox(&mut cs.uppercase, "HURUF BESAR");
                        });
                        if cs.box_on {
                            ui.add(egui::Slider::new(&mut cs.box_opacity, 0.1..=1.0).text("Opasitas kotak"));
                        }
                        ui.horizontal(|ui| {
                            ui.label("Posisi");
                            ui.selectable_value(&mut cs.position, Position::Top, "Atas");
                            ui.selectable_value(&mut cs.position, Position::Center, "Tengah");
                            ui.selectable_value(&mut cs.position, Position::Bottom, "Bawah");
                        });
                        ui.add(egui::Slider::new(&mut cs.margin, 0.0..=25.0).text("Jarak dari tepi %"));
                        ui.add(egui::Slider::new(&mut cs.max_width, 30.0..=100.0).text("Lebar maks %"));
                    });
                });

                ui.add_space(8.0);
                egui::CollapsingHeader::new("Pengaturan Whisper").show(ui, |ui| self.whisper_settings_ui(ui));
            });
        });

        egui::Panel::bottom("actions").show(ui, |ui| {
            ui.add_space(6.0);
            match &rv.step {
                Step::Idle => {
                    ui.horizontal(|ui| {
                        ui.label("Judul");
                        ui.add(egui::TextEdit::singleline(&mut rv.title).desired_width((ui.available_width() - 10.0).max(120.0)));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Folder simpan");
                        let cur = self.cfg.save_dir.clone().unwrap_or_else(|| "(tanya setiap kali)".into());
                        ui.label(RichText::new(cur).weak());
                        if ui.button("Pilih folder…").clicked() {
                            act = Some(UiAct::PickDir);
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui.add(egui::Button::new(RichText::new("Ekspor & unggah").strong().color(Color32::WHITE)).fill(ACCENT)).clicked() {
                            act = Some(UiAct::Upload);
                        }
                        if ui.button("Simpan MP4").clicked() {
                            act = Some(UiAct::Save);
                        }
                        if ui.button("Simpan sebagai…").clicked() {
                            act = Some(UiAct::SaveAs);
                        }
                        if ui.button("Buang").clicked() {
                            act = Some(UiAct::Discard);
                        }
                    });
                }
                Step::Exporting(p) => {
                    ui.add(egui::ProgressBar::new(*p as f32 / 100.0).show_percentage().text("Merender video…"));
                }
                Step::Uploading(p) => {
                    ui.add(egui::ProgressBar::new(*p as f32 / 100.0).show_percentage().text("Mengunggah…"));
                }
                Step::Done { link, saved } => {
                    if let Some(l) = link {
                        ui.label(RichText::new("Terunggah").color(Color32::from_rgb(0x66, 0xbb, 0x6a)));
                        ui.horizontal(|ui| {
                            // Selectable (Ctrl+A / Ctrl+C works too) and always followed by its own copy button.
                            let mut shown = l.clone();
                            ui.add(egui::TextEdit::singleline(&mut shown).desired_width((ui.available_width() - 90.0).max(120.0)));
                            if ui.button(if rv.copied { "Tersalin ✓" } else { "Salin" }).clicked() {
                                ui.ctx().copy_text(l.clone());
                                rv.copied = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui.button("Buka di browser").clicked() {
                                ui.ctx().open_url(egui::OpenUrl::new_tab(l.clone()));
                            }
                            if ui.button("Rekaman baru").clicked() {
                                act = Some(UiAct::Done);
                            }
                        });
                    } else {
                        if let Some(s) = saved {
                            ui.label(RichText::new(format!("Tersimpan: {}", s.display())).color(Color32::from_rgb(0x66, 0xbb, 0x6a)));
                            ui.horizontal(|ui| {
                                if ui.button("Buka folder").clicked() {
                                    if let Some(dir) = s.parent() {
                                        ui.ctx().open_url(egui::OpenUrl::new_tab(format!("file://{}", dir.display())));
                                    }
                                }
                                if ui.button("Rekaman baru").clicked() {
                                    act = Some(UiAct::Done);
                                }
                            });
                        }
                    }
                }
            }
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(e) = self.error.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(Color32::from_rgb(0xff, 0x6b, 0x6b), e);
                    if ui.small_button("tutup").clicked() {
                        self.error = None;
                    }
                });
            }
            let avail = ui.available_size();
            let slider_h = 34.0;
            if let Some(tex) = &rv.tex {
                let ts = tex.size_vec2();
                let k = ((avail.x) / ts.x).min((avail.y - slider_h) / ts.y).max(0.1);
                ui.vertical_centered(|ui| {
                    ui.image((tex.id(), ts * k));
                });
            } else {
                ui.centered_and_justified(|ui| ui.spinner());
            }
            let dur = (rv.duration_ms as f32 / 1000.0).max(0.1);
            ui.spacing_mut().slider_width = (avail.x - 110.0).max(100.0);
            ui.add_enabled(!busy, egui::Slider::new(&mut rv.t, 0.0..=dur - 0.05).show_value(true).suffix(" s").fixed_decimals(1));
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Potong:");
                    if ui.button("Mulai di sini").clicked() {
                        rv.trim_a = rv.t.min(rv.trim_b - 0.2).max(0.0);
                    }
                    if ui.button("Akhir di sini").clicked() {
                        rv.trim_b = rv.t.max(rv.trim_a + 0.2).min(dur);
                    }
                    ui.add(egui::DragValue::new(&mut rv.trim_a).range(0.0..=dur).speed(0.05).fixed_decimals(1).suffix(" s"));
                    ui.label("→");
                    ui.add(egui::DragValue::new(&mut rv.trim_b).range(0.0..=dur).speed(0.05).fixed_decimals(1).suffix(" s"));
                    if rv.trimmed() && ui.button("Reset").clicked() {
                        rv.trim_a = 0.0;
                        rv.trim_b = dur;
                    }
                });
                if rv.trimmed() {
                    ui.weak(format!("Hasil: {:.1} s dari {:.1} s", (rv.trim_b - rv.trim_a).max(0.0), dur));
                }
            });
        });

        self.review = Some(rv);
        match act {
            Some(UiAct::Upload) => self.start_export(After::Upload),
            Some(UiAct::Save) | Some(UiAct::SaveAs) => {
                let ask = matches!(act, Some(UiAct::SaveAs));
                let name = self.review.as_ref().map(|r| format!("{}.mp4", r.title.replace(['/', '\\', ':'], "-"))).unwrap_or_default();
                let dir = self.cfg.save_dir.clone().map(PathBuf::from).filter(|d| d.is_dir());
                let dest = match (&dir, ask) {
                    (Some(d), false) => {
                        let mut p = d.join(&name);
                        let stem = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
                        let mut k = 2;
                        while p.exists() {
                            p = d.join(format!("{stem} ({k}).mp4"));
                            k += 1;
                        }
                        Some(p)
                    }
                    _ => {
                        let mut dlg = rfd::FileDialog::new().set_file_name(name).add_filter("MP4", &["mp4"]);
                        if let Some(d) = &dir {
                            dlg = dlg.set_directory(d);
                        }
                        dlg.save_file()
                    }
                };
                if let Some(dest) = dest {
                    if let Some(d) = dest.parent() {
                        self.cfg.save_dir = Some(d.to_string_lossy().to_string());
                        self.paths.save(&self.cfg);
                    }
                    self.start_export(After::Save(dest));
                }
            }
            Some(UiAct::PickDir) => {
                let mut dlg = rfd::FileDialog::new().set_title("Folder tujuan simpan video");
                if let Some(d) = self.cfg.save_dir.clone().map(PathBuf::from).filter(|d| d.is_dir()) {
                    dlg = dlg.set_directory(d);
                }
                if let Some(d) = dlg.pick_folder() {
                    self.cfg.save_dir = Some(d.to_string_lossy().to_string());
                    self.paths.save(&self.cfg);
                }
            }
            Some(UiAct::Discard) => self.discard(),
            Some(UiAct::Done) => {
                self.review = None;
                self.phase = Phase::Home;
                self.set_panel(PANEL);
            }
            Some(UiAct::Transcribe) => self.start_transcription(),
            Some(UiAct::Seek(t)) => {
                if let Some(r) = self.review.as_mut() {
                    r.t = t;
                }
            }
            Some(UiAct::ImportSrt) => {
                if let Some(p) = rfd::FileDialog::new().add_filter("Subtitle", &["srt"]).pick_file() {
                    match std::fs::read_to_string(&p) {
                        Ok(s) => {
                            let caps = reka_studio::captions::parse_srt(&s);
                            if caps.is_empty() {
                                self.error = Some("File .srt tidak berisi subtitle yang valid.".into());
                            } else if let Some(r) = self.review.as_mut() {
                                r.captions = caps;
                                r.look.captions.enabled = true;
                            }
                        }
                        Err(e) => self.error = Some(format!("Tidak bisa membaca file: {e}")),
                    }
                }
            }
            Some(UiAct::ExportSrt) => {
                if let Some(r) = self.review.as_ref() {
                    if let Some(p) = rfd::FileDialog::new().set_file_name("subtitle.srt").add_filter("Subtitle", &["srt"]).save_file() {
                        if let Err(e) = std::fs::write(&p, reka_studio::captions::to_srt(&r.captions)) {
                            self.error = Some(format!("Tidak bisa menyimpan: {e}"));
                        }
                    }
                }
            }
            None => {}
        }
        self.kick_preview();
    }
}

enum UiAct {
    Upload,
    Save,
    SaveAs,
    PickDir,
    Discard,
    Done,
    Transcribe,
    ImportSrt,
    ExportSrt,
    Seek(f32),
}

fn combo(ui: &mut egui::Ui, label: &str, id: &str, items: &[String], sel: &mut usize) {
    ui.label(label);
    let cur = items.get(*sel).cloned().unwrap_or_else(|| "—".into());
    egui::ComboBox::from_id_salt(id).selected_text(cur).width(ui.available_width() - 8.0).show_ui(ui, |ui| {
        for (i, it) in items.iter().enumerate() {
            ui.selectable_value(sel, i, it);
        }
    });
}

fn hex_color(h: &str) -> Color32 {
    let [r, g, b] = reka_studio::parse_hex(h);
    Color32::from_rgb(r, g, b)
}

fn hex_edit(ui: &mut egui::Ui, hex: &mut String, label: &str) {
    let mut rgb = reka_studio::parse_hex(hex);
    if !label.is_empty() {
        ui.label(label);
    }
    if egui::color_picker::color_edit_button_srgb(ui, &mut rgb).changed() {
        *hex = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(m) = self.rx.try_recv() {
            self.handle(m);
        }
        while let Ok(h) = self.hot_rx.try_recv() {
            match (h, self.phase) {
                (Hot::Toggle, Phase::Home) if self.devices_loaded => self.begin_countdown(),
                (Hot::Toggle, Phase::Recording) => self.stop_recording(),
                (Hot::Toggle | Hot::Cancel, Phase::Countdown(_) | Phase::Starting(_) | Phase::Launching) | (Hot::Cancel, Phase::Recording) => self.abort_to_home(),
                (Hot::Pause, Phase::Recording) => self.toggle_pause(),
                _ => {}
            }
        }
        match self.phase {
            Phase::Countdown(t0) if t0.elapsed().as_secs_f32() >= COUNTDOWN_SECS => self.phase = Phase::Starting(Instant::now()),
            Phase::Starting(t0) if t0.elapsed() >= Duration::from_millis(350) => self.begin_capture(),
            _ => {}
        }
        match self.phase {
            Phase::Countdown(_) | Phase::Starting(_) | Phase::Launching | Phase::Recording => ctx.request_repaint_after(Duration::from_millis(100)),
            Phase::Review if self.review.as_ref().map(|r| r.busy || r.transcribing.is_some()).unwrap_or(false) => {
                ctx.request_repaint_after(Duration::from_millis(100))
            }
            _ => {}
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.overlays(&ctx);
        match self.phase {
            Phase::Home => {
                egui::CentralPanel::default().show(ui, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.home_ui(ui));
                });
            }
            Phase::Countdown(_) | Phase::Starting(_) | Phase::Launching | Phase::Recording => self.bar_ui(ui),
            Phase::Processing => {
                egui::CentralPanel::default().show(ui, |ui| self.processing_ui(ui));
            }
            Phase::Review => self.review_ui(ui),
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_cfg();
        if let Some(s) = self.session.take() {
            s.cancel();
        }
        if self.studio_screen.take().is_some() {
            input::end();
        }
    }
}
