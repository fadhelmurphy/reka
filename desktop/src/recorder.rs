//! Native screen recording by driving `ffmpeg`.
//!
//! Each OS uses its own capture device:
//! - macOS:   avfoundation (screen + mic in one input)
//! - Windows: gdigrab (screen) + dshow (mic)
//! - Linux:   x11grab (screen) + pulse (mic / system-audio monitor)
//!
//! ffmpeg cannot pause, so every pause closes the current segment and resume starts a
//! new one; `finish` concatenates the segments losslessly.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Screen {
    pub label: String,
    /// avfoundation device index (macOS only).
    pub mac_index: Option<u32>,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecOpts {
    pub screen: Screen,
    /// macOS: avfoundation audio index, Windows: dshow device name, Linux: pulse source.
    pub mic: Option<String>,
    /// Studio mode records without the system cursor; it is re-drawn later from the mouse log.
    #[serde(default)]
    pub hide_cursor: bool,
}

/// A stretch of wall-clock time that ended up in the video (pauses leave gaps between spans).
#[derive(Debug, Clone, Copy)]
pub struct Span {
    pub start: Instant,
    pub end: Instant,
}

/// Offset between spawning ffmpeg and video time 0. Measured on Linux with the
/// `studio_record_roundtrip` test (x11grab stamps frames from when the device is opened, so ≈ 0).
/// Windows (gdigrab) should behave the same; macOS (avfoundation) starts at the first delivered
/// frame, so its value is an estimate that has not been measured.
pub fn capture_latency() -> Duration {
    Duration::from_millis(if cfg!(target_os = "macos") {
        350
    } else if cfg!(windows) {
        30
    } else {
        LINUX_LATENCY_MS
    })
}
const LINUX_LATENCY_MS: u64 = 0;

/// Find ffmpeg: `REKA_FFMPEG`, a copy bundled next to the executable, common Homebrew
/// locations (apps started from Finder do not inherit the shell PATH), then PATH.
pub fn ffmpeg_bin() -> PathBuf {
    if let Ok(p) = std::env::var("REKA_FFMPEG") {
        return p.into();
    }
    let exe_name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for cand in [dir.join(exe_name), dir.join("../Resources").join(exe_name)] {
                if cand.is_file() {
                    return cand;
                }
            }
        }
    }
    for cand in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"] {
        if cfg!(target_os = "macos") && Path::new(cand).is_file() {
            return cand.into();
        }
    }
    exe_name.into()
}

pub fn ffmpeg_cmd() -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(ffmpeg_bin());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flashing up
    }
    c
}

fn s<T: ToString>(v: T) -> String {
    v.to_string()
}

pub fn build_args(o: &RecOpts, out: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "warning", "-y"].map(s).to_vec();
    let cursor = if o.hide_cursor { "0" } else { "1" };
    let has_audio = o.mic.is_some();
    let audio_input: &str;

    #[cfg(target_os = "macos")]
    {
        let idx = o.screen.mac_index.unwrap_or(0);
        let mic = o.mic.as_deref().unwrap_or("none");
        a.extend(["-thread_queue_size", "1024", "-f", "avfoundation", "-capture_cursor", cursor,
                  "-framerate", "30"].map(s));
        a.extend([s("-i"), format!("{idx}:{mic}")]);
        audio_input = "0:a";
    }
    #[cfg(target_os = "windows")]
    {
        a.extend(["-thread_queue_size", "1024", "-f", "gdigrab", "-framerate", "30",
                  "-draw_mouse", cursor].map(s));
        a.extend([s("-offset_x"), s(o.screen.x), s("-offset_y"), s(o.screen.y),
                  s("-video_size"), format!("{}x{}", o.screen.w, o.screen.h), s("-i"), s("desktop")]);
        if let Some(m) = &o.mic {
            a.extend(["-thread_queue_size", "1024", "-f", "dshow"].map(s));
            a.extend([s("-i"), format!("audio={m}")]);
        }
        audio_input = "1:a";
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
        a.extend(["-thread_queue_size", "1024", "-f", "x11grab", "-framerate", "30",
                  "-draw_mouse", cursor].map(s));
        a.extend([s("-video_size"), format!("{}x{}", o.screen.w, o.screen.h),
                  s("-i"), format!("{display}+{},{}", o.screen.x, o.screen.y)]);
        if let Some(m) = &o.mic {
            a.extend(["-thread_queue_size", "1024", "-f", "pulse"].map(s));
            a.extend([s("-i"), m.clone()]);
        }
        audio_input = "1:a";
    }

    a.extend(["-map", "0:v"].map(s));
    if has_audio {
        a.extend([s("-map"), s(audio_input)]);
    }
    // Cap width at 1920 (Retina/4K screens would make x264 struggle in real time) and keep
    // both dimensions even, which yuv420p requires.
    a.extend([
        "-vf", r"scale=w=trunc(min(iw\,1920)/2)*2:h=-2,format=yuv420p",
        "-r", "30", "-c:v", "libx264", "-preset", "veryfast", "-crf", "23",
    ].map(s));
    if has_audio {
        a.extend(["-c:a", "aac", "-b:a", "128k"].map(s));
    }
    a.push(out.to_string_lossy().into_owned());
    a
}

pub struct Session {
    opts: RecOpts,
    dir: PathBuf,
    segments: Vec<PathBuf>,
    spans: Vec<Span>,
    cur_start: Option<Instant>,
    child: Option<Child>,
    seg_started: Option<Instant>,
    elapsed: Duration,
}

