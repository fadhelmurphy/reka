//! Webcam preview for the floating bubble: ffmpeg decodes the camera into small square RGBA
//! frames, the UI shows the latest one. The bubble sits on screen, so the screen recording
//! picks it up without any extra compositing.

use crate::recorder::ffmpeg_cmd;
use std::io::Read;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};

pub const SIZE: usize = 240;

#[derive(Default)]
pub struct Shared {
    pub rgba: Vec<u8>,
    pub version: u64,
    pub error: Option<String>,
}

pub struct Camera {
    pub shared: Arc<Mutex<Shared>>,
    child: Arc<Mutex<Option<Child>>>,
}

fn input_args(id: &str) -> Vec<String> {
    let s = |v: &str| v.to_string();
    if std::env::var_os("REKA_FAKE_CAMERA").is_some() {
        return vec![s("-f"), s("lavfi"), s("-i"), s("testsrc2=s=640x480:r=30")];
    }
    if cfg!(target_os = "windows") {
        vec![s("-f"), s("dshow"), s("-framerate"), s("30"), s("-i"), format!("video={id}")]
    } else if cfg!(target_os = "macos") {
        vec![s("-f"), s("avfoundation"), s("-framerate"), s("30"), s("-i"), id.to_string()]
    } else {
        vec![s("-f"), s("v4l2"), s("-framerate"), s("30"), s("-video_size"), s("640x480"), s("-i"), id.to_string()]
    }
}

impl Camera {
    /// `on_frame` is called after every new frame so the UI can repaint.
    pub fn start(device: &str, on_frame: impl Fn() + Send + 'static) -> Result<Camera, String> {
        let mut c = ffmpeg_cmd();
        c.args(["-hide_banner", "-loglevel", "error"]).args(input_args(device)).args([
            "-vf",
            &format!("crop='min(iw,ih)':'min(iw,ih)',scale={SIZE}:{SIZE},hflip"),
            "-pix_fmt",
            "rgba",
            "-f",
            "rawvideo",
            "-",
        ]);
        let mut child = c
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("kamera tidak bisa dibuka: {e}"))?;
        let mut out = child.stdout.take().ok_or("no stdout")?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let sh = shared.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; SIZE * SIZE * 4];
            loop {
                if out.read_exact(&mut buf).is_err() {
                    let mut g = sh.lock().unwrap();
                    if g.version == 0 {
                        g.error = Some("kamera tidak memberi gambar (dipakai aplikasi lain?)".into());
                    }
                    break;
                }
                {
                    let mut g = sh.lock().unwrap();
                    g.rgba.clear();
                    g.rgba.extend_from_slice(&buf);
                    g.version += 1;
                }
                on_frame();
            }
            on_frame();
        });
        Ok(Camera { shared, child: Arc::new(Mutex::new(Some(child))) })
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
