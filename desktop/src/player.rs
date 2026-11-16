//! Video playback for the editor: ffmpeg decodes low-res RGBA frames at a fixed rate and the UI
//! shows the one that is due according to the wall clock. No audio.

use crate::recorder::ffmpeg_bin;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc::{sync_channel, Receiver};
use std::time::Instant;

pub const FPS: f32 = 15.0;
const WIDTH: u32 = 640;

pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

pub struct Player {
    rx: Receiver<Frame>,
    child: Child,
    start: Instant,
    pub from: f32,
    n: u64,
    pending: Option<Frame>,
    pub ended: bool,
}

impl Player {
    /// Start decoding `path` from `from` seconds. `size` is the source size (to know the frame layout).
    pub fn start(path: &Path, from: f32, size: (u32, u32), ctx: eframe::egui::Context) -> Result<Player, String> {
        let w = WIDTH.min(size.0) & !1;
        let h = ((size.1 as f32 * w as f32 / size.0.max(1) as f32).round() as u32).max(2) & !1;
        let mut child = reka_studio::command(&ffmpeg_bin())
            .args(["-hide_banner", "-loglevel", "error", "-ss", &format!("{from:.3}"), "-i"])
            .arg(path)
            .args(["-vf", &format!("fps={FPS},scale={w}:{h}"), "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("ffmpeg: {e}"))?;
        let mut out = child.stdout.take().ok_or("tidak ada output ffmpeg")?;
        let (tx, rx) = sync_channel::<Frame>(6);
        std::thread::spawn(move || {
            let len = (w * h * 4) as usize;
            loop {
                let mut buf = vec![0u8; len];
                if out.read_exact(&mut buf).is_err() {
                    break;
                }
                if tx.send(Frame { w: w as usize, h: h as usize, rgba: buf }).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            ctx.request_repaint(); // wake the UI so it notices the end
        });
        Ok(Player { rx, child, start: Instant::now(), from, n: 0, pending: None, ended: false })
    }

    /// Current playback position in seconds.
    pub fn pos(&self) -> f32 {
        self.from + self.start.elapsed().as_secs_f32()
    }

    /// The newest frame that is due now, if any.
    pub fn poll(&mut self) -> Option<Frame> {
        let elapsed = self.start.elapsed().as_secs_f32();
        let mut latest = None;
        loop {
            let f = match self.pending.take() {
                Some(f) => f,
                None => match self.rx.try_recv() {
                    Ok(f) => f,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.ended = true;
                        break;
                    }
                },
            };
            if self.n as f32 / FPS <= elapsed {
                self.n += 1;
                latest = Some(f);
            } else {
                self.pending = Some(f);
                break;
            }
        }
        latest
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