impl Session {
    pub fn start(opts: RecOpts, dir: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut s = Session { opts, dir, segments: vec![], spans: vec![], cur_start: None, child: None, seg_started: None, elapsed: Duration::ZERO };
        if let Err(e) = s.spawn_segment() {
            let _ = std::fs::remove_dir_all(&s.dir);
            return Err(e);
        }
        Ok(s)
    }

    fn spawn_segment(&mut self) -> Result<(), String> {
        let out = self.dir.join(format!("seg{:03}.mp4", self.segments.len()));
        let log_path = self.dir.join(format!("seg{:03}.log", self.segments.len()));
        let log = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
        let started = Instant::now(); // ffmpeg captures from here on, so the timer must too
        let mut child = ffmpeg_cmd()
            .args(build_args(&self.opts, &out))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|e| format!("Tidak bisa menjalankan ffmpeg ({}): {e}", ffmpeg_bin().display()))?;

        // Device/permission errors make ffmpeg exit almost immediately; surface them now
        // rather than after the user has talked for five minutes.
        std::thread::sleep(Duration::from_millis(900));
        if let Ok(Some(status)) = child.try_wait() {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(6).collect();
            return Err(format!(
                "ffmpeg berhenti ({status}). {}",
                tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
            ));
        }
        self.segments.push(out);
        self.child = Some(child);
        self.seg_started = Some(started);
        self.cur_start = Some(started + capture_latency());
        Ok(())
    }

    fn close_segment(&mut self) {
        let now = Instant::now(); // stopping can take a while; the video ends at "now"
        if let Some(mut c) = self.child.take() {
            stop_gracefully(&mut c);
        }
        if let Some(t) = self.seg_started.take() {
            self.elapsed += now.saturating_duration_since(t);
        }
        if let Some(st) = self.cur_start.take() {
            self.spans.push(Span { start: st, end: now });
        }
    }

    pub fn is_paused(&self) -> bool {
        self.child.is_none()
    }

    pub fn elapsed(&self) -> Duration {
        self.elapsed + self.seg_started.map(|t| t.elapsed()).unwrap_or_default()
    }

    pub fn pause(&mut self) {
        self.close_segment();
    }

    pub fn resume(&mut self) -> Result<(), String> {
        if self.child.is_some() {
            return Ok(());
        }
        self.spawn_segment()
    }

    /// Stop, stitch segments together and return (final file, duration).
    pub fn finish(mut self, out: &Path) -> Result<(PathBuf, Duration, Vec<Span>), String> {
        self.close_segment();
        let segs: Vec<&PathBuf> = self
            .segments
            .iter()
            .filter(|p| std::fs::metadata(p).map(|m| m.len() > 1024).unwrap_or(false))
            .collect();
        if segs.is_empty() {
            let _ = std::fs::remove_dir_all(&self.dir);
            return Err("Rekaman kosong — ffmpeg tidak menulis video.".into());
        }
        let list = self.dir.join("list.txt");
        let body: String = segs
            .iter()
            .map(|p| format!("file '{}'\n", p.to_string_lossy().replace('\'', r"'\''")))
            .collect();
        std::fs::write(&list, body).map_err(|e| e.to_string())?;
        let st = ffmpeg_cmd()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
            .arg(&list)
            .args(["-c", "copy", "-movflags", "+faststart"])
            .arg(out)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| e.to_string())?;
        if !st.success() {
            return Err(format!("Gagal menggabungkan segmen (lihat {})", self.dir.display()));
        }
        let _ = std::fs::remove_dir_all(&self.dir);
        Ok((out.to_path_buf(), self.elapsed, std::mem::take(&mut self.spans)))
    }

    pub fn cancel(mut self) {
        self.close_segment();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Ask ffmpeg to quit with `q` so it writes the MP4 index; kill if it hangs.
fn stop_gracefully(c: &mut Child) {
    if let Some(stdin) = c.stdin.as_mut() {
        let _ = stdin.write_all(b"q");
        let _ = stdin.flush();
    }
    drop(c.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = c.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = c.kill();
    let _ = c.wait();
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Needs an X server: `xvfb-run cargo test -p reka-desktop -- --ignored`
    #[test]
    #[ignore]
    fn record_pause_resume_finish() {
        let dir = std::env::temp_dir().join("reka-rec-test");
        let _ = std::fs::remove_dir_all(&dir);
        let screen = Screen { label: "t".into(), mac_index: None, x: 0, y: 0, w: 641, h: 481 };
        let mut s = Session::start(RecOpts { screen, mic: None, hide_cursor: false }, dir.join("sess")).expect("start");
        std::thread::sleep(Duration::from_millis(1500));
        s.pause();
        assert!(s.is_paused());
        std::thread::sleep(Duration::from_millis(500)); // paused time must not count
        s.resume().unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let out = dir.join("final.mp4");
        let (p, dur, _) = s.finish(&out).expect("finish");

        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration:stream=width,height,codec_name", "-of", "csv=p=0"])
            .arg(&p)
            .output()
            .unwrap();
        let txt = String::from_utf8_lossy(&probe.stdout);
        println!("{txt}");
        let file_secs: f64 = txt.lines().last().unwrap().trim().parse().unwrap();
        assert!((file_secs - dur.as_secs_f64()).abs() < 0.6, "timer {dur:?} vs file {file_secs}s");
        assert!(txt.contains("h264,640,480"), "odd 641x481 must become even 640x480: {txt}");
        assert!(!dir.join("sess").exists(), "segment dir cleaned up");
    }
}
