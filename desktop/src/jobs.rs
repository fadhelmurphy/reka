//! Work that must not block the UI thread. Every job reports back through a [`Msg`].

use crate::input;
use crate::recorder::{ffmpeg_bin, Screen, Session, Span};
use reka_studio::{Caption, InputEvent, Settings};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

pub struct Stopped {
    pub path: PathBuf,
    pub duration_ms: u64,
    /// Mouse log for Studio recordings.
    pub events: Option<Vec<InputEvent>>,
}

pub enum Msg {
    Devices(crate::devices::Devices),
    Started(Result<Session, String>),
    Stopped(Result<Stopped, String>),
    Preview { seq: u64, img: Result<(u32, u32, Vec<u8>), String> },
    ExportProgress(u32),
    ExportDone(Result<PathBuf, String>),
    Stage(String),
    TranscribeProgress(i32),
    TranscribeDone(Result<Vec<Caption>, String>),
    ModelProgress(u64, u64),
    ModelDone(Result<PathBuf, String>),
    UploadProgress(u32),
    UploadDone(Result<String, String>),
}

/// Sends a message and wakes the UI.
#[derive(Clone)]
pub struct Notifier {
    pub tx: Sender<Msg>,
    pub ctx: eframe::egui::Context,
}

impl Notifier {
    pub fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
        self.ctx.request_repaint();
    }
}

pub fn list_devices(n: Notifier) {
    std::thread::spawn(move || n.send(Msg::Devices(crate::devices::list())));
}

/// Stop ffmpeg, join the segments and turn the raw mouse log into video-timeline events.
pub fn finish_recording(n: Notifier, session: Session, out: PathBuf, studio: Option<(Screen, Vec<input::Raw>)>) {
    std::thread::spawn(move || {
        let res = session.finish(&out).map(|(path, dur, spans): (PathBuf, Duration, Vec<Span>)| Stopped {
            events: studio.map(|(screen, raw)| input::map(&raw, &spans, &screen)),
            path,
            duration_ms: dur.as_millis() as u64,
        });
        n.send(Msg::Stopped(res));
    });
}

pub fn preview(n: Notifier, seq: u64, video: PathBuf, events: Arc<Vec<InputEvent>>, caps: Vec<Caption>, s: Settings, t_ms: u64) {
    std::thread::spawn(move || {
        let img = reka_studio::preview_pixmap(&ffmpeg_bin(), &video, &events, &caps, &s, t_ms, 1000)
            .map(|pm| (pm.width(), pm.height(), pm.data().to_vec()));
        n.send(Msg::Preview { seq, img });
    });
}

pub fn export(n: Notifier, video: PathBuf, events: Arc<Vec<InputEvent>>, caps: Vec<Caption>, s: Settings, trim: Option<(u64, u64)>, out: PathBuf) {
    std::thread::spawn(move || {
        let n2 = n.clone();
        let res = reka_studio::export(&ffmpeg_bin(), &video, &events, &caps, &s, trim, &out, move |p| n2.send(Msg::ExportProgress(p)));
        n.send(Msg::ExportDone(res.map(|_| out)));
    });
}

pub fn transcribe(n: Notifier, video: PathBuf, model: PathBuf, lang: Option<String>) {
    std::thread::spawn(move || {
        n.send(Msg::Stage("Mengambil audio…".into()));
        let audio = match reka_studio::transcribe::extract_audio(&ffmpeg_bin(), &video) {
            Ok(a) => a,
            Err(e) => return n.send(Msg::TranscribeDone(Err(e))),
        };
        n.send(Msg::Stage("Mengenali suara…".into()));
        let n2 = n.clone();
        let res = reka_studio::transcribe::transcribe(&model, &audio, lang.as_deref(), move |p| n2.send(Msg::TranscribeProgress(p)));
        n.send(Msg::TranscribeDone(res));
    });
}

/// Download a Whisper model to `dest` (through a `.part` file so a broken download is never used).
pub fn download_model(n: Notifier, url: String, dest: PathBuf) {
    std::thread::spawn(move || {
        let res = (|| -> Result<PathBuf, String> {
            use std::io::{Read, Write};
            let resp = reqwest::blocking::Client::builder()
                .timeout(None)
                .build()
                .and_then(|c| c.get(&url).send())
                .map_err(|e| format!("unduhan gagal: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("server model menjawab {}", resp.status()));
            }
            let total = resp.content_length().unwrap_or(0);
            let part = dest.with_extension("part");
            let mut f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
            let (mut done, mut last) = (0u64, 0u64);
            let mut r = resp;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let k = r.read(&mut buf).map_err(|e| format!("unduhan terputus: {e}"))?;
                if k == 0 {
                    break;
                }
                f.write_all(&buf[..k]).map_err(|e| e.to_string())?;
                done += k as u64;
                if done - last > 512 * 1024 {
                    last = done;
                    n.send(Msg::ModelProgress(done, total));
                }
            }
            drop(f);
            if total > 0 && done != total {
                let _ = std::fs::remove_file(&part);
                return Err("unduhan tidak lengkap".into());
            }
            std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
            Ok(dest.clone())
        })();
        n.send(Msg::ModelDone(res));
    });
}

/// Upload with progress, resolving to the share link.
pub fn upload(rt: &tokio::runtime::Handle, n: Notifier, base: String, path: PathBuf, title: String, duration_ms: u64) {
    rt.spawn(async move {
        let res = upload_inner(&n, &base, &path, title, duration_ms).await;
        n.send(Msg::UploadDone(res));
    });
}

async fn upload_inner(n: &Notifier, base: &str, path: &Path, title: String, duration_ms: u64) -> Result<String, String> {
    use futures_util::StreamExt;
    let file = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
    let total = file.metadata().await.map_err(|e| e.to_string())?.len();
    let (mut sent, mut last_pct) = (0u64, 0u64);
    let n2 = n.clone();
    let stream = tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024).inspect(move |chunk| {
        if let Ok(c) = chunk {
            sent += c.len() as u64;
            let pct = sent * 100 / total.max(1);
            if pct != last_pct {
                last_pct = pct;
                n2.send(Msg::UploadProgress(pct as u32));
            }
        }
    });
    let part = reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(stream), total)
        .file_name("recording.mp4")
        .mime_str("video/mp4")
        .map_err(|e| e.to_string())?;
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
    Ok(format!("{base}{}", v["share_url"].as_str().unwrap_or("")))
}
